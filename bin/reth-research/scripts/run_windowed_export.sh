#!/usr/bin/env bash
# Windowed ClickHouse-export driver for reth-research.
#
# Re-analyzes a large historical block range and exports it to ClickHouse one
# bounded window at a time, deleting each window's SQLite after it has fully
# drained to the warehouse. This keeps peak disk to ~one window even though the
# full range's analysis would not fit (full-fidelity SQLite is several MB/block).
#
# Requires the `--research.backfill-max-block` flag (so a backfill can be bounded
# to an explicit [min, max] window — a fresh per-window SQLite carries no dedup
# state). ClickHouse (ReplacingMergeTree) is the durable store; each window SQLite
# is ephemeral. Resumable: progress is recorded only on full window completion.
#
# Usage (env-configured; CLICKHOUSE_PASSWORD must already be exported):
#   export CLICKHOUSE_PASSWORD=...                 # never passed as an argument
#   START_BLOCK=25319986 END_BLOCK=22719986 \      # inclusive [END, START]
#   WINDOW_SIZE=50000 BACKFILL_CONCURRENCY=44 \
#   SCHEDULE_FLAGS="--research.amsterdam" \
#   nohup ./bin/reth-research/scripts/run_windowed_export.sh >> windowed.log 2>&1 &
#
# Stop reth.service first (single MDBX lock). Never touches the chain archive
# beyond what a normal node does; the only files it deletes are its own window
# SQLites under SQLITE_DIR.

set -euo pipefail

# ─── Configuration (all env-overridable) ──────────────────────────────────────
REPO="${REPO:-/home/ubuntu/reth}"
RUN_SCRIPT="${RUN_SCRIPT:-$REPO/bin/reth-research/scripts/run_xeon_perf.sh}"
ARCHIVE_DATADIR="${ARCHIVE_DATADIR:-/home/ubuntu/.local/share/reth/mainnet}"  # read by the node; never deleted here
EXPORT_CONFIG="${EXPORT_CONFIG:-$REPO/clickhouse-export.toml}"
SQLITE_DIR="${SQLITE_DIR:-$REPO/windows}"
STATE_FILE="${STATE_FILE:-$SQLITE_DIR/.windowed_progress}"
LOG_DIR="${LOG_DIR:-$SQLITE_DIR/logs}"
PID_FILE="${PID_FILE:-$SQLITE_DIR/.window.pid}"

START_BLOCK="${START_BLOCK:?set START_BLOCK (inclusive top of the range)}"
END_BLOCK="${END_BLOCK:?set END_BLOCK (inclusive bottom of the range)}"
WINDOW_SIZE="${WINDOW_SIZE:-50000}"
BACKFILL_CONCURRENCY="${BACKFILL_CONCURRENCY:-44}"
SCHEDULE_FLAGS="${SCHEDULE_FLAGS:---research.amsterdam}"
CSV_FLAGS="${CSV_FLAGS:-}"               # e.g. --research.csv 7904-prelim=/path.csv (adds 1 schedule)
EXTRA_NODE_FLAGS="${EXTRA_NODE_FLAGS:-}" # e.g. --research.max-divergences-per-block 8192 (shared across schedules),
                                         # or --research.tx-gas-results for the per-tx gas spine (off by default)
POLL_SECS="${POLL_SECS:-30}"
DRAIN_STABLE_POLLS="${DRAIN_STABLE_POLLS:-3}"
MB_PER_BLOCK="${MB_PER_BLOCK:-6}"        # disk budget per analyzed block (observed ~5 on heavy recent blocks)
STOP_GRACE_SECS="${STOP_GRACE_SECS:-180}"

# ─── Logging / helpers ────────────────────────────────────────────────────────
log()  { printf '%s [windowed] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }  # stderr: never pollute $(read_resume_point)/$(schedule_count) stdout
halt() { log "HALT: $*"; exit 1; }

# Read-only SQLite query against a (possibly live, WAL-mode) window DB.
sqlite_ro() { sqlite3 -readonly "$1" "$2"; }

# Count schedule-producing flags → one block_coverage row per (schedule, block).
schedule_count() {
    { grep -oE -- '--research\.(amsterdam|csv|multiplier)' <<<"$SCHEDULE_FLAGS $CSV_FLAGS" || true; } | wc -l | tr -d ' '
}

disk_guard() {
    local dir="$1" need free
    need=$(( WINDOW_SIZE * MB_PER_BLOCK * 3 / 2 ))   # 1.5× one window
    free=$(df -Pm "$dir" | awk 'NR==2 {print $4}')
    [ "${free:-0}" -ge "$need" ] || halt "disk: need ~${need} MB free in $dir, have ${free:-0} MB"
}

wait_for_pid_exit() {
    local pid="$1" max="$2" i=0
    while kill -0 "$pid" 2>/dev/null; do
        sleep 1; i=$((i + 1))
        [ "$i" -ge "$max" ] && halt "node PID $pid did not exit within ${max}s (use SIGTERM only; investigate)"
    done
    return 0   # the loop's final `kill -0` fails once the process is gone; don't let that trip `set -e`
}

dump_blocked() {
    log "blocked rows in $1:"
    sqlite_ro "$1" "SELECT export_id, block_number, schedule_name, payload_bytes, substr(last_error,1,200)
                    FROM export_outbox WHERE state='blocked' LIMIT 20;" || true
}

# ─── Preflight ────────────────────────────────────────────────────────────────
preflight() {
    [ -n "${CLICKHOUSE_PASSWORD:-}" ] || halt "CLICKHOUSE_PASSWORD must be exported (never passed as an argument)"
    command -v sqlite3 >/dev/null 2>&1 || halt "sqlite3 is required for drain polling"
    if systemctl is-active --quiet reth.service 2>/dev/null; then
        halt "reth.service is running — stop it first (single MDBX lock): sudo systemctl stop reth.service"
    fi
    [ -f "$EXPORT_CONFIG" ] || halt "export config not found: $EXPORT_CONFIG"
    [ "$START_BLOCK" -ge "$END_BLOCK" ] || halt "START_BLOCK ($START_BLOCK) must be >= END_BLOCK ($END_BLOCK)"
    [ "$WINDOW_SIZE" -ge 1 ] || halt "WINDOW_SIZE must be >= 1"
    mkdir -p "$SQLITE_DIR" "$LOG_DIR"

    # Build the binary ONCE and pin the commit — all windows reuse this exact
    # binary, so analysis_config_hash stays constant across the whole run.
    log "building reth-research-bin (release) once..."
    ( cd "$REPO" && cargo build --release -p reth-research-bin ) || halt "build failed"
    COMMIT=$(cd "$REPO" && git rev-parse HEAD)
    log "pinned commit: $COMMIT"

    # Refuse to run if a node from a previous invocation is still alive.
    if [ -f "$PID_FILE" ]; then
        local p; p=$(cat "$PID_FILE" 2>/dev/null || echo "")
        if [ -n "$p" ] && kill -0 "$p" 2>/dev/null; then
            halt "a node from a previous run (PID $p) still holds the MDBX lock; stop it (kill -TERM $p) and retry"
        fi
        rm -f "$PID_FILE"
    fi
}

# ─── Resume point ─────────────────────────────────────────────────────────────
read_resume_point() {
    if [ -f "$STATE_FILE" ]; then
        local sc sf ws le
        sc=$(awk -F= '/^commit=/{print $2}' "$STATE_FILE")
        sf=$(sed -n 's/^schedule_flags=//p' "$STATE_FILE")
        ws=$(awk -F= '/^window_size=/{print $2}' "$STATE_FILE")
        le=$(awk -F= '/^lowest_exported=/{print $2}' "$STATE_FILE")
        [ "$sc" = "$COMMIT" ]          || halt "state file commit ($sc) != current ($COMMIT) — different binary changes analysis_config_hash. Start a NEW run/state file."
        [ "$sf" = "$SCHEDULE_FLAGS" ]  || halt "state file schedule_flags ($sf) != current ($SCHEDULE_FLAGS) — would change the dataset identity."
        [ "$ws" = "$WINDOW_SIZE" ]     || halt "state file window_size ($ws) != current ($WINDOW_SIZE)."
        log "resuming from state file: lowest_exported=$le"
        echo "$(( le - 1 ))"
    else
        echo "$START_BLOCK"
    fi
}

record_progress() {   # $1 = w_low of the just-completed window (lowest fully exported)
    local tmp="$STATE_FILE.tmp"
    {
        echo "commit=$COMMIT"
        echo "schedule_flags=$SCHEDULE_FLAGS"
        echo "window_size=$WINDOW_SIZE"
        echo "start_block=$START_BLOCK"
        echo "end_block=$END_BLOCK"
        echo "lowest_exported=$1"
    } > "$tmp"
    mv -f "$tmp" "$STATE_FILE"
}

# ─── Window completion: backfill done (A), THEN drained (B) ───────────────────
wait_for_window_complete() {   # $1=sqlite $2=w_low $3=w_high $4=pid $5=log
    local sqlite="$1" w_low="$2" w_high="$3" pid="$4" wlog="$5"
    local expected=$(( (w_high - w_low + 1) * SCHEDULE_COUNT ))
    local stable=0
    while true; do
        sleep "$POLL_SECS"
        kill -0 "$pid" 2>/dev/null || { tail -n 30 "$wlog" || true; halt "node (PID $pid) died during window $w_low-$w_high; see $wlog"; }

        # Condition A — backfill finished producing (log is authoritative; the
        # "Backfill cursor reached lower bound" line is emitted exactly once).
        local producer_done=0 cov
        grep -q "Backfill cursor reached lower bound" "$wlog" 2>/dev/null && producer_done=1
        cov=$(sqlite_ro "$sqlite" "SELECT COUNT(*) FROM block_coverage WHERE block_number BETWEEN $w_low AND $w_high;" 2>/dev/null || echo 0)
        [ "${cov:-0}" -ge "$expected" ] && producer_done=1
        if [ "$producer_done" -ne 1 ]; then
            log "window $w_low-$w_high: analyzing (coverage ${cov:-0}/$expected)"
            continue
        fi

        # Condition B — export fully drained (only checked AFTER A, to avoid the
        # producer-still-writing false-zero race).
        local row pend blk
        row=$(sqlite_ro "$sqlite" "SELECT COUNT(*) FILTER (WHERE state IN ('pending','retry')) || '|' ||
                                          COUNT(*) FILTER (WHERE state='blocked') FROM export_outbox;" 2>/dev/null || echo "1|0")
        pend=${row%%|*}; blk=${row##*|}
        if [ "${blk:-0}" -gt 0 ]; then
            dump_blocked "$sqlite"
            halt "window $w_low-$w_high has ${blk} BLOCKED rows — data-loss risk. SQLite preserved at $sqlite; raise max_single_row_bytes / triage, then re-run."
        fi
        if [ "${pend:-1}" -eq 0 ]; then
            stable=$(( stable + 1 ))
            log "window $w_low-$w_high: drained (stable $stable/$DRAIN_STABLE_POLLS)"
            [ "$stable" -ge "$DRAIN_STABLE_POLLS" ] && return 0
        else
            stable=0
            log "window $w_low-$w_high: draining (${pend} pending)"
        fi
    done
}

# ─── One window ───────────────────────────────────────────────────────────────
run_window() {   # $1=w_low $2=w_high
    local w_low="$1" w_high="$2"
    local sqlite="$SQLITE_DIR/window_${w_low}_${w_high}.sqlite"
    local wlog="$LOG_DIR/window_${w_low}_${w_high}.log"

    # Crash-safe restart: a stale partial SQLite for THIS window is re-done from
    # scratch; ClickHouse ReplacingMergeTree dedupes anything already exported.
    rm -f "$sqlite" "$sqlite-wal" "$sqlite-shm"
    disk_guard "$SQLITE_DIR"

    log "launching window [$w_low, $w_high] → $sqlite"
    # run_xeon_perf.sh applies the engine tuning then `exec`s the node, so $! is
    # the node PID after exec. CLICKHOUSE_PASSWORD is inherited from the env.
    # shellcheck disable=SC2086
    nohup "$RUN_SCRIPT" \
        $SCHEDULE_FLAGS $CSV_FLAGS $EXTRA_NODE_FLAGS \
        --research.backfill \
        --research.backfill-min-block "$w_low" \
        --research.backfill-max-block "$w_high" \
        --research.backfill-concurrency "$BACKFILL_CONCURRENCY" \
        --research.db-path "$sqlite" \
        --research.export-config-path "$EXPORT_CONFIG" \
        >> "$wlog" 2>&1 &
    local pid=$!
    echo "$pid" > "$PID_FILE"

    wait_for_window_complete "$sqlite" "$w_low" "$w_high" "$pid" "$wlog"

    log "window [$w_low, $w_high] drained; SIGTERM node PID $pid (graceful — never SIGKILL)"
    kill -TERM "$pid"
    wait_for_pid_exit "$pid" "$STOP_GRACE_SECS"

    # Re-verify after the clean exit (the export worker's shutdown grace may have
    # shipped a few more rows). Only delete once provably empty.
    local pend
    pend=$(sqlite_ro "$sqlite" "SELECT COUNT(*) FILTER (WHERE state IN ('pending','retry')) FROM export_outbox;" 2>/dev/null || echo 1)
    [ "${pend:-1}" -eq 0 ] || halt "window $w_low-$w_high not drained after clean exit (${pend} pending). SQLite preserved at $sqlite."

    local mb; mb=$(du -m "$sqlite" 2>/dev/null | awk '{print $1}') || mb="?"
    rm -f "$sqlite" "$sqlite-wal" "$sqlite-shm"
    rm -f "$PID_FILE"
    log "window [$w_low, $w_high] sqlite deleted, reclaimed ~${mb:-?} MB"
}

# ─── Main ─────────────────────────────────────────────────────────────────────
SCHEDULE_COUNT=$(schedule_count)
[ "$SCHEDULE_COUNT" -ge 1 ] || halt "no schedule flags parsed from SCHEDULE_FLAGS/CSV_FLAGS"
preflight
resume=$(read_resume_point)
log "windowed export over [$END_BLOCK, $START_BLOCK], W=$WINDOW_SIZE, schedules=$SCHEDULE_COUNT, backfill_concurrency=$BACKFILL_CONCURRENCY, resume_high=$resume"

w_high=$resume
while [ "$w_high" -ge "$END_BLOCK" ]; do
    w_low=$(( w_high - WINDOW_SIZE + 1 ))
    [ "$w_low" -lt "$END_BLOCK" ] && w_low=$END_BLOCK
    run_window "$w_low" "$w_high"
    record_progress "$w_low"
    w_high=$(( w_low - 1 ))
done

log "DONE: exported [$END_BLOCK, $START_BLOCK] to ClickHouse across all windows."
