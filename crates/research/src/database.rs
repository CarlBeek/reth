//! SQLite-backed storage for the research data model.
//!
//! The DDL below is canonical; `crates/research/docs/storage-redesign.md` is the
//! v9-era design record that motivated it, not a current schema reference. Three
//! write granularities: an opt-in `tx_gas_results` row per (schedule, block,
//! tx) (`--research.tx-gas-results`), per-tx forensic rows for the txs that
//! failed or diverged in trace
//! ([`crate::divergence::DivergenceFacts::store_full_forensics`]), and per-block
//! aggregates keyed by execution-fact class (`unchanged` / `gas_only`) for the
//! rest. See [`SCHEMA_VERSION`] for how the shape got here.
//!
//! Why `SQLite`, not `DuckDB`: we tried `DuckDB` first for its analytical query
//! performance, but ran into its single-process writer-lock — the dashboard
//! couldn't read while reth held the writer. `SQLite` WAL handles 1 writer + N
//! readers across processes natively. The consumer (the Python `FastAPI` app in
//! the sibling repo) attaches via `DuckDB`'s `sqlite_scanner` extension and runs
//! analytical queries through `DuckDB`'s vectorized engine over the `SQLite`
//! storage. Best of both worlds.
//!
//! Schema-version policy (per the doc): the producer refuses to open a DB
//! whose latest `analysis_runs.schema_version` doesn't match its compiled-in
//! version. No migration shims — a major schema change is a full re-replay.
//!
//! Type translation from the `DuckDB` attempt:
//! - All numeric `DuckDB` types (UBIGINT, UINTEGER, UTINYINT, BIGINT) collapse to `SQLite` INTEGER.
//!   `SQLite` is dynamically typed; the affinity hints in the DDL are documentation as much as
//!   enforcement.
//! - BOOLEAN becomes INTEGER 0/1. rusqlite's `ToSql` for bool handles the conversion automatically;
//!   consumers read it back through `DuckDB` which treats nonzero INTEGER as truthy.
//! - HUGEINT (i128, used for `gas_delta_sum_sq`) becomes REAL. Loses precision past 2^53, but
//!   variance/stddev computed from it are already approximate.
//! - INTEGER[12] arrays and STRUCT(...)[] lists become JSON TEXT. The consumer `json_each()` them
//!   on read.
//! - `DuckDB` sequences (`CREATE SEQUENCE`, `DEFAULT nextval('seq')`) become `INTEGER PRIMARY KEY
//!   AUTOINCREMENT`.

use crate::divergence::{
    AccountDrivers, AggregateClass, EventLog, FrameOpcodeCounts, StorageDrivers,
};
use alloy_primitives::{keccak256, Address, B256};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use thiserror::Error;

/// Compiled-in schema version. Bumped on any column add/remove/type change.
///
/// Stored on each `analysis_runs` row; on open we verify the latest row
/// matches and reject DB files written by a different version (the doc's
/// "no migration shims; major schema change is a full re-replay" rule).
///
/// History:
/// - v1: `DuckDB` attempt (retired — single-process writer-lock issue).
/// - v2: `SQLite` + `DuckDB` `sqlite_scanner`. Initial production schema.
/// - v3: collapsed the two placeholder opcode-totals JSON columns (`opcode_count_totals_7904`,
///   `opcode_gas_delta_totals_7904`) into a single `opcode_totals_7904` populated with sparse
///   (opcode, count, `gas_baseline`, `gas_schedule`) tuples per bucket.
/// - v4: (undocumented — refer to git history)
/// - v5: split symmetric `status_changed` into two directions. Added `Bucket::ScheduleRescued` and
///   `block_coverage.tx_count_schedule_rescued` to surface tx flips toward success (schedule
///   rescued a baseline-failed tx) separately from flips toward failure. The classifier's shallow
///   predicate now switches from `divergence_call_depth` to `oog_call_depth` and drops the
///   `call_count == 0` guard.
/// - v6: added `Bucket::InconclusiveNeedsHigherSweep` and
///   `block_coverage.tx_count_inconclusive_needs_higher_sweep` for break-direction txs whose
///   highest configured replay tier still halted OOG without proving a throttled call-chain
///   bottleneck.
/// - v7: added `divergence_call_frames.deployed_bytecode_len` (NULL except on successful
///   CREATE/CREATE2 frames) so the EIP-8037 dashboard can plot true deployed-code size for the
///   deployment-ceiling chart instead of approximating from baseline gas.
/// - v8: added `divergences.schedule_state_gas_demanded` — state gas the tx attempted, including a
///   charge that OOG'd (so it's nonzero even when `schedule_state_gas_spent` is 0 because the state
///   op ran out of gas). Lets the dashboard show "this op needed N state gas" instead of 0.
/// - v9: added `Bucket::AaGasReestimation` + `block_coverage.tx_count_aa_gas_reestimation` for
///   ERC-4337 `EntryPoint` OOG breaks — an off-chain `UserOp` gas re-estimation fix, split out of
///   `contract_broken` where they were misclassified as `FixedGas` contract bottlenecks. This
///   column was added in-place without bumping `SCHEMA_VERSION`, so a v9 database created before it
///   lacked the column and every write failed with `no column named …`.
/// - v10: this PR's schema, a single bump over the v9 merge base. The editorial bucket/analysis-tag
///   taxonomy (`wallet_fixable_*`, `contract_broken`, `schedule_rescued`, `aa_gas_reestimation`,
///   `inconclusive_needs_higher_sweep`, `event_logs_changed`, `trace_only`) is removed entirely.
///   Aggregates are re-keyed by a 2-value execution-fact `class` (`unchanged` / `gas_only`) — the
///   only txs without a per-tx forensic row; every failure and trace-divergence now gets a per-tx
///   `divergences` row (strictly more per-tx coverage than the old drill-in buckets).
///   `block_coverage` drops the 10 `tx_count_<bucket>` editorial counts for fact counts
///   `tx_count_unchanged` / `tx_count_gas_only` / `tx_count_stored` (keeping `block_gas_used` /
///   `block_gas_limit`). `block_summaries` is re-keyed on `class` and renames `opcode_totals_7904`
///   to `opcode_totals` (keeping `cold_account_access_count`). `block_bucket_recipients` becomes
///   `block_recipients`, re-keyed on `class` (keeping `gas_delta_sum_succeeding`). `divergences`
///   drops `bucket` for the `outer_limit_only_failure` witness (distinguishes a
///   succeeded-only-at-the-bumped-tier tx from a no-OOG break). Additive nullable forensic columns
///   make "what failed and why" answerable from raw facts: `divergences` gains
///   `additional_gas_charged` (F4 total repricing surcharge), `failure_selector_path` (F6
///   root-to-divergence selector JSON), `tx_type` / `tx_nonce` / `entry_selector` /
///   `input_zero_bytes` / `input_nonzero_bytes` / `has_authorization` (F5 tx identity),
///   `failure_reason` (F1 `HaltReason` discriminant / `Revert` / `Rejected`), `revert_data` /
///   `revert_decoded` / `tx_output` (F2 capped returndata plus decode), and
///   `baseline_frame_success` / `baseline_frame_gas_used` / `baseline_frame_gas_provided` (F7
///   baseline twin of the failing frame), `surcharge_at_oog` (F13 repricing surcharge at the OOG
///   instant) and the `gas_div_*` quad (F10 first opcode where cumulative schedule gas exceeded
///   baseline); `divergence_call_frames` gains `repricing_gas_delta` (F4 per-frame surcharge) and
///   the F9 failing-frame context (`caller_pc` / `was_precompile` / `precompile_address` /
///   `gas_remaining_at_fail` / `is_divergent_frame`). `divergence_call_frames.to_address` /
///   `code_address` are now split (F3): `to_address` is the call/storage target (the proxy under
///   DELEGATECALL) while `code_address` is the code holder (the implementation, revm
///   `bytecode_address`); `codehash` and the metadata backfill resolve from `code_address` (F14).
///   `divergences` also gains the F12 per-category tax decomposition `tax_second_db_read` /
///   `tax_other` (these two sum to `additional_gas_charged`) plus `tax_intrinsic` (the tx-level
///   intrinsic-gas delta). Two zero-information columns are dropped:
///   `divergence_call_frames.state_gas_running` (was always NULL) and
///   `divergences.would_fit_in_original_limit` (an exact duplicate of `schedule_success`). F8
///   storage-reprice drivers — `sload_cold_count` / `sload_warm_count` / `sstore_cold_count` /
///   `sstore_{set,reset,clear,noop,dirty}_count` — are added to BOTH `divergences` (per-tx) and
///   `block_summaries` (per-class), attributing the native `COLD_STORAGE_ACCESS` / `STORAGE_WRITE`
///   / `REFUND_STORAGE_CLEAR` surcharges that never reach `additional_gas_charged`.
///   `divergence_call_frames` and `divergence_opcode_counts` gain a `trace_kind` (`"schedule"` /
///   `"baseline"`) in their primary key, mirroring `divergence_event_logs`, so a drill-in whose
///   call tree diverged also stores the baseline call tree (F15) and baseline opcode counts (F11).
///   There is no in-place migration: opening a pre-v10 database is rejected by
///   [`enforce_schema_version`] (via `PRAGMA user_version`) — wipe the divergences `SQLite` and
///   re-gather. The archive datadir is never touched. This branch additionally adds two optional,
///   additive `ClickHouse`-export tables within v10 — `analysis_manifests` (immutable
///   per-`analysis_config_hash` dataset descriptor) and `export_outbox` (one durable row per
///   export-enabled block output, drained by the embedded export worker) — created via `CREATE
///   TABLE IF NOT EXISTS`; they are inert unless `--research.export-config-path` is set, so v10 is
///   unchanged for non-export deployments.
/// - v11: additive per-class transaction-taxonomy + relative-gas columns on `block_summaries`,
///   requested by the dashboard because the aggregate-only cohorts (`unchanged` / `gas_only`) have
///   no per-tx rows to derive them from. Six EIP-2718 type counts
///   `tx_count_type_{legacy,access_list,dynamic_fee,blob,set_code,other}` (`other` catches any type
///   outside 0..=4, incl. future ones, so the six always sum to `tx_count`); two envelope-shape
///   counts `tx_count_simple_transfer` (non-create, empty calldata) / `tx_count_contract_call`
///   (non-create, non-empty calldata) which partition with the pre-existing tx-level
///   `tx_count_creation` (`creation + simple_transfer + contract_call == tx_count`);
///   `gas_delta_pct_hist`, a JSON array of exactly 13 ints — closed-left bins of `100 * gas_delta /
///   baseline_gas_used` with edges `[-100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf)`, every
///   aggregated tx binned so the bin sum equals `tx_count`; and `baseline_gas_used_sum`, the
///   class's baseline-gas denominator for ratio-of-sums cross-checks. There is no in-place
///   migration: a v10 database is rejected by [`enforce_schema_version`] — start a fresh `SQLite`
///   (or rename the old one aside; drain its `export_outbox` with the old binary first). The
///   `ClickHouse` dataset identity re-keys automatically because the analysis manifest embeds
///   `producer_schema_version`.
/// - v12: new `tx_gas_results` table — one row per (schedule, block, tx), covering **every** tx
///   rather than only the `store_full_forensics` minority. Collection is opt-in per run
///   (`--research.tx-gas-results`); the table exists in every v12 database but stays empty unless
///   the run asks for it, and `analysis_manifests` records which it was. The forensic tables answer
///   "what broke and why" for the divergent tail; a repricing *simulator* needs the repriced gas of
///   every transaction, including the byte-identical majority that previously dissolved into a
///   `block_summaries` class aggregate. Columns are the slim per-tx gas facts the analyze loop
///   already computes (`schedule_gas_used`, `schedule_total_gas_spent` — the pre-refund figure
///   EIP-7778 block accounting needs — `schedule_gas_refunded`, `schedule_floor_gas`,
///   `schedule_intrinsic_gas`, `schedule_state_gas_spent`, `min_multiplier_to_succeed`) plus per-tx
///   fee fields (`max_fee_per_gas`, `max_priority_fee_per_gas`, decimal-string U256) that were in
///   scope for `TxEnv` but never persisted. `block_coverage` additionally gains
///   `block_base_fee_per_gas`. Together these remove the simulator's dependency on an external
///   per-tx fee source (Xatu): fee data, gas limits, and repriced gas now all come from the replay
///   itself. Deliberately NOT re-added: `would_fit_in_original_limit`, dropped in v10 as an exact
///   duplicate of `schedule_success`. There is no in-place migration: a v11 database is rejected by
///   [`enforce_schema_version`] — start a fresh `SQLite` (drain the old one's `export_outbox` with
///   the old binary first).
pub const SCHEMA_VERSION: u32 = 12;

/// Errors raised by the storage layer.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// Underlying `SQLite` driver error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// I/O error opening or creating the database file.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encode error when serializing array / struct columns.
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// The on-disk schema version doesn't match the compiled-in
    /// `SCHEMA_VERSION`. There is no in-place migration — wipe & re-gather.
    #[error(
        "research DB schema mismatch: binary expects v{expected}, database is v{found}. \
         No migration — wipe the divergences SQLite and re-gather (the archive datadir is \
         unaffected)."
    )]
    SchemaVersionMismatch {
        /// Version the running binary expects.
        expected: u32,
        /// Version found in the existing database.
        found: u32,
    },
    /// An `analysis_manifests` row already exists for this
    /// `analysis_config_hash` but carries different `manifest_json`. Because the
    /// hash is derived from the JSON this is impossible without a hash
    /// collision or a bug, so we refuse the upsert rather than silently
    /// overwrite an immutable dataset descriptor.
    #[error(
        "analysis manifest conflict: hash {analysis_config_hash} already stored \
         with different manifest JSON"
    )]
    ManifestConflict {
        /// The conflicting dataset identity.
        analysis_config_hash: String,
    },
}

/// SQLite-backed divergence storage.
///
/// `Clone` is cheap — the underlying connection is wrapped in
/// `Arc<Mutex<_>>`. Backfill workers and the live arm can hold their own
/// handles to the same DB without contending on open/close. External
/// readers (the dashboard) get their own connections to the same file
/// via `SQLite` WAL.
#[derive(Debug, Clone)]
pub struct DivergenceDatabase {
    conn: Arc<Mutex<Connection>>,
    /// Counter for `record_block_output` calls, used to trigger
    /// periodic WAL checkpoints (see `record_block_output`). Wraps
    /// rather than overflowing.
    blocks_since_checkpoint: Arc<AtomicU64>,
}

/// How many `record_block_output` calls between explicit
/// `wal_checkpoint(RESTART)` runs. With one block per call and
/// roughly 200-500 tx-rows per block writing to ~5 tables each, this
/// keeps the WAL bounded to roughly the size of a thousand-block
/// batch (a few hundred MB), even with a long-lived reader that's
/// preventing TRUNCATE checkpoints from succeeding.
const CHECKPOINT_EVERY_N_BLOCKS: u64 = 1_000;

/// Cap on WAL size before `SQLite` recycles it. `SQLite` normally
/// auto-checkpoints when WAL crosses 1000 frames (~4MB) but the
/// PASSIVE checkpoints it runs can be defeated by a long-lived
/// reader — the WAL keeps growing forever. `journal_size_limit` forces
/// `SQLite` to truncate the WAL down to this size after every
/// successful checkpoint regardless of who else is reading. 1 GB is a
/// generous cap that won't fire mid-block.
const WAL_SIZE_LIMIT_BYTES: i64 = 1024 * 1024 * 1024;

impl DivergenceDatabase {
    /// Open (or create) a `SQLite` database at `path`. Initialises the schema
    /// if the file is new, sets WAL pragmas, and verifies the version of any
    /// existing data.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let conn = Connection::open(path.as_ref())?;
        Self::initialize(conn)
    }

    /// Open an in-memory `SQLite` database. Used in tests; on shutdown the
    /// data is discarded.
    pub fn in_memory() -> Result<Self, DatabaseError> {
        let conn = Connection::open_in_memory()?;
        Self::initialize(conn)
    }

    fn initialize(conn: Connection) -> Result<Self, DatabaseError> {
        // WAL mode is the bedrock of the producer/consumer concurrency
        // model: one writer + many readers across processes without
        // blocking. synchronous=NORMAL is the standard trade-off for WAL —
        // durable on commit, no fsync per page write. `:memory:` doesn't
        // honor WAL (returns "memory"), which is fine for tests.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        // Cap WAL size: when the WAL grows past this on a checkpoint,
        // SQLite truncates it. Without this, a long-lived reader (the
        // consumer dashboard) can keep WAL_pages "in use" indefinitely
        // and the WAL grows past the database size, slowing every read.
        conn.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT_BYTES)?;
        enforce_schema_version(&conn)?;
        initialize_schema(&conn)?;
        verify_schema_version(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            blocks_since_checkpoint: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Record a new analysis run. Stamps the current schema version so
    /// future opens can detect drift.
    ///
    /// `notes` is free-form (e.g. CLI args, motivation). Returns the
    /// auto-assigned `run_id`.
    pub fn record_analysis_run_start(
        &self,
        schedule_name: &str,
        schedule_config_hash: &str,
        reth_commit: Option<&str>,
        notes: Option<&str>,
    ) -> Result<u64, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let started_at = current_unix_seconds();
        conn.execute(
            "INSERT INTO analysis_runs (
                schema_version, schedule_name, schedule_config_hash,
                reth_commit, run_started_at, notes
             ) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                SCHEMA_VERSION,
                schedule_name,
                schedule_config_hash,
                reth_commit,
                started_at as i64,
                notes,
            ],
        )?;
        Ok(conn.last_insert_rowid() as u64)
    }

    /// Mark an analysis run as finished, recording the wall-clock end time
    /// and the total block count processed.
    pub fn record_analysis_run_end(
        &self,
        run_id: u64,
        blocks_processed: u64,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let finished_at = current_unix_seconds();
        conn.execute(
            "UPDATE analysis_runs
             SET run_finished_at = ?, blocks_processed = ?
             WHERE run_id = ?",
            params![finished_at as i64, blocks_processed as i64, run_id as i64],
        )?;
        Ok(())
    }
}

fn current_unix_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Verify the on-disk schema version matches [`SCHEMA_VERSION`].
///
/// A freshly initialised DB has zero rows in `analysis_runs` and passes
/// trivially; the version is stamped the first time
/// [`DivergenceDatabase::record_analysis_run_start`] is called.
/// Enforce the research-DB schema version via `SQLite`'s `PRAGMA user_version`.
///
/// There is **no in-place migration**: a [`SCHEMA_VERSION`] bump means wipe the
/// divergences `SQLite` and re-gather (the archive datadir is unaffected). A
/// brand-new DB is stamped with the current version; an existing DB whose version
/// differs — including a legacy/un-stamped DB (`user_version == 0` with tables
/// already present) — is rejected so the producer fails loudly instead of silently
/// writing into a stale schema.
///
/// Must run BEFORE [`initialize_schema`]: `CREATE TABLE IF NOT EXISTS` would
/// otherwise no-op against a stale DB's tables, and the missing columns would only
/// surface as a runtime "no column named …" on the first write. This is the
/// actually-enforced gate; [`verify_schema_version`] (below) is the older,
/// inert `analysis_runs`-based check kept for backward compatibility.
fn enforce_schema_version(conn: &Connection) -> Result<(), DatabaseError> {
    let user_version: u32 =
        conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? as u32;
    let has_schema = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='block_coverage'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    if !has_schema && user_version == 0 {
        // Brand-new DB: stamp the current version; the DDL then creates current tables.
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        return Ok(());
    }
    if user_version == SCHEMA_VERSION {
        return Ok(());
    }
    Err(DatabaseError::SchemaVersionMismatch { expected: SCHEMA_VERSION, found: user_version })
}

fn verify_schema_version(conn: &Connection) -> Result<(), DatabaseError> {
    let latest: Option<u32> = conn
        .query_row(
            "SELECT schema_version FROM analysis_runs
             ORDER BY run_id DESC LIMIT 1",
            [],
            |row| {
                let v: i64 = row.get(0)?;
                Ok(v as u32)
            },
        )
        .optional()?;
    match latest {
        Some(found) if found != SCHEMA_VERSION => {
            Err(DatabaseError::SchemaVersionMismatch { expected: SCHEMA_VERSION, found })
        }
        _ => Ok(()),
    }
}

/// Apply the full DDL. Idempotent via `CREATE TABLE IF NOT EXISTS` and
/// matching `CREATE INDEX IF NOT EXISTS` guards.
fn initialize_schema(conn: &Connection) -> Result<(), DatabaseError> {
    // One row per (schedule, block). Always emitted, even for blocks with
    // zero divergences, so coverage joins work.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_coverage (
            schedule_name        TEXT    NOT NULL,
            schedule_config_hash TEXT    NOT NULL,
            block_number         INTEGER NOT NULL,
            block_hash           BLOB    NOT NULL,
            parent_hash          BLOB    NOT NULL,
            timestamp            INTEGER NOT NULL,
            tx_count             INTEGER NOT NULL,
            -- Execution-fact split of the block's txs (no editorial taxonomy):
            -- `unchanged` (byte-identical), `gas_only` (only gas differs), and
            -- `stored` (everything else — a per-tx `divergences` row exists, so
            -- its cohort is reconstructable downstream). Sum == tx_count.
            tx_count_unchanged   INTEGER NOT NULL,
            tx_count_gas_only    INTEGER NOT NULL,
            tx_count_stored      INTEGER NOT NULL,
            -- Native block header values (NULL on rows written before v10 / not
            -- re-touched). gas_used is the actual mainnet baseline the block
            -- consumed; gas_limit is the protocol cap. Together they anchor
            -- utilization, block-fullness shift, and gas-limit-exceedance under
            -- a repricing schedule. Nullable so an additive ADD COLUMN on an
            -- existing DB leaves historical rows readable as 'n/a'.
            block_gas_used                     INTEGER,
            block_gas_limit                    INTEGER,
            -- EIP-1559 base fee from the header. With the per-tx fee caps in
            -- `tx_gas_results` this is everything a fee-market simulation needs,
            -- so no external per-tx fee source is required.
            block_base_fee_per_gas             INTEGER,
            PRIMARY KEY (schedule_name, block_number, block_hash)
        );",
    )?;

    // One row per (schedule, block, tx) — covering EVERY transaction, not only
    // the `store_full_forensics` minority that earns a `divergences` row.
    //
    // The table is created unconditionally so the schema is identical across
    // runs, but collection is opt-in (`--research.tx-gas-results`): it is the
    // largest table the producer writes, and only a repricing simulator needs
    // it. A run that leaves it off simply writes no rows here.
    //
    // The forensic tables are built to explain the divergent tail; a repricing
    // simulator instead needs the repriced gas of every tx, including the
    // byte-identical majority that only ever reaches a `block_summaries` class
    // aggregate. This table is that per-tx spine: slim (no call frames, no
    // opcode counts, no capped returndata) and uncapped (unlike drill-ins,
    // which `--research.max-divergences-per-block` truncates).
    //
    // Gas column semantics, which differ in ways that matter downstream:
    //   * `schedule_gas_used`        — sender-facing: post-refund, floor-applied.
    //   * `schedule_total_gas_spent` — PRE-refund. EIP-7778 makes block-level gas accounting ignore
    //     refunds, so this (not `schedule_gas_used`) is the figure that feeds block-fill /
    //     gas-limit analysis.
    //   * `schedule_gas_refunded`    — the difference, retained explicitly so neither figure has to
    //     be back-derived.
    // `schedule_intrinsic_gas` is NULL for execution-only schedules (they have
    // no intrinsic opinion); every other gas column is a measured value.
    //
    // Fee caps are TEXT because they are U256 in principle — bound as decimal
    // strings rather than risking an i64 overflow on a hostile value.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tx_gas_results (
            schedule_name        TEXT    NOT NULL,
            schedule_config_hash TEXT    NOT NULL,
            block_number         INTEGER NOT NULL,
            tx_index             INTEGER NOT NULL,
            tx_hash              BLOB    NOT NULL,
            tx_type              INTEGER NOT NULL,
            tx_gas_limit         INTEGER NOT NULL,
            -- U256 decimal strings; max_priority_fee_per_gas is NULL for
            -- legacy / access-list txs that carry no priority cap.
            max_fee_per_gas          TEXT,
            max_priority_fee_per_gas TEXT,
            baseline_success             INTEGER NOT NULL,
            baseline_gas_used            INTEGER NOT NULL,
            baseline_total_gas_spent     INTEGER NOT NULL,
            schedule_success             INTEGER NOT NULL,
            schedule_gas_used            INTEGER NOT NULL,
            schedule_total_gas_spent     INTEGER NOT NULL,
            schedule_gas_refunded        INTEGER NOT NULL,
            schedule_floor_gas           INTEGER NOT NULL,
            schedule_state_gas_spent     INTEGER NOT NULL,
            schedule_intrinsic_gas       INTEGER,
            min_multiplier_to_succeed    REAL,
            PRIMARY KEY (schedule_name, block_number, tx_index)
        );",
    )?;

    // One row per (schedule, block, class) for the aggregate-only fact classes
    // `{unchanged, gas_only}` — these summarize the txs with no per-tx
    // `divergences` row (the silent majority). Stored txs are reconstructable
    // from their per-tx rows, so they get no summary row here.
    //
    // `opcode_totals` is a sparse JSON array of
    // `{opcode, count, gas_baseline, gas_schedule}` objects — one entry per
    // distinct opcode the class touched in this block, summed across every
    // frame of every tx. Lets the dashboard compute both the share of gas
    // each opcode burned and the delta the schedule introduced.
    // `multiplier_log2_hist` and `gas_delta_log2_hist` are JSON arrays of
    // exactly 12 ints (fixed-size
    // log2 bins) so the CDF charts read pre-binned data; `gas_delta_pct_hist`
    // is the same idea with exactly 13 ints (fixed percentage bins).
    //
    // `gas_delta_sum_sq` is REAL (loses precision past 2^53) rather than
    // a hypothetical i128: variance/stddev derived from it are already
    // approximate.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_summaries (
            schedule_name TEXT    NOT NULL,
            block_number  INTEGER NOT NULL,
            class         TEXT    NOT NULL,
            tx_count      INTEGER NOT NULL,
            gas_delta_sum       INTEGER,
            gas_delta_sum_sq    REAL,
            gas_delta_min       INTEGER,
            gas_delta_max       INTEGER,
            gas_delta_log2_hist TEXT,
            opcode_totals           TEXT,
            -- state_gas_sum / state_gas_spillover_sum / tx_count_runtime_state are
            -- EIP-8037-only (the state-gas reservoir model); inert (0 / NULL) for
            -- EIP-8038 — exclude them from 8038 analysis.
            state_gas_sum           INTEGER,
            state_gas_spillover_sum INTEGER,
            multiplier_log2_hist    TEXT,
            tx_count_creation       INTEGER,
            tx_count_authorization  INTEGER,
            tx_count_runtime_state  INTEGER,
            tx_count_no_state       INTEGER,
            cold_account_access_count INTEGER,
            sload_cold_count          INTEGER,
            sload_warm_count          INTEGER,
            sstore_cold_count         INTEGER,
            sstore_set_count          INTEGER,
            sstore_reset_count        INTEGER,
            sstore_clear_count        INTEGER,
            sstore_noop_count         INTEGER,
            sstore_dirty_count        INTEGER,
            -- F2/F3 account-side gas drivers summed over this class's txs:
            -- warm account accesses, CALL/CALLCODE value transfers, CREATE+CREATE2
            -- opcodes, and the tx-declared EIP-2930 access-list address / storage
            -- key counts. NULL when the class saw none of these.
            warm_account_access_count    INTEGER,
            value_transfer_count         INTEGER,
            create_opcode_count          INTEGER,
            access_list_address_count    INTEGER,
            access_list_storage_key_count INTEGER,
            -- v11 transaction taxonomy over the class's (aggregated-only) txs.
            -- The six type counts sum to tx_count (`other` = any EIP-2718
            -- type outside 0..=4, incl. future ones); creation +
            -- simple_transfer + contract_call == tx_count
            -- (tx_count_creation above is the tx-level create count).
            tx_count_type_legacy      INTEGER,
            tx_count_type_access_list INTEGER,
            tx_count_type_dynamic_fee INTEGER,
            tx_count_type_blob        INTEGER,
            tx_count_type_set_code    INTEGER,
            tx_count_type_other       INTEGER,
            tx_count_simple_transfer  INTEGER,
            tx_count_contract_call    INTEGER,
            -- v11: JSON array of exactly 13 ints — closed-left bins of
            -- 100*gas_delta/baseline_gas_used with edges
            -- [-100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf); bin sum
            -- equals tx_count. Baseline-gas denominator alongside.
            gas_delta_pct_hist        TEXT,
            baseline_gas_used_sum     INTEGER,
            PRIMARY KEY (schedule_name, block_number, class)
        );",
    )?;

    // Per-(schedule, block, class) recipient attribution for the aggregate-only
    // fact classes `{unchanged, gas_only}` — gives the silent-majority cohorts
    // (whose txs have no per-tx row) destination/selector attribution. Stored txs
    // carry their own `recipient` on the per-tx row. One row per distinct destination, ranked
    // and truncated to the top `BUCKET_RECIPIENT_TOP_K` by `tx_count`; the
    // truncated tail is folded into a single synthetic `recipient='__other__'`
    // row so the unattributed mass stays quantified (no silent long-tail
    // blind spot). `recipient` is the tx's to-address as lowercase `{:#x}`
    // hex (joins `contract_labels.address`), `'__create__'` for contract
    // creations, or `'__other__'` for the tail. `top_selector` is the 4-byte
    // function selector (`x'00000000'` when absent / create / tail).
    //
    // `gas_delta_sum_succeeding` sums `gas_delta` ONLY over txs that succeeded
    // under the schedule within their original gas limit. Txs that OOG under
    // the schedule are deliberately excluded: their `schedule_gas_used` is
    // revm's all-gas-consumed halt value (≈ the inflated tier limit), so
    // including them would pollute the sum with halt-gas artefacts. Their
    // `tx_count` is still counted, so the failed cohort is attributed by
    // *who/how many*, while gas magnitude reflects only the clean cohort.
    //
    // NOTE: this OOG-exclusion gate is a no-op for the only classes that reach
    // this rollup (`unchanged` / `gas_only`) — those are all tier-1 successes,
    // so `succeeded_within_limit` is always true for them. The gate is purely
    // defensive (it would matter only if a failing/OOG tx were ever rolled into
    // an aggregate class instead of getting its own per-tx `divergences` row).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_recipients (
            schedule_name TEXT    NOT NULL,
            block_number  INTEGER NOT NULL,
            class         TEXT    NOT NULL,
            recipient     TEXT    NOT NULL,
            top_selector  BLOB    NOT NULL,
            tx_count      INTEGER NOT NULL,
            gas_delta_sum_succeeding INTEGER NOT NULL,
            PRIMARY KEY (schedule_name, block_number, class, recipient, top_selector)
        );",
    )?;

    // Surrogate divergence_id (auto-increment) so child tables can FK to a
    // single column. Natural key is (schedule_name, block_number, tx_index,
    // schedule_config_hash); a UNIQUE constraint enforces it.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergences (
            divergence_id        INTEGER PRIMARY KEY AUTOINCREMENT,
            schedule_name        TEXT    NOT NULL,
            schedule_config_hash TEXT    NOT NULL,
            block_number         INTEGER NOT NULL,
            tx_index             INTEGER NOT NULL,
            tx_hash              BLOB    NOT NULL,
            timestamp            INTEGER NOT NULL,
            -- Execution-fact witness that the schedule replay succeeded only at
            -- a bumped tier while exceeding the original limit (and matched
            -- baseline behavior). Lets downstream tell wallet-fixable-by-more-gas
            -- from a contract break that has no OOG site. NULL = not applicable.
            outer_limit_only_failure INTEGER,

            sender       TEXT    NOT NULL,
            recipient    TEXT,
            is_create    INTEGER NOT NULL,
            tx_gas_limit INTEGER NOT NULL,

            baseline_success   INTEGER NOT NULL,
            schedule_success   INTEGER NOT NULL,
            status_changed     INTEGER NOT NULL,
            event_logs_changed INTEGER NOT NULL,
            output_changed     INTEGER NOT NULL,
            logs_bloom_changed INTEGER NOT NULL,

            baseline_gas_used        INTEGER NOT NULL,
            schedule_gas_used        INTEGER NOT NULL,
            -- schedule_gas_used - baseline_gas_used. NOTE: for a rescued failure
            -- (`schedule_success=0 AND replay_halt_oog IS NULL`) the schedule gas
            -- figures are the bumped (N×) tier's execution, not the 1× run, so
            -- `gas_delta` is NOT a meaningful repricing measure when
            -- `baseline_success=0`. Downstream repricing aggregates must filter
            -- on `schedule_success=1` (and `baseline_success=1`).
            gas_delta                INTEGER NOT NULL,
            baseline_total_gas_spent INTEGER,
            baseline_gas_refunded    INTEGER,
            schedule_total_gas_spent INTEGER,
            schedule_gas_refunded    INTEGER,
            schedule_intrinsic_gas   INTEGER,
            schedule_floor_gas       INTEGER,
            min_multiplier_to_succeed   REAL,

            divergence_contract   TEXT,
            divergence_pc         INTEGER,
            divergence_call_depth INTEGER,
            divergence_opcode     INTEGER,
            oog_contract          TEXT,
            oog_pc                INTEGER,
            oog_call_depth        INTEGER,
            oog_opcode            INTEGER,
            oog_pattern           TEXT,
            oog_gas_remaining     INTEGER,
            oog_chain_proportional INTEGER,
            oog_bottleneck_depth   INTEGER,
            oog_bottleneck_kind    TEXT,

            -- EIP-8037-only: the state-gas reservoir model is an 8037 construct.
            -- These columns are inert (0 / NULL) for EIP-8038 (and 7904/baseline)
            -- and should be excluded from 8038 analysis.
            schedule_state_gas_spent    INTEGER,
            schedule_state_gas_demanded INTEGER,
            schedule_initial_state_gas  INTEGER,
            schedule_initial_reservoir  INTEGER,
            runtime_state_gas           INTEGER,
            runtime_state_gas_spillover INTEGER,
            state_gas_category          TEXT,
            reservoir_exhausted         INTEGER,

            -- Outcome of the schedule replays final attempted multiplier
            -- tier (see --research.gas-limit-multipliers):
            --   1    = halted OOG at the highest tried tier; the true
            --          minimum multiplier exceeds the configured ceiling.
            --   0    = halted for a non-gas reason (revert, stack,
            --          intrinsic rejection); more gas would not help.
            --   NULL = at least one tier succeeded; consumers should use
            --          min_multiplier_to_succeed for the magnitude.
            -- Lets the consumer split unresolved replay failures into
            -- needs-more-gas vs permanently-broken-under-this-schedule.
            replay_halt_oog             INTEGER,

            -- Per-tx count of cold account accesses (first access this tx) for
            -- the account-access opcodes (BALANCE / EXTCODE* / CALL family /
            -- SELFDESTRUCT). Collected for every schedule.
            cold_account_access_count   INTEGER,

            -- F4: total repricing surcharge charged across all frames
            -- (signed; the summed per-opcode deltas). Per-frame breakdown
            -- lives in divergence_call_frames.repricing_gas_delta.
            additional_gas_charged      INTEGER,
            -- F6: root→divergence 4-byte selector path as a JSON array of
            -- nullable hex strings (null = a frame with no selector).
            failure_selector_path       TEXT,

            -- F5: top-level tx identity. tx_type is the EIP-2718 type byte;
            -- entry_selector is the first 4 calldata bytes; input_zero/nonzero
            -- are the calldata byte split; has_authorization flags EIP-7702.
            tx_type                     INTEGER,
            tx_nonce                    INTEGER,
            entry_selector              BLOB,
            input_zero_bytes            INTEGER,
            input_nonzero_bytes         INTEGER,
            has_authorization           INTEGER,

            -- F1: structured failure reason (HaltReason discriminant / Revert /
            -- Rejected / NULL on success).
            failure_reason              TEXT,
            -- F2: raw revert returndata (capped), its best-effort decode, and
            -- the top-level tx output bytes (capped).
            revert_data                 BLOB,
            revert_decoded              TEXT,
            tx_output                   BLOB,

            -- F7: baseline counterpart of the frame that failed under the
            -- schedule (matched by call_index) — wallet-fixable discriminator.
            baseline_frame_success      INTEGER,
            baseline_frame_gas_used     INTEGER,
            baseline_frame_gas_provided INTEGER,

            -- F13: cumulative repricing surcharge at the OOG instant.
            surcharge_at_oog            INTEGER,

            -- F10: first opcode where cumulative schedule gas exceeded baseline
            -- (distinct from the behavioral divergence_* columns).
            gas_div_contract            TEXT,
            gas_div_pc                  INTEGER,
            gas_div_call_depth          INTEGER,
            gas_div_opcode              INTEGER,

            -- F12: per-category decomposition of the repricing tax. The first
            -- two sum to additional_gas_charged; tax_intrinsic is the separate
            -- tx-level intrinsic delta.
            tax_second_db_read          INTEGER,
            -- Reserved for multiplier / CSV schedules (the unclassified delta
            -- bucket); identically 0 for EIP-8037 and EIP-8038, whose only
            -- opcode-delta category is `tax_second_db_read`.
            tax_other                   INTEGER,
            tax_intrinsic               INTEGER,

            -- F8: storage-reprice drivers. cold reads/writes drive
            -- COLD_STORAGE_ACCESS; set/reset/clear drive STORAGE_WRITE; clear
            -- also drives REFUND_STORAGE_CLEAR.
            sload_cold_count            INTEGER,
            sload_warm_count            INTEGER,
            sstore_cold_count           INTEGER,
            sstore_set_count            INTEGER,
            sstore_reset_count          INTEGER,
            sstore_clear_count          INTEGER,
            sstore_noop_count           INTEGER,
            sstore_dirty_count          INTEGER,

            -- F2/F3: account-side gas drivers. warm account accesses + value
            -- transfers + CREATE/CREATE2 are runtime inspector counts; the two
            -- access-list counts come from the tx envelope.
            warm_account_access_count     INTEGER,
            value_transfer_count          INTEGER,
            create_opcode_count           INTEGER,
            access_list_address_count     INTEGER,
            access_list_storage_key_count INTEGER,

            -- F1: 1×-failure forensics. When a tx fails at the mainnet-equivalent
            -- tier (1×) but is rescued at a bumped tier (or fails at both), these
            -- preserve WHERE/WHY it broke at 1× (the rescued/highest-tier figures
            -- in the columns above describe the accepted attempt, not the 1×
            -- failure). NULL when 1× succeeded or wasn't a failure.
            tier1_failure_reason        TEXT,
            tier1_oog_opcode            INTEGER,
            tier1_oog_contract          TEXT,
            tier1_oog_pc                INTEGER,
            tier1_oog_depth             INTEGER,
            tier1_oog_gas_remaining     INTEGER,
            tier1_failing_selector      BLOB,
            tier1_failing_gas_provided  INTEGER,
            tier1_failing_gas_requested INTEGER,

            UNIQUE (schedule_name, block_number, tx_index, schedule_config_hash)
        );",
    )?;

    // One row per call frame of a drill-in divergence. `trace_kind`
    // ("schedule" / "baseline") partitions the two traces in one table,
    // mirroring `divergence_event_logs`; baseline rows (F15) are written only
    // when the call tree diverged.
    //
    // FK constraints from child tables to `divergences` are documented in
    // the design doc but omitted at the DDL level; the producer maintains
    // referential integrity at the application layer via the per-block
    // transactional delete pattern.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_call_frames (
            divergence_id          INTEGER NOT NULL,
            trace_kind             TEXT    NOT NULL DEFAULT 'schedule',
            call_index             INTEGER NOT NULL,
            parent_call_index      INTEGER,
            depth                  INTEGER NOT NULL,
            from_address           TEXT    NOT NULL,
            to_address             TEXT    NOT NULL,
            code_address           TEXT,
            codehash               BLOB,
            call_type              TEXT    NOT NULL,
            selector               BLOB,
            value_wei              TEXT,
            gas_provided           INTEGER NOT NULL,
            gas_used               INTEGER NOT NULL,
            gas_margin             INTEGER,
            success                INTEGER NOT NULL,
            parent_gas_at_call     INTEGER,
            gas_requested_on_stack INTEGER,
            eip150_cap_binding     INTEGER,
            deployed_bytecode_len  INTEGER,
            repricing_gas_delta    INTEGER NOT NULL,
            -- F9: failing-frame context.
            caller_pc              INTEGER,
            was_precompile         INTEGER NOT NULL DEFAULT 0,
            precompile_address     TEXT,
            gas_remaining_at_fail  INTEGER,
            is_divergent_frame     INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (divergence_id, trace_kind, call_index)
        );",
    )?;

    // Sparse opcode counts keyed by frame. Zero rows omitted by the
    // producer at insert time. `trace_kind` partitions schedule vs baseline
    // (F11) — baseline rows carry the baseline opcode counts (where
    // gas_baseline == gas_schedule).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_opcode_counts (
            divergence_id INTEGER NOT NULL,
            trace_kind    TEXT    NOT NULL DEFAULT 'schedule',
            call_index    INTEGER NOT NULL,
            opcode        INTEGER NOT NULL,
            count         INTEGER NOT NULL,
            gas_baseline  INTEGER NOT NULL,
            gas_schedule  INTEGER NOT NULL,
            PRIMARY KEY (divergence_id, trace_kind, call_index, opcode)
        );",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_event_logs (
            divergence_id INTEGER NOT NULL,
            trace_kind    TEXT    NOT NULL,
            log_index     INTEGER NOT NULL,
            address       TEXT    NOT NULL,
            topic0        BLOB,
            topic1        BLOB,
            topic2        BLOB,
            topic3        BLOB,
            data_bytes    BLOB,
            data_hash     BLOB,
            PRIMARY KEY (divergence_id, trace_kind, log_index)
        );",
    )?;

    // Static contract metadata, keyed by codehash so identical
    // implementations dedupe across addresses. Backfilled by the
    // contract-metadata-backfill subcommand.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contract_metadata (
            codehash               BLOB PRIMARY KEY,
            representative_address TEXT,
            solc_version           TEXT,
            solc_commit            TEXT,
            evm_target             TEXT,
            cbor_present           INTEGER NOT NULL,
            has_metadata_hash      INTEGER NOT NULL,
            bytecode_len           INTEGER NOT NULL,
            extracted_at           INTEGER NOT NULL
        );",
    )?;

    // Per-address contract labels backfilled out-of-band by the
    // external-label task (Blockscout → Sourcify → Etherscan fallback
    // chain). Keyed by address, not codehash: proxy contracts share a
    // codehash across many products, so the human-readable label has
    // to follow the address. `protocol_tag` is the curated brand name
    // ("USDC", "Uniswap V3: Router") — only Blockscout's API
    // currently surfaces these. `contract_name` is the Solidity class
    // name (e.g. "FiatTokenV2_2") and tends to be present on any
    // verified contract.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contract_labels (
            address        TEXT    PRIMARY KEY,
            contract_name  TEXT,
            protocol_tag   TEXT,
            is_proxy       INTEGER NOT NULL DEFAULT 0,
            impl_address   TEXT,
            source         TEXT    NOT NULL,
            fetched_at     INTEGER NOT NULL
        );",
    )?;

    // Per-selector function signatures backfilled from OpenChain /
    // 4byte.directory. Selectors are address- and chain-independent so
    // the table has no chain/address columns. `signature` is the
    // canonical Solidity signature string (e.g.
    // "transfer(address,uint256)"); collisions across multiple known
    // names are resolved at write time by the orchestrator.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS function_signatures (
            selector    BLOB    PRIMARY KEY,
            signature   TEXT,
            source      TEXT    NOT NULL,
            fetched_at  INTEGER NOT NULL
        );",
    )?;

    // Per-replay-run manifest. A consumer reads the latest row to detect
    // "is this lake written by the current code or do I need to migrate?".
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS analysis_runs (
            run_id               INTEGER PRIMARY KEY AUTOINCREMENT,
            schema_version       INTEGER NOT NULL,
            schedule_name        TEXT    NOT NULL,
            schedule_config_hash TEXT    NOT NULL,
            reth_commit          TEXT,
            run_started_at       INTEGER NOT NULL,
            run_finished_at      INTEGER,
            blocks_processed     INTEGER,
            notes                TEXT
        );",
    )?;

    // Indexes for the per-recipient / per-codehash / per-bucket lookups
    // the dashboard hits. The composite divergence indexes mirror the
    // consumer's hot path: every dashboard aggregate is scoped by
    // `(schedule_name, bucket)` before grouping by recipient, opcode, or
    // bottleneck fields.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_div_schedule      ON divergences(schedule_name);
         CREATE INDEX IF NOT EXISTS idx_div_block         ON divergences(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_div_recipient     ON divergences(schedule_name, recipient);
         CREATE INDEX IF NOT EXISTS idx_div_tx_hash       ON divergences(tx_hash);
         CREATE INDEX IF NOT EXISTS idx_div_sched_oog
             ON divergences(schedule_name, oog_call_depth, oog_bottleneck_kind);
         CREATE INDEX IF NOT EXISTS idx_div_sched_opcode
             ON divergences(schedule_name, divergence_opcode);
         CREATE INDEX IF NOT EXISTS idx_dcf_to_addr       ON divergence_call_frames(to_address);
         CREATE INDEX IF NOT EXISTS idx_dcf_codehash      ON divergence_call_frames(codehash);
         CREATE INDEX IF NOT EXISTS idx_doc_opcode        ON divergence_opcode_counts(opcode);
         CREATE INDEX IF NOT EXISTS idx_bc_schedule_block ON block_coverage(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_block ON block_summaries(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_class ON block_summaries(schedule_name, class);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_opcode_totals
             ON block_summaries(schedule_name)
             WHERE opcode_totals IS NOT NULL AND opcode_totals <> '[]';
         CREATE INDEX IF NOT EXISTS idx_tgr_sched_block
             ON tx_gas_results(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_tgr_tx_hash     ON tx_gas_results(tx_hash);
         CREATE INDEX IF NOT EXISTS idx_br_sched_recipient
             ON block_recipients(schedule_name, recipient);",
    )?;

    // Immutable per-dataset manifest. A durable outbox row can outlive the
    // process/configuration that created it, so a restarted process must be
    // able to export an old pending item using the manifest the data was
    // produced under — not the current CLI configuration. Keyed by the
    // deterministic `analysis_config_hash` from `export/model.rs`.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS analysis_manifests (
            analysis_config_hash   TEXT PRIMARY KEY,
            schema_version         INTEGER NOT NULL,
            chain_id               INTEGER NOT NULL,
            producer_git_commit    TEXT NOT NULL,
            replay_semantics       TEXT NOT NULL,
            manifest_json          TEXT NOT NULL,
            created_at             INTEGER NOT NULL
        );",
    )?;

    // Transactional export outbox. One row per export-enabled block output,
    // written in the SAME transaction as the analytical rows so there is no
    // crash window between local persistence and the export request. The
    // worker drains rows whose `state` is `pending`/`retry` and whose
    // `next_attempt_at` is due. `state` transitions: pending → retry (transient
    // failure, backoff in `next_attempt_at`) → exported (coverage insert
    // confirmed) or blocked (permanent, e.g. oversized row). The deterministic
    // `export_id` is the primary key, so re-recording a block is idempotent.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS export_outbox (
            export_id             TEXT PRIMARY KEY,
            analysis_config_hash  TEXT NOT NULL,
            schedule_name         TEXT NOT NULL,
            schedule_config_hash  TEXT NOT NULL,
            block_number          INTEGER NOT NULL,
            block_hash            BLOB NOT NULL,
            payload_version       INTEGER NOT NULL,
            payload_zstd          BLOB NOT NULL,
            payload_hash          BLOB NOT NULL,
            payload_bytes         INTEGER NOT NULL,
            state                 TEXT NOT NULL DEFAULT 'pending',
            attempts              INTEGER NOT NULL DEFAULT 0,
            next_attempt_at       INTEGER NOT NULL DEFAULT 0,
            last_error            TEXT,
            created_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL,
            exported_at           INTEGER,
            CHECK (state IN ('pending', 'retry', 'exported', 'blocked'))
        );",
    )?;

    // Drives the worker's "next due item" query: filter by state, order by
    // due time then insertion order.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_export_outbox_due
            ON export_outbox(state, next_attempt_at, created_at);",
    )?;

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Row types
// ─────────────────────────────────────────────────────────────────────────────

/// One-shot output for a single (schedule, block): coverage row + zero or
/// more bucket-summary rows + zero or more drill-in records. Built by the
/// `BlockAggregator` and consumed by [`DivergenceDatabase::record_block_output`]
/// in a single transaction so the per-block state lands atomically.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockOutput {
    /// Always present. Counts go to `block_coverage`.
    pub coverage: BlockCoverageRow,
    /// One per non-empty aggregate class (`unchanged` / `gas_only`). Go to
    /// `block_summaries`.
    pub summaries: Vec<BlockSummaryRow>,
    /// Drill-in records — one per tx meeting [`DivergenceFacts::store_full_forensics`].
    pub drill_ins: Vec<DrillInRecord>,
    /// Per-(class) top-recipient rollup rows. Go to `block_recipients`.
    pub recipients: Vec<RecipientRow>,
    /// One row per tx in the block. Go to `tx_gas_results`. Unlike
    /// [`Self::drill_ins`] this is never truncated — the simulator needs every
    /// tx, not a capped sample. Empty for runs that didn't opt into the spine
    /// with `--research.tx-gas-results`.
    ///
    /// `serde(default)` so a pre-v12 outbox envelope still decodes (as an empty
    /// vec) instead of failing the whole payload — the format version is
    /// provenance, not a decode gate.
    #[serde(default)]
    pub tx_gas_results: Vec<TxGasResultRow>,
}

/// One row of `tx_gas_results`: the slim per-tx gas facts for a single
/// (schedule, block, tx), emitted for every transaction of a run that opted
/// into the spine (`--research.tx-gas-results`).
///
/// See the table DDL for the `schedule_gas_used` (post-refund, sender-facing)
/// vs `schedule_total_gas_spent` (pre-refund, EIP-7778 block accounting)
/// distinction — they are not interchangeable.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TxGasResultRow {
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub tx_index: u32,
    pub tx_hash: B256,
    /// EIP-2718 envelope type byte.
    pub tx_type: u8,
    pub tx_gas_limit: u64,
    /// Fee caps as U256 decimal strings. `max_priority_fee_per_gas` is `None`
    /// for envelope types that carry no priority cap (legacy, access-list).
    pub max_fee_per_gas: String,
    pub max_priority_fee_per_gas: Option<String>,
    pub baseline_success: bool,
    pub baseline_gas_used: u64,
    pub baseline_total_gas_spent: u64,
    pub schedule_success: bool,
    /// Sender-facing: post-refund, floor-applied.
    pub schedule_gas_used: u64,
    /// Pre-refund — the EIP-7778 block-accounting figure.
    pub schedule_total_gas_spent: u64,
    pub schedule_gas_refunded: u64,
    pub schedule_floor_gas: u64,
    pub schedule_state_gas_spent: u64,
    /// `None` for execution-only schedules, which have no intrinsic opinion.
    pub schedule_intrinsic_gas: Option<u64>,
    pub min_multiplier_to_succeed: Option<f64>,
}

/// One row of the `block_recipients` rollup: a single destination's share of a
/// (schedule, block, class). See the table DDL for the ranking / truncation
/// rules and the `gas_delta_sum_succeeding` semantics.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RecipientRow {
    pub schedule_name: String,
    pub block_number: u64,
    pub class: AggregateClass,
    /// Lowercase `{:#x}` to-address, or the `__create__` / `__other__` sentinel.
    pub recipient: String,
    /// 4-byte function selector; all-zero when absent / create / tail row.
    pub top_selector: [u8; 4],
    pub tx_count: u32,
    /// Sum of `gas_delta` over the succeeding-within-original-limit cohort only.
    pub gas_delta_sum_succeeding: i64,
}

/// Counts per bucket for one (schedule, block, `block_hash`). Always emitted
/// regardless of divergence count so coverage joins work even for fully-
/// matching blocks.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockCoverageRow {
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub tx_count: u32,
    /// Execution-fact split (sum == `tx_count`): byte-identical, gas-only, and
    /// everything-else (a per-tx `divergences` row was stored).
    pub tx_count_unchanged: u32,
    pub tx_count_gas_only: u32,
    pub tx_count_stored: u32,
    /// Native block header `gas_used` — the actual gas the block consumed on
    /// mainnet (the baseline for utilization and the repricing-tax fraction).
    pub block_gas_used: u64,
    /// Native block header `gas_limit` — the protocol cap, for block-fullness
    /// and gas-limit-exceedance analysis under a repricing schedule.
    pub block_gas_limit: u64,
    /// Native block header `base_fee_per_gas`. `None` for pre-London blocks.
    /// With the per-tx fee caps in [`TxGasResultRow`] this completes the
    /// fee-market picture without an external per-tx fee source.
    #[serde(default)]
    pub block_base_fee_per_gas: Option<u64>,
}

/// Per-opcode totals for one (block, bucket) row in `block_summaries`,
/// emitted as JSON array. Sparse — only opcodes that actually executed
/// in this bucket on this block appear.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpcodeBucketTotal {
    pub opcode: u8,
    /// Total executions of this opcode across every tx in the bucket
    /// for this block (summed across every call frame).
    pub count: u64,
    /// Sum of baseline gas cost (what the original schedule charged).
    pub gas_baseline: u64,
    /// Sum of schedule gas cost (what the replay schedule charged).
    /// Subtract `gas_baseline` for the per-opcode delta the schedule
    /// introduced.
    pub gas_schedule: u64,
}

/// Aggregate summary for one (schedule, block, class).
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockSummaryRow {
    pub schedule_name: String,
    pub block_number: u64,
    pub class: AggregateClass,
    pub tx_count: u32,
    pub gas_delta_sum: Option<i64>,
    /// Sum of squared `gas_delta` for the bucket. Stored under `SQLite`'s
    /// REAL column type, so values past 2^53 lose precision — fine for the
    /// approximate variance/stddev the dashboard renders.
    pub gas_delta_sum_sq: Option<i64>,
    pub gas_delta_min: Option<i64>,
    pub gas_delta_max: Option<i64>,
    /// 12-bin log2 histogram of `abs(gas_delta)`. `None` for buckets where
    /// no gas delta makes sense. Serialized as JSON.
    pub gas_delta_log2_hist: Option<[i32; 12]>,
    /// Per-opcode totals across this class in this block. Empty when
    /// no opcodes ran (e.g. the aggregator has nothing to fold in).
    /// JSON-encoded in the DB as a sparse list.
    pub opcode_totals: Vec<OpcodeBucketTotal>,
    pub state_gas_sum: Option<u64>,
    pub state_gas_spillover_sum: Option<u64>,
    pub multiplier_log2_hist: Option<[i32; 12]>,
    pub tx_count_creation: Option<u32>,
    pub tx_count_authorization: Option<u32>,
    pub tx_count_runtime_state: Option<u32>,
    pub tx_count_no_state: Option<u32>,
    /// Cold account accesses in this bucket (account-access opcodes). Populated
    /// for every schedule; `None` when the bucket made no cold account access.
    pub cold_account_access_count: Option<u64>,
    /// EIP-8038 storage-reprice drivers (F8) summed over this class's txs.
    /// `None` when the class saw no SLOAD/SSTORE activity.
    pub storage_drivers: Option<StorageDrivers>,
    /// Account-side gas drivers (F2/F3) summed over this class's txs. `None`
    /// when the class saw none of these (warm account access / value transfer /
    /// CREATE / access-list entries).
    pub account_drivers: Option<AccountDrivers>,
    /// v11 EIP-2718 type counts over the class's txs. Always `Some` from the
    /// producer (`Some(0)` = measured zero); `serde(default)` keeps pre-v11
    /// outbox envelopes decodable. The six sum to `tx_count`.
    #[serde(default)]
    pub tx_count_type_legacy: Option<u32>,
    #[serde(default)]
    pub tx_count_type_access_list: Option<u32>,
    #[serde(default)]
    pub tx_count_type_dynamic_fee: Option<u32>,
    #[serde(default)]
    pub tx_count_type_blob: Option<u32>,
    #[serde(default)]
    pub tx_count_type_set_code: Option<u32>,
    /// Any EIP-2718 type outside 0..=4, so the type counts stay a partition
    /// under future tx types.
    #[serde(default)]
    pub tx_count_type_other: Option<u32>,
    /// Non-create txs with empty calldata (the destination may still be a
    /// contract — envelope-shape classification only). Partition law:
    /// `tx_count_creation + tx_count_simple_transfer + tx_count_contract_call
    /// == tx_count`.
    #[serde(default)]
    pub tx_count_simple_transfer: Option<u32>,
    /// Non-create txs with non-empty calldata.
    #[serde(default)]
    pub tx_count_contract_call: Option<u32>,
    /// 13-bin closed-left histogram of `100 * gas_delta / baseline_gas_used`
    /// with edges `[-100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf)`.
    /// Every aggregated tx is binned, so the bin sum equals `tx_count`.
    /// Serialized as JSON.
    #[serde(default)]
    pub gas_delta_pct_hist: Option<[i32; 13]>,
    /// Sum of baseline gas used over the class's txs — the class-grain
    /// denominator for ratio-of-sums (`gas_delta_sum / baseline_gas_used_sum`).
    #[serde(default)]
    pub baseline_gas_used_sum: Option<u64>,
}

/// Per-tx drill-in record: the `divergences` row plus its dependent
/// `divergence_call_frames`, `divergence_opcode_counts`, and
/// `divergence_event_logs` rows. Emitted for every stored tx (a failure or a
/// trace divergence — see `DivergenceFacts::store_full_forensics`).
///
/// Call-frame and opcode-count rows carry a `trace_kind` (`"schedule"` /
/// `"baseline"`), mirroring the event-log table. The `baseline_*` vecs are
/// populated only when the call tree diverged (where the baseline path differs
/// and is worth keeping); otherwise they're empty and only schedule rows land.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DrillInRecord {
    pub divergence: DivergenceRow,
    pub call_frames: Vec<CallFrameRow>,
    /// Per-frame opcode counts. The producer omits zero-count rows; this
    /// vec is what `FrameOpcodeCounts::nonzero()` yielded for the tx.
    pub opcode_counts: Vec<OpcodeCountRow>,
    /// Baseline-side call frames (F15) — populated only on call-tree
    /// divergence, written with `trace_kind="baseline"`.
    pub baseline_call_frames: Vec<CallFrameRow>,
    /// Baseline-side per-opcode counts (F11) — populated only on call-tree
    /// divergence, written with `trace_kind="baseline"`.
    pub baseline_opcode_counts: Vec<OpcodeCountRow>,
    pub baseline_event_logs: Vec<EventLog>,
    pub schedule_event_logs: Vec<EventLog>,
}

/// One row destined for `divergences`. Most fields mirror the column
/// names; the `schema_version` / `divergence_id` / timestamp are filled in by
/// the writer.
///
/// `Default` is derived so test/example fixtures can spread `..Default::default()`
/// and set only the fields they care about — new nullable forensic columns then
/// need no fixture edits.
#[allow(missing_docs)]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DivergenceRow {
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub tx_index: u32,
    pub tx_hash: B256,
    pub timestamp: u64,
    /// Execution-fact witness: the schedule replay succeeded only at a bumped
    /// tier while exceeding the original gas limit (and matched baseline
    /// behavior) — wallet-fixable-by-more-gas. `None` when not applicable.
    pub outer_limit_only_failure: Option<bool>,

    pub sender: Address,
    pub recipient: Option<Address>,
    pub is_create: bool,
    pub tx_gas_limit: u64,

    pub baseline_success: bool,
    pub schedule_success: bool,
    /// Denormalized `baseline_success != schedule_success`. Stored (not derived
    /// at query time) so the common "did the outcome flip" filter is a single
    /// indexed column read on the query hot-path.
    pub status_changed: bool,
    pub event_logs_changed: bool,
    pub output_changed: bool,
    pub logs_bloom_changed: bool,

    pub baseline_gas_used: u64,
    pub schedule_gas_used: u64,
    /// Denormalized `schedule_gas_used - baseline_gas_used`, kept for the query
    /// hot-path (the dominant repricing aggregate) rather than recomputed.
    pub gas_delta: i64,
    pub baseline_total_gas_spent: Option<u64>,
    pub baseline_gas_refunded: Option<u64>,
    pub schedule_total_gas_spent: Option<u64>,
    pub schedule_gas_refunded: Option<u64>,
    pub schedule_intrinsic_gas: Option<u64>,
    pub schedule_floor_gas: Option<u64>,
    pub min_multiplier_to_succeed: Option<f64>,

    pub divergence_contract: Option<Address>,
    pub divergence_pc: Option<u32>,
    pub divergence_call_depth: Option<i32>,
    pub divergence_opcode: Option<u8>,
    pub oog_contract: Option<Address>,
    pub oog_pc: Option<u32>,
    pub oog_call_depth: Option<i32>,
    pub oog_opcode: Option<u8>,
    pub oog_pattern: Option<String>,
    pub oog_gas_remaining: Option<u64>,
    pub oog_chain_proportional: Option<bool>,
    pub oog_bottleneck_depth: Option<i32>,
    pub oog_bottleneck_kind: Option<String>,

    pub schedule_state_gas_spent: Option<u64>,
    /// State gas the tx *attempted* (incl. a charge that OOG'd). Nonzero even
    /// when `schedule_state_gas_spent` is 0 because the state op ran out of gas.
    pub schedule_state_gas_demanded: Option<u64>,
    pub schedule_initial_state_gas: Option<u64>,
    pub schedule_initial_reservoir: Option<u64>,
    pub runtime_state_gas: Option<u64>,
    pub runtime_state_gas_spillover: Option<u64>,
    pub state_gas_category: Option<String>,
    pub reservoir_exhausted: Option<bool>,

    /// Outcome of the schedule replay's *final* attempted multiplier
    /// tier. `Some(true)` when the highest tier halted OOG (true min
    /// multiplier exceeds the ceiling); `Some(false)` for non-gas
    /// halts/reverts; `None` when at least one tier succeeded.
    pub replay_halt_oog: Option<bool>,

    /// Cold account accesses in this tx (account-access opcodes). Populated for
    /// every schedule; `None` when the tx made no cold account access, or when
    /// the schedule replay was rejected before classification completed.
    pub cold_account_access_count: Option<u64>,

    /// Total repricing surcharge the schedule charged this tx across all frames
    /// (`ScheduleResult::additional_gas` — the summed per-opcode deltas). The
    /// headline "how much more gas did this schedule cost". `None` on the
    /// reject path (replay never executed). Signed: schedules that only reduce
    /// costs go negative. (F4)
    pub additional_gas_charged: Option<i64>,
    /// Root→divergence function-selector path: the 4-byte selector of each call
    /// on the stack from the entry frame down to the divergence frame, JSON
    /// `["0x12345678", null, …]` (null = a frame with no selector, e.g. a value
    /// transfer or create). `None` when no divergence location was resolved.
    /// (F6)
    pub failure_selector_path: Option<String>,

    /// EIP-2718 transaction type byte (0 legacy, 1 EIP-2930, 2 EIP-1559,
    /// 3 EIP-4844 blob, 4 EIP-7702). (F5)
    pub tx_type: Option<u8>,
    /// Sender account nonce of this tx. (F5)
    pub tx_nonce: Option<u64>,
    /// Top-level 4-byte entry selector (first 4 calldata bytes). `None` for
    /// creations and calls with fewer than 4 calldata bytes. (F5)
    pub entry_selector: Option<[u8; 4]>,
    /// Calldata zero / nonzero byte split — the intrinsic-gas drivers and a
    /// cheap calldata-shape fingerprint. (F5)
    pub input_zero_bytes: Option<u64>,
    pub input_nonzero_bytes: Option<u64>,
    /// Whether the tx carries an EIP-7702 authorization list. (F5)
    pub has_authorization: Option<bool>,

    /// Structured failure reason: the `HaltReason` discriminant (`OutOfGas`,
    /// `StackOverflow`, …), `"Revert"`, `"Rejected"`, or `None` on success.
    /// (F1)
    pub failure_reason: Option<String>,
    /// Raw revert returndata, truncated to a bounded prefix. `None` unless the
    /// schedule result was a revert. (F2)
    pub revert_data: Option<Vec<u8>>,
    /// Best-effort decode of `revert_data` (`Error(string): …` / `Panic(0xNN)` /
    /// `custom:0x…` / `empty`). (F2)
    pub revert_decoded: Option<String>,
    /// Top-level tx output bytes, truncated to a bounded prefix (today only a
    /// hash is otherwise kept). (F2)
    pub tx_output: Option<Vec<u8>>,

    /// Baseline counterpart of the frame that failed under the schedule (matched
    /// by `call_index` when the pre-divergence structure lines up): whether
    /// baseline ran that frame successfully, and its gas used / provided. The
    /// wallet-fixable (baseline ran it fine) vs contract-broken discriminator.
    /// `None` when no failing frame matched a baseline frame. (F7)
    pub baseline_frame_success: Option<bool>,
    pub baseline_frame_gas_used: Option<u64>,
    pub baseline_frame_gas_provided: Option<u64>,

    /// Cumulative repricing surcharge applied at the instant OOG was first
    /// recorded — the absolute gas deficit the repricing introduced. `None` for
    /// non-OOG rows. (F13)
    pub surcharge_at_oog: Option<i64>,

    /// First-gas-divergence location (F10): contract / pc / call depth / opcode
    /// where the cumulative surcharge first exceeded baseline. Distinct from the
    /// behavioral `divergence_*` columns (a schedule with negative deltas, e.g.
    /// CSV pricing, can reprice an opcode before its first net surcharge).
    pub gas_div_contract: Option<Address>,
    pub gas_div_pc: Option<u32>,
    pub gas_div_call_depth: Option<i32>,
    pub gas_div_opcode: Option<u8>,

    /// Per-category decomposition of the repricing tax (F12). The opcode-delta
    /// categories (`second_db_read` / `other`) sum to `additional_gas_charged`;
    /// `tax_intrinsic` is the separate tx-level intrinsic-gas delta
    /// (schedule − baseline). `None` on the reject path.
    pub tax_second_db_read: Option<i64>,
    pub tax_other: Option<i64>,
    pub tax_intrinsic: Option<i64>,

    /// EIP-8038 storage-reprice drivers (F8) — the eight SLOAD/SSTORE counts,
    /// mapped to explicit columns at insert time. `None` on the reject path
    /// (the replay never executed, so the counts are unknown, not zero).
    pub storage_drivers: Option<StorageDrivers>,

    /// Account-side gas drivers (F2/F3) — warm account accesses, value
    /// transfers, CREATE+CREATE2, and the tx access-list counts, mapped to
    /// explicit columns at insert time. `None` on the reject path.
    pub account_drivers: Option<AccountDrivers>,

    /// F1: forensics for the mainnet-equivalent (1×) tier when this tx FAILED
    /// at 1× but the accepted attempt is a bumped tier (rescue) or the highest
    /// tier (fail-under-both). The `oog_*`/frame figures above describe the
    /// accepted attempt; these preserve where/why 1× broke. All `None` when 1×
    /// succeeded or the tx wasn't a 1× failure.
    pub tier1_failure_reason: Option<String>,
    pub tier1_oog_opcode: Option<u8>,
    pub tier1_oog_contract: Option<Address>,
    pub tier1_oog_pc: Option<u32>,
    pub tier1_oog_depth: Option<i32>,
    pub tier1_oog_gas_remaining: Option<u64>,
    pub tier1_failing_selector: Option<[u8; 4]>,
    pub tier1_failing_gas_provided: Option<u64>,
    pub tier1_failing_gas_requested: Option<u64>,
}

/// One frame row destined for `divergence_call_frames`. `call_index` and
/// `parent_call_index` match the inspector's frame-open order (root = 0).
///
/// `Default` is derived for the same fixture-spread convenience as
/// [`DivergenceRow`].
#[allow(missing_docs)]
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CallFrameRow {
    pub call_index: u32,
    pub parent_call_index: Option<u32>,
    pub depth: u32,
    pub from_address: Address,
    pub to_address: Address,
    pub code_address: Option<Address>,
    pub codehash: Option<B256>,
    pub call_type: String,
    pub selector: Option<[u8; 4]>,
    pub value_wei: Option<String>,
    pub gas_provided: u64,
    pub gas_used: u64,
    /// Denormalized `gas_provided - gas_used`, kept for the query hot-path
    /// rather than recomputed per row.
    pub gas_margin: Option<i64>,
    pub success: bool,
    pub parent_gas_at_call: Option<u64>,
    pub gas_requested_on_stack: Option<u64>,
    pub eip150_cap_binding: Option<bool>,
    /// Length of the deployed bytecode in bytes. Set only on successful
    /// CREATE / CREATE2 frames (`call_type` is `CREATE`/`CREATE2` and
    /// `success` is true); `None` everywhere else. Used by the EIP-8037
    /// dashboard to plot true deployment cost without approximating
    /// bytecode size from baseline gas.
    pub deployed_bytecode_len: Option<u32>,
    /// Repricing surcharge the schedule charged within this frame
    /// (`CallFrame::repricing_gas_delta` — the summed per-opcode deltas applied
    /// while this frame was on top of the stack). Lets the dashboard attribute
    /// the per-tx tax to the frame that incurred it. Signed. (F4)
    pub repricing_gas_delta: i64,

    /// Caller PC of the CALL/CREATE that opened this frame (F9). `None` for the
    /// root and baseline frames.
    pub caller_pc: Option<u32>,
    /// Whether this frame was served by a precompile (F9).
    pub was_precompile: bool,
    /// Precompile address when `was_precompile` (F9), else `None`.
    pub precompile_address: Option<Address>,
    /// Gas remaining at the instant this frame failed (F9). `None` unless this
    /// is a failing schedule frame.
    pub gas_remaining_at_fail: Option<u64>,
    /// Whether this is the innermost failing frame under the schedule — the
    /// bottleneck (F9). False for baseline rows and non-failing frames.
    pub is_divergent_frame: bool,
}

/// One row destined for `divergence_opcode_counts`. Producer omits zero-
/// count rows.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OpcodeCountRow {
    pub call_index: u32,
    pub opcode: u8,
    pub count: u64,
    pub gas_baseline: u64,
    pub gas_schedule: u64,
}

impl OpcodeCountRow {
    /// Build a vec of per-(frame, opcode) rows from the inspector's
    /// `FrameOpcodeCounts`. Zero-count opcodes are skipped via
    /// `FrameOpcodeCounts::nonzero`, producing the sparse representation
    /// the schema expects.
    pub fn from_frames(frames: &[FrameOpcodeCounts]) -> Vec<Self> {
        let mut out = Vec::new();
        for frame in frames {
            for (opcode, count, gas_baseline, gas_schedule) in frame.nonzero() {
                out.push(Self {
                    call_index: frame.call_index,
                    opcode,
                    count,
                    gas_baseline,
                    gas_schedule,
                });
            }
        }
        out
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Write API
// ─────────────────────────────────────────────────────────────────────────────

impl DivergenceDatabase {
    /// Persist all per-block state in a single transaction so the
    /// coverage row, summaries, and drill-in records land together. Any
    /// failure leaves the DB untouched.
    ///
    /// Every `CHECKPOINT_EVERY_N_BLOCKS` calls, we also run
    /// `PRAGMA wal_checkpoint(RESTART)` to bound the WAL file size.
    /// RESTART (not TRUNCATE) lets the checkpoint recycle the WAL even
    /// when a long-lived reader is attached (e.g. the dashboard
    /// holding a `DuckDB` `sqlite_scanner` session); TRUNCATE would block
    /// until the reader released. We don't error on a failed
    /// checkpoint — it's best-effort and the next call will retry.
    pub fn record_block_output(&self, output: &BlockOutput) -> Result<(), DatabaseError> {
        self.record_block_output_inner(output, None)
    }

    /// Persist per-block state AND enqueue a durable export-outbox row in the
    /// same transaction, so a crash can never leave the analytical rows durable
    /// while losing the export request. The `export` envelope is pre-encoded
    /// (JSON → ZSTD) by the caller before the connection lock is taken.
    ///
    /// Idempotent on the deterministic `export_id`: re-recording an identical
    /// payload is a no-op that preserves an already-`exported` state; a changed
    /// payload for the same ID is an invariant violation that resets the row to
    /// `pending` and logs a warning.
    pub fn record_block_output_with_export(
        &self,
        output: &BlockOutput,
        export: &EncodedExportEnvelope,
    ) -> Result<(), DatabaseError> {
        self.record_block_output_inner(output, Some(export))
    }

    /// Shared transaction body for both public record methods. The outbox row,
    /// when present, lands in the same transaction as coverage/summaries/
    /// divergences.
    fn record_block_output_inner(
        &self,
        output: &BlockOutput,
        export: Option<&EncodedExportEnvelope>,
    ) -> Result<(), DatabaseError> {
        let mut conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let tx = conn.transaction()?;

        insert_block_coverage(&tx, &output.coverage)?;
        for summary in &output.summaries {
            insert_block_summary(&tx, summary)?;
        }
        for recipient in &output.recipients {
            insert_recipient(&tx, recipient)?;
        }
        for tx_gas_result in &output.tx_gas_results {
            insert_tx_gas_result(&tx, tx_gas_result)?;
        }
        for drill_in in &output.drill_ins {
            let divergence_id = insert_divergence(&tx, &drill_in.divergence)?;
            for frame in &drill_in.call_frames {
                insert_call_frame(&tx, divergence_id, "schedule", frame)?;
            }
            for frame in &drill_in.baseline_call_frames {
                insert_call_frame(&tx, divergence_id, "baseline", frame)?;
            }
            for opc in &drill_in.opcode_counts {
                insert_opcode_count(&tx, divergence_id, "schedule", opc)?;
            }
            for opc in &drill_in.baseline_opcode_counts {
                insert_opcode_count(&tx, divergence_id, "baseline", opc)?;
            }
            for log in &drill_in.baseline_event_logs {
                insert_event_log(&tx, divergence_id, "baseline", log)?;
            }
            for log in &drill_in.schedule_event_logs {
                insert_event_log(&tx, divergence_id, "schedule", log)?;
            }
        }
        if let Some(export) = export {
            insert_export_outbox(&tx, &output.coverage, export)?;
        }
        tx.commit()?;

        // Periodic explicit checkpoint. Reset the counter on every
        // checkpoint attempt — even if the checkpoint partial-completes
        // (reader holding WAL pages), we don't want to thrash the file
        // on every block.
        if self.blocks_since_checkpoint.fetch_add(1, Ordering::Relaxed) + 1 >=
            CHECKPOINT_EVERY_N_BLOCKS
        {
            self.blocks_since_checkpoint.store(0, Ordering::Relaxed);
            // Best-effort. The pragma returns three counters
            // (busy, log, checkpointed); we ignore them. If the
            // checkpoint fails or partials, the next iteration will
            // try again.
            let _ = conn.pragma_update(None, "wal_checkpoint", "RESTART");
        }

        Ok(())
    }

    /// Delete all rows in the inclusive block range from every per-block
    /// table. Called on chain reorg / revert to drop stale data before
    /// re-analysing the new tip.
    ///
    /// Deletes cascade through `divergences → call_frames → opcode_counts
    /// → event_logs` by explicit DELETE statements rather than DB-side
    /// CASCADE, so we keep the row counts visible for logging.
    pub fn delete_block_range(
        &self,
        from_block: u64,
        to_block: u64,
    ) -> Result<BlockRangeDeleteCounts, DatabaseError> {
        let mut conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let tx = conn.transaction()?;

        let event_logs_deleted = tx.execute(
            "DELETE FROM divergence_event_logs
             WHERE divergence_id IN
                 (SELECT divergence_id FROM divergences
                  WHERE block_number BETWEEN ? AND ?)",
            params![from_block as i64, to_block as i64],
        )?;
        let opcode_counts_deleted = tx.execute(
            "DELETE FROM divergence_opcode_counts
             WHERE divergence_id IN
                 (SELECT divergence_id FROM divergences
                  WHERE block_number BETWEEN ? AND ?)",
            params![from_block as i64, to_block as i64],
        )?;
        let call_frames_deleted = tx.execute(
            "DELETE FROM divergence_call_frames
             WHERE divergence_id IN
                 (SELECT divergence_id FROM divergences
                  WHERE block_number BETWEEN ? AND ?)",
            params![from_block as i64, to_block as i64],
        )?;
        let divergences_deleted = tx.execute(
            "DELETE FROM divergences WHERE block_number BETWEEN ? AND ?",
            params![from_block as i64, to_block as i64],
        )?;
        let tx_gas_results_deleted = tx.execute(
            "DELETE FROM tx_gas_results WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        let summaries_deleted = tx.execute(
            "DELETE FROM block_summaries WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        let recipients_deleted = tx.execute(
            "DELETE FROM block_recipients WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        let coverage_deleted = tx.execute(
            "DELETE FROM block_coverage WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        // Drop not-yet-exported outbox rows for the reverted range in the same
        // transaction. Already-`exported` rows are preserved as an audit trail;
        // their ClickHouse rows are not retracted in v1 (live-head reorg
        // tombstones are out of scope — see the export design doc).
        let outbox_deleted = tx.execute(
            "DELETE FROM export_outbox
             WHERE block_number >= ? AND block_number <= ?
               AND state IN ('pending', 'retry')",
            params![from_block as i64, to_block as i64],
        )?;
        tx.commit()?;

        Ok(BlockRangeDeleteCounts {
            coverage: coverage_deleted,
            summaries: summaries_deleted,
            recipients: recipients_deleted,
            tx_gas_results: tx_gas_results_deleted,
            divergences: divergences_deleted,
            call_frames: call_frames_deleted,
            opcode_counts: opcode_counts_deleted,
            event_logs: event_logs_deleted,
            outbox_pending: outbox_deleted,
        })
    }

    /// Whether any divergence row exists for `(schedule_name,
    /// block_number, schedule_config_hash)`. Used by the backfill arm to
    /// skip blocks already covered under the current configuration.
    pub fn has_block_coverage_with_config(
        &self,
        schedule_name: &str,
        block_number: u64,
        schedule_config_hash: &str,
    ) -> Result<bool, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM block_coverage
                 WHERE schedule_name = ?
                   AND block_number = ?
                   AND schedule_config_hash = ?
                 LIMIT 1",
                params![schedule_name, block_number as i64, schedule_config_hash],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// UPSERT a parsed `contract_metadata` row keyed by codehash. Used by
    /// the `contract-metadata-backfill` subcommand to record solc version /
    /// metadata-hash presence per deployed contract. Idempotent.
    pub fn upsert_contract_metadata(
        &self,
        codehash: [u8; 32],
        representative_address: &str,
        bytecode_len: u64,
        metadata: &crate::contract_metadata::ContractMetadata,
        extracted_at_unix: u64,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "INSERT INTO contract_metadata (
                codehash, representative_address, solc_version, solc_commit,
                evm_target, cbor_present, has_metadata_hash,
                bytecode_len, extracted_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (codehash) DO UPDATE SET
                representative_address = excluded.representative_address,
                solc_version           = excluded.solc_version,
                solc_commit            = excluded.solc_commit,
                evm_target             = excluded.evm_target,
                cbor_present           = excluded.cbor_present,
                has_metadata_hash      = excluded.has_metadata_hash,
                bytecode_len           = excluded.bytecode_len,
                extracted_at           = excluded.extracted_at",
            params![
                codehash.as_slice(),
                representative_address,
                metadata.solc_version,
                metadata.solc_commit,
                metadata.evm_target,
                metadata.cbor_present,
                metadata.has_metadata_hash,
                bytecode_len as i64,
                extracted_at_unix as i64,
            ],
        )?;
        Ok(())
    }

    /// Whether a `contract_metadata` row already exists for this codehash.
    pub fn has_contract_metadata(&self, codehash: [u8; 32]) -> Result<bool, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM contract_metadata WHERE codehash = ? LIMIT 1",
                params![codehash.as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    /// Distinct `code_address` values seen across `divergence_call_frames`.
    /// Backfill iterates these, fetches bytecode from reth state, hashes
    /// it, and upserts a `contract_metadata` row keyed by codehash.
    ///
    /// Keyed on `code_address` (the code holder / implementation), not
    /// `to_address` (the call/storage target): under DELEGATECALL the solc
    /// identity belongs to the implementation, so a proxy `to_address` would
    /// resolve the wrong (or empty) bytecode. (F14)
    pub fn distinct_call_frame_addresses(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT code_address FROM divergence_call_frames
             WHERE code_address IS NOT NULL AND code_address <> ''
             ORDER BY code_address",
        )?;
        let rows =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Code addresses with at least one call frame whose codehash is not yet
    /// represented in `contract_metadata`. Drives the periodic incremental
    /// backfill so each tick only re-fetches bytecode for genuinely new
    /// (or never-labeled) contracts. NULL `codehash` values match too —
    /// those rows need a fresh fetch to derive a codehash from current
    /// state. Keyed on `code_address` (the implementation) — see
    /// [`Self::distinct_call_frame_addresses`]. (F14)
    pub fn distinct_unlabeled_call_frame_addresses(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT dcf.code_address
             FROM divergence_call_frames dcf
             LEFT JOIN contract_metadata cm ON cm.codehash = dcf.codehash
             WHERE dcf.code_address IS NOT NULL AND dcf.code_address <> ''
               AND cm.codehash IS NULL
             ORDER BY dcf.code_address",
        )?;
        let rows =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Distinct addresses seen as either `to_address` or `code_address` in
    /// `divergence_call_frames` that don't yet have a `contract_labels`
    /// row. `UNION`ing in `code_address` catches implementations of proxy
    /// contracts that are never directly called but appear in the
    /// delegate-call code position. Drives the external-label backfill.
    pub fn distinct_unlabeled_addresses_for_labels(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT addr FROM (
               SELECT to_address   AS addr FROM divergence_call_frames
                 WHERE to_address   IS NOT NULL AND to_address   <> ''
               UNION
               SELECT code_address AS addr FROM divergence_call_frames
                 WHERE code_address IS NOT NULL AND code_address <> ''
             ) a
             WHERE NOT EXISTS (
               SELECT 1 FROM contract_labels cl WHERE cl.address = a.addr
             )
             ORDER BY addr",
        )?;
        let rows =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// UPSERT a `contract_labels` row. Idempotent on `address`. `source`
    /// records which fallback rung (`blockscout` / `sourcify` /
    /// `etherscan` / `none`) actually populated the row so the consumer
    /// can attribute coverage. A `none` row with all NULL fields is
    /// written when every source missed, so future ticks skip the
    /// address.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_contract_label(
        &self,
        address: &str,
        contract_name: Option<&str>,
        protocol_tag: Option<&str>,
        is_proxy: bool,
        impl_address: Option<&str>,
        source: &str,
        fetched_at_unix: u64,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "INSERT INTO contract_labels (
                address, contract_name, protocol_tag, is_proxy,
                impl_address, source, fetched_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT (address) DO UPDATE SET
                contract_name = excluded.contract_name,
                protocol_tag  = excluded.protocol_tag,
                is_proxy      = excluded.is_proxy,
                impl_address  = excluded.impl_address,
                source        = excluded.source,
                fetched_at    = excluded.fetched_at",
            params![
                address,
                contract_name,
                protocol_tag,
                if is_proxy { 1i64 } else { 0i64 },
                impl_address,
                source,
                fetched_at_unix as i64,
            ],
        )?;
        Ok(())
    }

    /// Distinct call-frame selectors not yet present in
    /// `function_signatures`. Returned as 4-byte vectors so the caller
    /// can format them as hex for the lookup API without re-querying.
    pub fn distinct_unresolved_selectors(&self) -> Result<Vec<Vec<u8>>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT dcf.selector
             FROM divergence_call_frames dcf
             LEFT JOIN function_signatures fs ON fs.selector = dcf.selector
             WHERE dcf.selector IS NOT NULL
               AND fs.selector IS NULL
             ORDER BY dcf.selector",
        )?;
        let rows =
            stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// UPSERT a `function_signatures` row. `signature` is `NULL` when the
    /// lookup ran but found no match — recording the miss prevents the
    /// orchestrator from re-querying the same dead selectors every tick.
    pub fn upsert_function_signature(
        &self,
        selector: &[u8],
        signature: Option<&str>,
        source: &str,
        fetched_at_unix: u64,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "INSERT INTO function_signatures (selector, signature, source, fetched_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT (selector) DO UPDATE SET
                signature  = excluded.signature,
                source     = excluded.source,
                fetched_at = excluded.fetched_at",
            params![selector, signature, source, fetched_at_unix as i64],
        )?;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Export outbox: types
// ─────────────────────────────────────────────────────────────────────────────

/// A pre-encoded export payload ready to be enqueued in the same transaction as
/// a block's analytical rows. Produced by `export/model.rs` on the writer
/// thread before the `SQLite` lock is taken. The block coordinates
/// (`schedule_name`, `block_number`, `block_hash`) come from the accompanying
/// [`BlockCoverageRow`]; this struct carries only the deterministic identity and
/// the compressed payload.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct EncodedExportEnvelope {
    pub export_id: String,
    pub analysis_config_hash: String,
    pub schedule_config_hash: String,
    pub payload_version: u16,
    pub payload_zstd: Vec<u8>,
    pub payload_hash: B256,
    pub payload_bytes: usize,
}

/// Immutable per-dataset descriptor stored in `analysis_manifests`. Persisted
/// once per `analysis_config_hash` so a pending outbox row can be exported under
/// the manifest it was produced with, even after a restart with a different CLI
/// configuration.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisManifestRecord {
    pub analysis_config_hash: String,
    pub schema_version: u32,
    pub chain_id: u64,
    pub producer_git_commit: String,
    pub replay_semantics: String,
    pub manifest_json: String,
}

/// A due outbox row returned by [`DivergenceDatabase::next_due_export`].
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub export_id: String,
    pub analysis_config_hash: String,
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: B256,
    pub payload_version: u16,
    pub payload_zstd: Vec<u8>,
    pub payload_hash: B256,
    pub payload_bytes: usize,
    pub attempts: u32,
    pub created_at: u64,
}

/// Backlog health snapshot returned by
/// [`DivergenceDatabase::export_backlog_stats`], used for periodic logging and
/// the `max_pending_bytes` backpressure check.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportBacklogStats {
    /// Rows in `pending`/`retry` state (awaiting or retrying export).
    pub pending_count: u64,
    /// Sum of `payload_bytes` across pending/retry rows.
    pub pending_bytes: u64,
    /// Age in seconds of the oldest pending/retry row (`0` if none).
    pub oldest_pending_age_secs: u64,
    /// Rows permanently `blocked` (e.g. oversized payload).
    pub blocked_count: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Export outbox: API
// ─────────────────────────────────────────────────────────────────────────────

impl DivergenceDatabase {
    /// Insert or confirm the immutable manifest row for a dataset. Idempotent
    /// when the stored `manifest_json` matches; returns
    /// [`DatabaseError::ManifestConflict`] if a row with the same hash but
    /// different JSON already exists.
    pub fn upsert_analysis_manifest(
        &self,
        manifest: &AnalysisManifestRecord,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let existing: Option<String> = conn
            .query_row(
                "SELECT manifest_json FROM analysis_manifests WHERE analysis_config_hash = ?",
                params![manifest.analysis_config_hash],
                |row| row.get(0),
            )
            .optional()?;
        match existing {
            Some(json) if json == manifest.manifest_json => Ok(()),
            Some(_) => Err(DatabaseError::ManifestConflict {
                analysis_config_hash: manifest.analysis_config_hash.clone(),
            }),
            None => {
                conn.execute(
                    "INSERT INTO analysis_manifests (
                        analysis_config_hash, schema_version, chain_id,
                        producer_git_commit, replay_semantics, manifest_json, created_at
                     ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![
                        manifest.analysis_config_hash,
                        manifest.schema_version as i64,
                        manifest.chain_id as i64,
                        manifest.producer_git_commit,
                        manifest.replay_semantics,
                        manifest.manifest_json,
                        current_unix_seconds() as i64,
                    ],
                )?;
                Ok(())
            }
        }
    }

    /// Load the immutable manifest for a dataset identity, if present.
    pub fn analysis_manifest(
        &self,
        analysis_config_hash: &str,
    ) -> Result<Option<AnalysisManifestRecord>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let record = conn
            .query_row(
                "SELECT analysis_config_hash, schema_version, chain_id,
                        producer_git_commit, replay_semantics, manifest_json
                 FROM analysis_manifests WHERE analysis_config_hash = ?",
                params![analysis_config_hash],
                |row| {
                    Ok(AnalysisManifestRecord {
                        analysis_config_hash: row.get(0)?,
                        schema_version: row.get::<_, i64>(1)? as u32,
                        chain_id: row.get::<_, i64>(2)? as u64,
                        producer_git_commit: row.get(3)?,
                        replay_semantics: row.get(4)?,
                        manifest_json: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    /// Fetch the single most-due export item: the oldest `pending`/`retry` row
    /// whose `next_attempt_at` is at or before `now`. Returns `None` when the
    /// queue is empty or nothing is due yet. Never holds the lock across HTTP —
    /// the caller releases it before contacting `ClickHouse`.
    pub fn next_due_export(&self, now: u64) -> Result<Option<OutboxItem>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let item = conn
            .query_row(
                "SELECT export_id, analysis_config_hash, schedule_name, schedule_config_hash,
                        block_number, block_hash, payload_version, payload_zstd,
                        payload_hash, payload_bytes, attempts, created_at
                 FROM export_outbox
                 WHERE state IN ('pending', 'retry') AND next_attempt_at <= ?
                 ORDER BY next_attempt_at ASC, created_at ASC
                 LIMIT 1",
                params![now as i64],
                |row| {
                    let block_hash: Vec<u8> = row.get(5)?;
                    let payload_hash: Vec<u8> = row.get(8)?;
                    Ok(OutboxItem {
                        export_id: row.get(0)?,
                        analysis_config_hash: row.get(1)?,
                        schedule_name: row.get(2)?,
                        schedule_config_hash: row.get(3)?,
                        block_number: row.get::<_, i64>(4)? as u64,
                        block_hash: B256::from_slice(&block_hash),
                        payload_version: row.get::<_, i64>(6)? as u16,
                        payload_zstd: row.get(7)?,
                        payload_hash: B256::from_slice(&payload_hash),
                        payload_bytes: row.get::<_, i64>(9)? as usize,
                        attempts: row.get::<_, i64>(10)? as u32,
                        created_at: row.get::<_, i64>(11)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(item)
    }

    /// Drain up to `limit` most-due export items (oldest `pending`/`retry` with
    /// `next_attempt_at <= now`), same ordering as [`Self::next_due_export`]. The
    /// lock is released before the caller contacts `ClickHouse`.
    pub fn next_due_exports(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<OutboxItem>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT export_id, analysis_config_hash, schedule_name, schedule_config_hash,
                    block_number, block_hash, payload_version, payload_zstd,
                    payload_hash, payload_bytes, attempts, created_at
             FROM export_outbox
             WHERE state IN ('pending', 'retry') AND next_attempt_at <= ?
             ORDER BY next_attempt_at ASC, created_at ASC
             LIMIT ?",
        )?;
        let rows = stmt.query_map(params![now as i64, limit as i64], |row| {
            let block_hash: Vec<u8> = row.get(5)?;
            let payload_hash: Vec<u8> = row.get(8)?;
            Ok(OutboxItem {
                export_id: row.get(0)?,
                analysis_config_hash: row.get(1)?,
                schedule_name: row.get(2)?,
                schedule_config_hash: row.get(3)?,
                block_number: row.get::<_, i64>(4)? as u64,
                block_hash: B256::from_slice(&block_hash),
                payload_version: row.get::<_, i64>(6)? as u16,
                payload_zstd: row.get(7)?,
                payload_hash: B256::from_slice(&payload_hash),
                payload_bytes: row.get::<_, i64>(9)? as usize,
                attempts: row.get::<_, i64>(10)? as u32,
                created_at: row.get::<_, i64>(11)? as u64,
            })
        })?;
        let mut items = Vec::new();
        for item in rows {
            items.push(item?);
        }
        Ok(items)
    }

    /// Mark many outbox rows `exported` in one transaction (coverage confirmed).
    pub fn mark_exported_batch(
        &self,
        export_ids: &[String],
        exported_at: u64,
    ) -> Result<(), DatabaseError> {
        if export_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let tx = conn.transaction()?;
        {
            let now = current_unix_seconds() as i64;
            let mut stmt = tx.prepare(
                "UPDATE export_outbox
                 SET state = 'exported', exported_at = ?, last_error = NULL, updated_at = ?
                 WHERE export_id = ?",
            )?;
            for id in export_ids {
                stmt.execute(params![exported_at as i64, now, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Move many outbox rows to `retry` in one transaction, incrementing each
    /// row's `attempts` and scheduling the shared `next_attempt_at`.
    pub fn mark_export_retry_batch(
        &self,
        export_ids: &[String],
        next_attempt_at: u64,
        error: &str,
    ) -> Result<(), DatabaseError> {
        if export_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let tx = conn.transaction()?;
        {
            let now = current_unix_seconds() as i64;
            let err = truncate_error(error);
            let mut stmt = tx.prepare(
                "UPDATE export_outbox
                 SET state = 'retry', attempts = attempts + 1, next_attempt_at = ?,
                     last_error = ?, updated_at = ?
                 WHERE export_id = ?",
            )?;
            for id in export_ids {
                stmt.execute(params![next_attempt_at as i64, err, now, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Record a transient export failure: bump `attempts`, schedule the next
    /// attempt, store a shortened error, and move the row to `retry`.
    pub fn mark_export_retry(
        &self,
        export_id: &str,
        attempts: u32,
        next_attempt_at: u64,
        error: &str,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "UPDATE export_outbox
             SET state = 'retry', attempts = ?, next_attempt_at = ?,
                 last_error = ?, updated_at = ?
             WHERE export_id = ?",
            params![
                attempts as i64,
                next_attempt_at as i64,
                truncate_error(error),
                current_unix_seconds() as i64,
                export_id,
            ],
        )?;
        Ok(())
    }

    /// Mark an outbox row as successfully exported (coverage insert confirmed).
    pub fn mark_exported(&self, export_id: &str, exported_at: u64) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "UPDATE export_outbox
             SET state = 'exported', exported_at = ?, last_error = NULL, updated_at = ?
             WHERE export_id = ?",
            params![exported_at as i64, current_unix_seconds() as i64, export_id],
        )?;
        Ok(())
    }

    /// Mark an outbox row as permanently `blocked` (e.g. a single row exceeds
    /// the configured hard size limit). Blocked rows are not retried and are
    /// surfaced by [`Self::export_backlog_stats`].
    pub fn mark_export_blocked(&self, export_id: &str, error: &str) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.execute(
            "UPDATE export_outbox
             SET state = 'blocked', last_error = ?, updated_at = ?
             WHERE export_id = ?",
            params![truncate_error(error), current_unix_seconds() as i64, export_id],
        )?;
        Ok(())
    }

    /// Snapshot of outbox backlog health for logging and backpressure.
    pub fn export_backlog_stats(&self, now: u64) -> Result<ExportBacklogStats, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let (pending_count, pending_bytes, oldest_created, blocked_count): (
            i64,
            i64,
            Option<i64>,
            i64,
        ) = conn.query_row(
            "SELECT
                    COUNT(*) FILTER (WHERE state IN ('pending', 'retry')),
                    COALESCE(SUM(payload_bytes) FILTER (WHERE state IN ('pending', 'retry')), 0),
                    MIN(created_at) FILTER (WHERE state IN ('pending', 'retry')),
                    COUNT(*) FILTER (WHERE state = 'blocked')
                 FROM export_outbox",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let oldest_pending_age_secs =
            oldest_created.map(|c| now.saturating_sub(c as u64)).unwrap_or(0);
        Ok(ExportBacklogStats {
            pending_count: pending_count as u64,
            pending_bytes: pending_bytes as u64,
            oldest_pending_age_secs,
            blocked_count: blocked_count as u64,
        })
    }

    /// Delete `exported` rows whose `exported_at` is older than `cutoff`,
    /// bounding the audit-trail growth of the outbox. Returns the number pruned.
    pub fn prune_exported_before(&self, cutoff: u64) -> Result<usize, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let deleted = conn.execute(
            "DELETE FROM export_outbox WHERE state = 'exported' AND exported_at < ?",
            params![cutoff as i64],
        )?;
        Ok(deleted)
    }

    /// Read the `state` of an outbox row by `export_id`. Test-only accessor for
    /// worker tests in a sibling module that cannot reach the private connection.
    #[cfg(test)]
    pub(crate) fn export_state_for_test(&self, export_id: &str) -> Option<String> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        conn.query_row(
            "SELECT state FROM export_outbox WHERE export_id = ?",
            params![export_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None)
    }
}

/// Truncate an error message to a bounded length before storing it in the
/// outbox, so a pathological remote response body can't bloat the row.
fn truncate_error(error: &str) -> String {
    const MAX: usize = 500;
    if error.len() <= MAX {
        error.to_string()
    } else {
        let mut end = MAX;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &error[..end])
    }
}

/// Per-table row counts returned by
/// [`DivergenceDatabase::delete_block_range`]. Used for logging.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockRangeDeleteCounts {
    pub coverage: usize,
    pub summaries: usize,
    pub recipients: usize,
    pub tx_gas_results: usize,
    pub divergences: usize,
    pub call_frames: usize,
    pub opcode_counts: usize,
    pub event_logs: usize,
    /// Non-exported (`pending`/`retry`) export-outbox rows removed for the range.
    pub outbox_pending: usize,
}

fn insert_block_coverage(
    tx: &Transaction<'_>,
    row: &BlockCoverageRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO block_coverage (
            schedule_name, schedule_config_hash, block_number, block_hash,
            parent_hash, timestamp, tx_count,
            tx_count_unchanged, tx_count_gas_only, tx_count_stored,
            block_gas_used, block_gas_limit, block_base_fee_per_gas
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, block_hash) DO UPDATE SET
            schedule_config_hash   = excluded.schedule_config_hash,
            parent_hash            = excluded.parent_hash,
            timestamp              = excluded.timestamp,
            tx_count               = excluded.tx_count,
            tx_count_unchanged     = excluded.tx_count_unchanged,
            tx_count_gas_only      = excluded.tx_count_gas_only,
            tx_count_stored        = excluded.tx_count_stored,
            block_gas_used         = excluded.block_gas_used,
            block_gas_limit        = excluded.block_gas_limit,
            block_base_fee_per_gas = excluded.block_base_fee_per_gas",
        params![
            row.schedule_name,
            row.schedule_config_hash,
            row.block_number as i64,
            row.block_hash.as_slice(),
            row.parent_hash.as_slice(),
            row.timestamp as i64,
            row.tx_count as i64,
            row.tx_count_unchanged as i64,
            row.tx_count_gas_only as i64,
            row.tx_count_stored as i64,
            row.block_gas_used as i64,
            row.block_gas_limit as i64,
            row.block_base_fee_per_gas.map(|f| f as i64),
        ],
    )?;
    Ok(())
}

/// Insert one `tx_gas_results` row. Upsert on the natural key so an
/// idempotent re-replay of a block refreshes it in place.
fn insert_tx_gas_result(tx: &Transaction<'_>, row: &TxGasResultRow) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO tx_gas_results (
            schedule_name, schedule_config_hash, block_number, tx_index,
            tx_hash, tx_type, tx_gas_limit,
            max_fee_per_gas, max_priority_fee_per_gas,
            baseline_success, baseline_gas_used, baseline_total_gas_spent,
            schedule_success, schedule_gas_used, schedule_total_gas_spent,
            schedule_gas_refunded, schedule_floor_gas, schedule_state_gas_spent,
            schedule_intrinsic_gas, min_multiplier_to_succeed
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, tx_index) DO UPDATE SET
            schedule_config_hash     = excluded.schedule_config_hash,
            tx_hash                  = excluded.tx_hash,
            tx_type                  = excluded.tx_type,
            tx_gas_limit             = excluded.tx_gas_limit,
            max_fee_per_gas          = excluded.max_fee_per_gas,
            max_priority_fee_per_gas = excluded.max_priority_fee_per_gas,
            baseline_success         = excluded.baseline_success,
            baseline_gas_used        = excluded.baseline_gas_used,
            baseline_total_gas_spent = excluded.baseline_total_gas_spent,
            schedule_success         = excluded.schedule_success,
            schedule_gas_used        = excluded.schedule_gas_used,
            schedule_total_gas_spent = excluded.schedule_total_gas_spent,
            schedule_gas_refunded    = excluded.schedule_gas_refunded,
            schedule_floor_gas       = excluded.schedule_floor_gas,
            schedule_state_gas_spent = excluded.schedule_state_gas_spent,
            schedule_intrinsic_gas   = excluded.schedule_intrinsic_gas,
            min_multiplier_to_succeed = excluded.min_multiplier_to_succeed",
        params![
            row.schedule_name,
            row.schedule_config_hash,
            row.block_number as i64,
            row.tx_index as i64,
            row.tx_hash.as_slice(),
            row.tx_type as i64,
            row.tx_gas_limit as i64,
            row.max_fee_per_gas,
            row.max_priority_fee_per_gas,
            row.baseline_success as i64,
            row.baseline_gas_used as i64,
            row.baseline_total_gas_spent as i64,
            row.schedule_success as i64,
            row.schedule_gas_used as i64,
            row.schedule_total_gas_spent as i64,
            row.schedule_gas_refunded as i64,
            row.schedule_floor_gas as i64,
            row.schedule_state_gas_spent as i64,
            row.schedule_intrinsic_gas.map(|g| g as i64),
            row.min_multiplier_to_succeed,
        ],
    )?;
    Ok(())
}

/// Insert one `block_recipients` rollup row. Upsert on the natural key
/// so re-touching a block (idempotent re-replay) refreshes counts in place.
fn insert_recipient(tx: &Transaction<'_>, row: &RecipientRow) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO block_recipients (
            schedule_name, block_number, class, recipient, top_selector,
            tx_count, gas_delta_sum_succeeding
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, class, recipient, top_selector)
        DO UPDATE SET
            tx_count                 = excluded.tx_count,
            gas_delta_sum_succeeding = excluded.gas_delta_sum_succeeding",
        params![
            row.schedule_name,
            row.block_number as i64,
            row.class.as_str(),
            row.recipient,
            row.top_selector.as_slice(),
            row.tx_count as i64,
            row.gas_delta_sum_succeeding,
        ],
    )?;
    Ok(())
}

fn insert_block_summary(tx: &Transaction<'_>, row: &BlockSummaryRow) -> Result<(), DatabaseError> {
    // SQLite has no native arrays — we JSON-encode the histograms and
    // the sparse per-opcode struct list. gas_delta_sum_sq is bound as a
    // REAL (f64).
    let gas_delta_log2_hist = match row.gas_delta_log2_hist {
        Some(arr) => Some(serde_json::to_string(&arr)?),
        None => None,
    };
    let multiplier_log2_hist = match row.multiplier_log2_hist {
        Some(arr) => Some(serde_json::to_string(&arr)?),
        None => None,
    };
    let gas_delta_pct_hist = match row.gas_delta_pct_hist {
        Some(arr) => Some(serde_json::to_string(&arr)?),
        None => None,
    };
    let opcode_totals = serde_json::to_string(&row.opcode_totals)?;
    let gas_delta_sum_sq_real = row.gas_delta_sum_sq.map(|v| v as f64);

    tx.execute(
        "INSERT INTO block_summaries (
            schedule_name, block_number, class, tx_count,
            gas_delta_sum, gas_delta_sum_sq, gas_delta_min, gas_delta_max,
            gas_delta_log2_hist,
            opcode_totals,
            state_gas_sum, state_gas_spillover_sum,
            multiplier_log2_hist,
            tx_count_creation, tx_count_authorization,
            tx_count_runtime_state, tx_count_no_state,
            cold_account_access_count,
            sload_cold_count, sload_warm_count, sstore_cold_count,
            sstore_set_count, sstore_reset_count, sstore_clear_count,
            sstore_noop_count, sstore_dirty_count,
            warm_account_access_count, value_transfer_count, create_opcode_count,
            access_list_address_count, access_list_storage_key_count,
            tx_count_type_legacy, tx_count_type_access_list, tx_count_type_dynamic_fee,
            tx_count_type_blob, tx_count_type_set_code, tx_count_type_other,
            tx_count_simple_transfer, tx_count_contract_call,
            gas_delta_pct_hist, baseline_gas_used_sum
        ) VALUES (?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?,
                  ?,
                  ?, ?,
                  ?,
                  ?, ?, ?, ?,
                  ?,
                  ?, ?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?, ?,
                  ?, ?)
        ON CONFLICT (schedule_name, block_number, class) DO UPDATE SET
            tx_count                = excluded.tx_count,
            gas_delta_sum           = excluded.gas_delta_sum,
            gas_delta_sum_sq        = excluded.gas_delta_sum_sq,
            gas_delta_min           = excluded.gas_delta_min,
            gas_delta_max           = excluded.gas_delta_max,
            gas_delta_log2_hist     = excluded.gas_delta_log2_hist,
            opcode_totals           = excluded.opcode_totals,
            state_gas_sum           = excluded.state_gas_sum,
            state_gas_spillover_sum = excluded.state_gas_spillover_sum,
            multiplier_log2_hist    = excluded.multiplier_log2_hist,
            tx_count_creation       = excluded.tx_count_creation,
            tx_count_authorization  = excluded.tx_count_authorization,
            tx_count_runtime_state  = excluded.tx_count_runtime_state,
            tx_count_no_state       = excluded.tx_count_no_state,
            cold_account_access_count = excluded.cold_account_access_count,
            sload_cold_count    = excluded.sload_cold_count,
            sload_warm_count    = excluded.sload_warm_count,
            sstore_cold_count   = excluded.sstore_cold_count,
            sstore_set_count    = excluded.sstore_set_count,
            sstore_reset_count  = excluded.sstore_reset_count,
            sstore_clear_count  = excluded.sstore_clear_count,
            sstore_noop_count   = excluded.sstore_noop_count,
            sstore_dirty_count  = excluded.sstore_dirty_count,
            warm_account_access_count     = excluded.warm_account_access_count,
            value_transfer_count          = excluded.value_transfer_count,
            create_opcode_count           = excluded.create_opcode_count,
            access_list_address_count     = excluded.access_list_address_count,
            access_list_storage_key_count = excluded.access_list_storage_key_count,
            tx_count_type_legacy      = excluded.tx_count_type_legacy,
            tx_count_type_access_list = excluded.tx_count_type_access_list,
            tx_count_type_dynamic_fee = excluded.tx_count_type_dynamic_fee,
            tx_count_type_blob        = excluded.tx_count_type_blob,
            tx_count_type_set_code    = excluded.tx_count_type_set_code,
            tx_count_type_other       = excluded.tx_count_type_other,
            tx_count_simple_transfer  = excluded.tx_count_simple_transfer,
            tx_count_contract_call    = excluded.tx_count_contract_call,
            gas_delta_pct_hist        = excluded.gas_delta_pct_hist,
            baseline_gas_used_sum     = excluded.baseline_gas_used_sum",
        params![
            row.schedule_name,
            row.block_number as i64,
            row.class.as_str(),
            row.tx_count as i64,
            row.gas_delta_sum,
            gas_delta_sum_sq_real,
            row.gas_delta_min,
            row.gas_delta_max,
            gas_delta_log2_hist,
            opcode_totals,
            row.state_gas_sum.map(|v| v as i64),
            row.state_gas_spillover_sum.map(|v| v as i64),
            multiplier_log2_hist,
            row.tx_count_creation.map(|v| v as i64),
            row.tx_count_authorization.map(|v| v as i64),
            row.tx_count_runtime_state.map(|v| v as i64),
            row.tx_count_no_state.map(|v| v as i64),
            row.cold_account_access_count.map(|v| v as i64),
            row.storage_drivers.map(|s| s.sload_cold as i64),
            row.storage_drivers.map(|s| s.sload_warm as i64),
            row.storage_drivers.map(|s| s.sstore_cold as i64),
            row.storage_drivers.map(|s| s.sstore_set as i64),
            row.storage_drivers.map(|s| s.sstore_reset as i64),
            row.storage_drivers.map(|s| s.sstore_clear as i64),
            row.storage_drivers.map(|s| s.sstore_noop as i64),
            row.storage_drivers.map(|s| s.sstore_dirty as i64),
            row.account_drivers.map(|a| a.warm_account_access as i64),
            row.account_drivers.map(|a| a.value_transfer as i64),
            row.account_drivers.map(|a| a.create_opcode as i64),
            row.account_drivers.map(|a| a.access_list_address as i64),
            row.account_drivers.map(|a| a.access_list_storage_key as i64),
            row.tx_count_type_legacy.map(|v| v as i64),
            row.tx_count_type_access_list.map(|v| v as i64),
            row.tx_count_type_dynamic_fee.map(|v| v as i64),
            row.tx_count_type_blob.map(|v| v as i64),
            row.tx_count_type_set_code.map(|v| v as i64),
            row.tx_count_type_other.map(|v| v as i64),
            row.tx_count_simple_transfer.map(|v| v as i64),
            row.tx_count_contract_call.map(|v| v as i64),
            gas_delta_pct_hist,
            row.baseline_gas_used_sum.map(|v| v as i64),
        ],
    )?;
    Ok(())
}

fn insert_divergence(tx: &Transaction<'_>, row: &DivergenceRow) -> Result<u64, DatabaseError> {
    tx.execute(
        "INSERT INTO divergences (
            schedule_name, schedule_config_hash, block_number, tx_index, tx_hash,
            timestamp, outer_limit_only_failure,
            sender, recipient, is_create, tx_gas_limit,
            baseline_success, schedule_success,
            status_changed, event_logs_changed, output_changed, logs_bloom_changed,
            baseline_gas_used, schedule_gas_used, gas_delta,
            baseline_total_gas_spent, baseline_gas_refunded,
            schedule_total_gas_spent, schedule_gas_refunded,
            schedule_intrinsic_gas, schedule_floor_gas,
            min_multiplier_to_succeed,
            divergence_contract, divergence_pc, divergence_call_depth, divergence_opcode,
            oog_contract, oog_pc, oog_call_depth, oog_opcode, oog_pattern, oog_gas_remaining,
            oog_chain_proportional, oog_bottleneck_depth, oog_bottleneck_kind,
            schedule_state_gas_spent, schedule_state_gas_demanded,
            schedule_initial_state_gas, schedule_initial_reservoir,
            runtime_state_gas, runtime_state_gas_spillover,
            state_gas_category, reservoir_exhausted,
            replay_halt_oog,
            cold_account_access_count,
            additional_gas_charged, failure_selector_path,
            tx_type, tx_nonce, entry_selector,
            input_zero_bytes, input_nonzero_bytes, has_authorization,
            failure_reason, revert_data, revert_decoded, tx_output,
            baseline_frame_success, baseline_frame_gas_used,
            baseline_frame_gas_provided, surcharge_at_oog,
            gas_div_contract, gas_div_pc, gas_div_call_depth, gas_div_opcode,
            tax_second_db_read, tax_other, tax_intrinsic,
            sload_cold_count, sload_warm_count, sstore_cold_count,
            sstore_set_count, sstore_reset_count, sstore_clear_count,
            sstore_noop_count, sstore_dirty_count,
            warm_account_access_count, value_transfer_count, create_opcode_count,
            access_list_address_count, access_list_storage_key_count,
            tier1_failure_reason, tier1_oog_opcode, tier1_oog_contract,
            tier1_oog_pc, tier1_oog_depth, tier1_oog_gas_remaining,
            tier1_failing_selector, tier1_failing_gas_provided, tier1_failing_gas_requested
        ) VALUES (?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?,
                  ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?,
                  ?,
                  ?,
                  ?, ?,
                  ?, ?, ?,
                  ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            row.schedule_name,
            row.schedule_config_hash,
            row.block_number as i64,
            row.tx_index as i64,
            row.tx_hash.as_slice(),
            row.timestamp as i64,
            row.outer_limit_only_failure,
            format!("{:#x}", row.sender),
            row.recipient.map(|a| format!("{a:#x}")),
            row.is_create,
            row.tx_gas_limit as i64,
            row.baseline_success,
            row.schedule_success,
            row.status_changed,
            row.event_logs_changed,
            row.output_changed,
            row.logs_bloom_changed,
            row.baseline_gas_used as i64,
            row.schedule_gas_used as i64,
            row.gas_delta,
            row.baseline_total_gas_spent.map(|v| v as i64),
            row.baseline_gas_refunded.map(|v| v as i64),
            row.schedule_total_gas_spent.map(|v| v as i64),
            row.schedule_gas_refunded.map(|v| v as i64),
            row.schedule_intrinsic_gas.map(|v| v as i64),
            row.schedule_floor_gas.map(|v| v as i64),
            row.min_multiplier_to_succeed,
            row.divergence_contract.map(|a| format!("{a:#x}")),
            row.divergence_pc.map(|v| v as i64),
            row.divergence_call_depth,
            row.divergence_opcode.map(|v| v as i64),
            row.oog_contract.map(|a| format!("{a:#x}")),
            row.oog_pc.map(|v| v as i64),
            row.oog_call_depth,
            row.oog_opcode.map(|v| v as i64),
            row.oog_pattern,
            row.oog_gas_remaining.map(|v| v as i64),
            row.oog_chain_proportional,
            row.oog_bottleneck_depth,
            row.oog_bottleneck_kind,
            row.schedule_state_gas_spent.map(|v| v as i64),
            row.schedule_state_gas_demanded.map(|v| v as i64),
            row.schedule_initial_state_gas.map(|v| v as i64),
            row.schedule_initial_reservoir.map(|v| v as i64),
            row.runtime_state_gas.map(|v| v as i64),
            row.runtime_state_gas_spillover.map(|v| v as i64),
            row.state_gas_category,
            row.reservoir_exhausted,
            row.replay_halt_oog,
            row.cold_account_access_count.map(|v| v as i64),
            row.additional_gas_charged,
            row.failure_selector_path,
            row.tx_type.map(|v| v as i64),
            row.tx_nonce.map(|v| v as i64),
            row.entry_selector.map(|s| s.to_vec()),
            row.input_zero_bytes.map(|v| v as i64),
            row.input_nonzero_bytes.map(|v| v as i64),
            row.has_authorization,
            row.failure_reason,
            row.revert_data,
            row.revert_decoded,
            row.tx_output,
            row.baseline_frame_success,
            row.baseline_frame_gas_used.map(|v| v as i64),
            row.baseline_frame_gas_provided.map(|v| v as i64),
            row.surcharge_at_oog,
            row.gas_div_contract.map(|a| format!("{a:#x}")),
            row.gas_div_pc.map(|v| v as i64),
            row.gas_div_call_depth,
            row.gas_div_opcode.map(|v| v as i64),
            row.tax_second_db_read,
            row.tax_other,
            row.tax_intrinsic,
            row.storage_drivers.map(|s| s.sload_cold as i64),
            row.storage_drivers.map(|s| s.sload_warm as i64),
            row.storage_drivers.map(|s| s.sstore_cold as i64),
            row.storage_drivers.map(|s| s.sstore_set as i64),
            row.storage_drivers.map(|s| s.sstore_reset as i64),
            row.storage_drivers.map(|s| s.sstore_clear as i64),
            row.storage_drivers.map(|s| s.sstore_noop as i64),
            row.storage_drivers.map(|s| s.sstore_dirty as i64),
            // F2/F3: account-side gas drivers.
            row.account_drivers.map(|a| a.warm_account_access as i64),
            row.account_drivers.map(|a| a.value_transfer as i64),
            row.account_drivers.map(|a| a.create_opcode as i64),
            row.account_drivers.map(|a| a.access_list_address as i64),
            row.account_drivers.map(|a| a.access_list_storage_key as i64),
            // F1: 1×-failure forensics (mirror the existing oog_* / selector
            // mapping — Address formatted as lowercase hex, opcode/pc/depth as
            // i64, selector as a byte vec).
            row.tier1_failure_reason,
            row.tier1_oog_opcode.map(|v| v as i64),
            row.tier1_oog_contract.map(|a| format!("{a:#x}")),
            row.tier1_oog_pc.map(|v| v as i64),
            row.tier1_oog_depth,
            row.tier1_oog_gas_remaining.map(|v| v as i64),
            row.tier1_failing_selector.map(|s| s.to_vec()),
            row.tier1_failing_gas_provided.map(|v| v as i64),
            row.tier1_failing_gas_requested.map(|v| v as i64),
        ],
    )?;
    Ok(tx.last_insert_rowid() as u64)
}

fn insert_call_frame(
    tx: &Transaction<'_>,
    divergence_id: u64,
    trace_kind: &str,
    row: &CallFrameRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO divergence_call_frames (
            divergence_id, trace_kind, call_index, parent_call_index, depth,
            from_address, to_address, code_address, codehash, call_type,
            selector, value_wei, gas_provided, gas_used, gas_margin,
            success, parent_gas_at_call, gas_requested_on_stack,
            eip150_cap_binding, deployed_bytecode_len,
            repricing_gas_delta,
            caller_pc, was_precompile, precompile_address,
            gas_remaining_at_fail, is_divergent_frame
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?, ?)",
        params![
            divergence_id as i64,
            trace_kind,
            row.call_index as i64,
            row.parent_call_index.map(|v| v as i64),
            row.depth as i64,
            format!("{:#x}", row.from_address),
            format!("{:#x}", row.to_address),
            row.code_address.map(|a| format!("{a:#x}")),
            row.codehash.map(|h| h.as_slice().to_vec()),
            row.call_type,
            row.selector.map(|s| s.to_vec()),
            row.value_wei,
            row.gas_provided as i64,
            row.gas_used as i64,
            row.gas_margin,
            row.success,
            row.parent_gas_at_call.map(|v| v as i64),
            row.gas_requested_on_stack.map(|v| v as i64),
            row.eip150_cap_binding,
            row.deployed_bytecode_len.map(|v| v as i64),
            row.repricing_gas_delta,
            row.caller_pc.map(|v| v as i64),
            row.was_precompile,
            row.precompile_address.map(|a| format!("{a:#x}")),
            row.gas_remaining_at_fail.map(|v| v as i64),
            row.is_divergent_frame,
        ],
    )?;
    Ok(())
}

fn insert_opcode_count(
    tx: &Transaction<'_>,
    divergence_id: u64,
    trace_kind: &str,
    row: &OpcodeCountRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO divergence_opcode_counts (
            divergence_id, trace_kind, call_index, opcode, count, gas_baseline, gas_schedule
        ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            divergence_id as i64,
            trace_kind,
            row.call_index as i64,
            row.opcode as i64,
            row.count as i64,
            row.gas_baseline as i64,
            row.gas_schedule as i64,
        ],
    )?;
    Ok(())
}

fn insert_event_log(
    tx: &Transaction<'_>,
    divergence_id: u64,
    trace_kind: &str,
    log: &EventLog,
) -> Result<(), DatabaseError> {
    let topic = |i: usize| log.topics.get(i).map(|t| t.as_slice().to_vec());
    let data_hash = keccak256(&log.data);
    tx.execute(
        "INSERT INTO divergence_event_logs (
            divergence_id, trace_kind, log_index, address,
            topic0, topic1, topic2, topic3, data_bytes, data_hash
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            divergence_id as i64,
            trace_kind,
            log.log_index as i64,
            format!("{:#x}", log.address),
            topic(0),
            topic(1),
            topic(2),
            topic(3),
            log.data.as_ref(),
            data_hash.as_slice(),
        ],
    )?;
    Ok(())
}

/// Insert (or idempotently reconcile) one export-outbox row inside the
/// caller's transaction. The block coordinates are read from `coverage`; the
/// identity and payload come from `export`.
///
/// Conflict handling on the deterministic `export_id`:
/// - identical `payload_hash` → no-op, preserving any `exported` state;
/// - different `payload_hash` → replace the payload and reset to `pending`, logging an invariant
///   warning (the same logical block produced two different payloads, which should not happen).
fn insert_export_outbox(
    tx: &Transaction<'_>,
    coverage: &BlockCoverageRow,
    export: &EncodedExportEnvelope,
) -> Result<(), DatabaseError> {
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "SELECT payload_hash FROM export_outbox WHERE export_id = ?",
            params![export.export_id],
            |row| row.get(0),
        )
        .optional()?;

    let now = current_unix_seconds() as i64;
    match existing {
        Some(hash) if hash == export.payload_hash.as_slice() => {
            // Identical payload already enqueued (or already exported). Nothing
            // to do — re-recording the same block must not resurrect or
            // duplicate an export.
        }
        Some(_) => {
            tracing::warn!(
                target: "exex::research::outbox",
                export_id = %export.export_id,
                block = coverage.block_number,
                schedule = %coverage.schedule_name,
                "export payload changed for an existing export_id; resetting outbox row to pending"
            );
            tx.execute(
                "UPDATE export_outbox SET
                    analysis_config_hash = ?, schedule_name = ?, schedule_config_hash = ?,
                    block_number = ?, block_hash = ?, payload_version = ?,
                    payload_zstd = ?, payload_hash = ?, payload_bytes = ?,
                    state = 'pending', attempts = 0, next_attempt_at = 0,
                    last_error = NULL, updated_at = ?, exported_at = NULL
                 WHERE export_id = ?",
                params![
                    export.analysis_config_hash,
                    coverage.schedule_name,
                    export.schedule_config_hash,
                    coverage.block_number as i64,
                    coverage.block_hash.as_slice(),
                    export.payload_version as i64,
                    export.payload_zstd,
                    export.payload_hash.as_slice(),
                    export.payload_bytes as i64,
                    now,
                    export.export_id,
                ],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO export_outbox (
                    export_id, analysis_config_hash, schedule_name, schedule_config_hash,
                    block_number, block_hash, payload_version, payload_zstd,
                    payload_hash, payload_bytes, state, attempts, next_attempt_at,
                    last_error, created_at, updated_at, exported_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', 0, 0, NULL, ?, ?, NULL)",
                params![
                    export.export_id,
                    export.analysis_config_hash,
                    coverage.schedule_name,
                    export.schedule_config_hash,
                    coverage.block_number as i64,
                    coverage.block_hash.as_slice(),
                    export.payload_version as i64,
                    export.payload_zstd,
                    export.payload_hash.as_slice(),
                    export.payload_bytes as i64,
                    now,
                    now,
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirm an empty `:memory:` DB is openable, all tables exist, and
    /// re-opening (re-running the DDL) is a no-op.
    #[test]
    fn in_memory_open_creates_all_tables() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let conn = db.conn.lock().unwrap();

        // The DDL is idempotent — calling initialize_schema again should
        // succeed without errors.
        initialize_schema(&conn).expect("initialize_schema is idempotent");

        // Every expected table is present.
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let mut rows = stmt.query([]).unwrap();
        let mut tables = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            tables.push(row.get::<_, String>(0).unwrap());
        }

        for expected in [
            "analysis_manifests",
            "analysis_runs",
            "block_coverage",
            "block_summaries",
            "contract_metadata",
            "divergence_call_frames",
            "divergence_event_logs",
            "divergence_opcode_counts",
            "divergences",
            "export_outbox",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}, have: {tables:?}"
            );
        }
    }

    #[test]
    fn analysis_run_records_round_trip() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let run_id = db
            .record_analysis_run_start(
                "eip-8037",
                "config-hash-abc",
                Some("abc1234"),
                Some("smoke test"),
            )
            .unwrap();
        assert!(run_id >= 1);

        // The row must have the compiled-in schema_version and a NULL
        // run_finished_at until we close it out.
        {
            let conn = db.conn.lock().unwrap();
            let (version, finished): (i64, Option<i64>) = conn
                .query_row(
                    "SELECT schema_version, run_finished_at
                     FROM analysis_runs WHERE run_id = ?",
                    params![run_id as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(version as u32, SCHEMA_VERSION);
            assert!(finished.is_none());
        }

        db.record_analysis_run_end(run_id, 42).unwrap();
        let conn = db.conn.lock().unwrap();
        let (finished, blocks): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT run_finished_at, blocks_processed
                 FROM analysis_runs WHERE run_id = ?",
                params![run_id as i64],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(finished.is_some(), "finished_at should be populated");
        assert_eq!(blocks, Some(42));
    }

    fn fresh_db_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("test.sqlite")
    }

    #[test]
    fn schema_version_mismatch_is_detected() {
        // Open a fresh DB and forge an analysis_runs row written under a
        // newer schema version. The next open should reject it.
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);

        {
            let db = DivergenceDatabase::open(&path).unwrap();
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO analysis_runs (
                    schema_version, schedule_name, schedule_config_hash,
                    run_started_at
                 ) VALUES (?, 'test', 'hash', ?)",
                params![SCHEMA_VERSION + 1, current_unix_seconds() as i64],
            )
            .unwrap();
        }

        let err = DivergenceDatabase::open(&path).unwrap_err();
        match err {
            DatabaseError::SchemaVersionMismatch { expected, found } => {
                assert_eq!(expected, SCHEMA_VERSION);
                assert_eq!(found, SCHEMA_VERSION + 1);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn fresh_open_passes_version_check() {
        // No analysis_runs rows means no version constraint to violate.
        let dir = tempfile::tempdir().unwrap();
        let _db = DivergenceDatabase::open(fresh_db_path(&dir)).unwrap();
    }

    #[test]
    fn pragma_user_version_mismatch_is_rejected() {
        // A DB created under a different schema version (tables present, wrong
        // `PRAGMA user_version`) is rejected at open — no migration, wipe & re-gather.
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        {
            // Fresh open stamps user_version = SCHEMA_VERSION and creates tables.
            let db = DivergenceDatabase::open(&path).unwrap();
            db.conn
                .lock()
                .unwrap()
                .pragma_update(None, "user_version", SCHEMA_VERSION - 1)
                .unwrap();
        }
        match DivergenceDatabase::open(&path).unwrap_err() {
            DatabaseError::SchemaVersionMismatch { expected, found } => {
                assert_eq!(expected, SCHEMA_VERSION);
                assert_eq!(found, SCHEMA_VERSION - 1);
            }
            other => panic!("expected SchemaVersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn legacy_unstamped_db_is_rejected() {
        // A pre-versioning DB has our tables but user_version == 0 → rejected.
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        {
            let db = DivergenceDatabase::open(&path).unwrap();
            db.conn.lock().unwrap().pragma_update(None, "user_version", 0i64).unwrap();
        }
        assert!(matches!(
            DivergenceDatabase::open(&path).unwrap_err(),
            DatabaseError::SchemaVersionMismatch { found: 0, .. }
        ));
    }

    /// Minimal `BlockCoverageRow` fixture for write-path tests. `stored` is the
    /// number of per-tx forensic rows (failures + divergences); `gas_only` the
    /// silent-majority count whose only change is gas.
    fn fixture_coverage(
        schedule: &str,
        block: u64,
        stored: u32,
        gas_only: u32,
    ) -> BlockCoverageRow {
        BlockCoverageRow {
            schedule_name: schedule.to_string(),
            schedule_config_hash: "config".to_string(),
            block_number: block,
            block_hash: B256::repeat_byte(0xb0 + (block as u8 & 0x0f)),
            parent_hash: B256::repeat_byte(0xa0),
            timestamp: 1_700_000_000 + block,
            tx_count: stored + gas_only,
            tx_count_unchanged: 0,
            tx_count_gas_only: gas_only,
            tx_count_stored: stored,
            block_gas_used: 15_000_000,
            block_gas_limit: 30_000_000,
            block_base_fee_per_gas: Some(1_000_000_000),
        }
    }

    /// Minimal `TxGasResultRow` fixture for the per-tx gas table.
    fn fixture_tx_gas_result(schedule: &str, block: u64, tx_index: u32) -> TxGasResultRow {
        TxGasResultRow {
            schedule_name: schedule.to_string(),
            schedule_config_hash: "config".to_string(),
            block_number: block,
            tx_index,
            tx_hash: B256::repeat_byte(0x11),
            tx_type: 2,
            tx_gas_limit: 500_000,
            max_fee_per_gas: "30000000000".to_string(),
            max_priority_fee_per_gas: Some("1000000000".to_string()),
            baseline_success: true,
            baseline_gas_used: 21_000,
            baseline_total_gas_spent: 21_000,
            schedule_success: true,
            schedule_gas_used: 33_000,
            schedule_total_gas_spent: 35_000,
            schedule_gas_refunded: 2_000,
            schedule_floor_gas: 21_000,
            schedule_state_gas_spent: 0,
            schedule_intrinsic_gas: Some(15_000),
            min_multiplier_to_succeed: Some(0.066),
        }
    }

    #[test]
    fn tx_gas_results_round_trips_every_column() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let row = fixture_tx_gas_result("test", 100, 3);
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 100, 0, 1),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![row.clone()],
        })
        .unwrap();

        let conn = db.conn.lock().unwrap();
        let (
            tx_hash,
            tx_type,
            gas_limit,
            max_fee,
            max_prio,
            sched_gas,
            pre_refund,
            refunded,
            intrinsic,
            multiplier,
        ): (
            Vec<u8>,
            i64,
            i64,
            String,
            Option<String>,
            i64,
            i64,
            i64,
            Option<i64>,
            Option<f64>,
        ) = conn
            .query_row(
                "SELECT tx_hash, tx_type, tx_gas_limit, max_fee_per_gas,
                        max_priority_fee_per_gas, schedule_gas_used,
                        schedule_total_gas_spent, schedule_gas_refunded,
                        schedule_intrinsic_gas, min_multiplier_to_succeed
                 FROM tx_gas_results WHERE block_number = 100 AND tx_index = 3",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(tx_hash, row.tx_hash.as_slice());
        assert_eq!(tx_type, row.tx_type as i64);
        assert_eq!(gas_limit, row.tx_gas_limit as i64);
        // Fee caps stay decimal strings — never narrowed to an integer column.
        assert_eq!(max_fee, row.max_fee_per_gas);
        assert_eq!(max_prio, row.max_priority_fee_per_gas);
        assert_eq!(sched_gas, row.schedule_gas_used as i64);
        // The pre-refund figure (EIP-7778 block accounting) must be stored
        // distinctly from the post-refund sender-facing one.
        assert_eq!(pre_refund, row.schedule_total_gas_spent as i64);
        assert_ne!(pre_refund, sched_gas);
        assert_eq!(refunded, row.schedule_gas_refunded as i64);
        assert_eq!(intrinsic, Some(row.schedule_intrinsic_gas.unwrap() as i64));
        assert_eq!(multiplier, row.min_multiplier_to_succeed);
    }

    #[test]
    fn tx_gas_results_written_for_every_tx_not_just_drill_ins() {
        // The whole point of the table: a block whose txs produced NO
        // divergence rows still yields one gas row per tx.
        let db = DivergenceDatabase::in_memory().unwrap();
        let rows: Vec<_> = (0..5).map(|i| fixture_tx_gas_result("test", 200, i)).collect();
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 200, 0, 5),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: rows,
        })
        .unwrap();

        let conn = db.conn.lock().unwrap();
        let divergences: i64 =
            conn.query_row("SELECT COUNT(*) FROM divergences", [], |r| r.get(0)).unwrap();
        let gas_rows: i64 =
            conn.query_row("SELECT COUNT(*) FROM tx_gas_results", [], |r| r.get(0)).unwrap();
        assert_eq!(divergences, 0);
        assert_eq!(gas_rows, 5);
    }

    #[test]
    fn tx_gas_results_upsert_on_replay_and_clear_on_reorg() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let mut row = fixture_tx_gas_result("test", 300, 0);
        let output = |r: TxGasResultRow| BlockOutput {
            coverage: fixture_coverage("test", 300, 0, 1),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![r],
        };

        db.record_block_output(&output(row.clone())).unwrap();
        // Idempotent re-replay refreshes in place rather than duplicating.
        row.schedule_gas_used = 99_999;
        db.record_block_output(&output(row)).unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let (count, gas): (i64, i64) = conn
                .query_row("SELECT COUNT(*), MAX(schedule_gas_used) FROM tx_gas_results", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .unwrap();
            assert_eq!(count, 1);
            assert_eq!(gas, 99_999);
        }

        // A reorg must drop the rows with everything else for the range,
        // or the next replay would leave stale per-tx gas behind.
        let deleted = db.delete_block_range(300, 300).unwrap();
        assert_eq!(deleted.tx_gas_results, 1);
        let conn = db.conn.lock().unwrap();
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM tx_gas_results", [], |r| r.get(0)).unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn block_coverage_stores_base_fee() {
        let db = DivergenceDatabase::in_memory().unwrap();
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 400, 0, 1),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();
        let conn = db.conn.lock().unwrap();
        let base_fee: Option<i64> = conn
            .query_row(
                "SELECT block_base_fee_per_gas FROM block_coverage WHERE block_number = 400",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(base_fee, Some(1_000_000_000));
    }

    /// Minimal `DivergenceRow` fixture for write-path tests. All optional
    /// columns left as `None`.
    fn fixture_divergence(
        block: u64,
        tx_index: u32,
        schedule_success: bool,
        gas_delta: i64,
    ) -> DivergenceRow {
        DivergenceRow {
            schedule_name: "test".to_string(),
            schedule_config_hash: "config".to_string(),
            block_number: block,
            tx_index,
            tx_hash: B256::repeat_byte(0xdd),
            timestamp: 1_700_000_000,
            sender: Address::repeat_byte(0x11),
            recipient: Some(Address::repeat_byte(0x22)),
            tx_gas_limit: 500_000,
            baseline_success: true,
            schedule_success,
            status_changed: !schedule_success,
            baseline_gas_used: 100_000,
            schedule_gas_used: (100_000i64 + gas_delta).max(0) as u64,
            gas_delta,
            ..Default::default()
        }
    }

    #[test]
    fn record_block_output_writes_coverage_summaries_and_drill_in() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let drill_in = DrillInRecord {
            divergence: fixture_divergence(100, 0, false, 12_345),
            call_frames: vec![CallFrameRow {
                call_index: 0,
                parent_call_index: None,
                depth: 0,
                from_address: Address::repeat_byte(0x11),
                to_address: Address::repeat_byte(0x22),
                code_address: Some(Address::repeat_byte(0x22)),
                codehash: Some(B256::repeat_byte(0xcc)),
                call_type: "CALL".to_string(),
                selector: Some([0x12, 0x34, 0x56, 0x78]),
                value_wei: Some("0".to_string()),
                gas_provided: 500_000,
                gas_used: 120_000,
                gas_margin: Some(380_000),
                success: false,
                parent_gas_at_call: None,
                gas_requested_on_stack: None,
                eip150_cap_binding: None,
                deployed_bytecode_len: None,
                repricing_gas_delta: 0,
                ..Default::default()
            }],
            opcode_counts: vec![OpcodeCountRow {
                call_index: 0,
                opcode: 0x55,
                count: 3,
                gas_baseline: 60_000,
                gas_schedule: 75_000,
            }],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![EventLog {
                log_index: 0,
                address: Address::repeat_byte(0x22),
                topics: vec![B256::repeat_byte(0xab)],
                data: alloy_primitives::Bytes::from(vec![1, 2, 3]),
            }],
        };

        let output = BlockOutput {
            coverage: fixture_coverage("test", 100, 1, 0),
            summaries: vec![BlockSummaryRow {
                schedule_name: "test".to_string(),
                block_number: 100,
                class: AggregateClass::GasOnly,
                tx_count: 1,
                gas_delta_sum: Some(12_345),
                gas_delta_sum_sq: Some(12_345i64 * 12_345i64),
                gas_delta_min: Some(12_345),
                gas_delta_max: Some(12_345),
                gas_delta_log2_hist: Some([0; 12]),
                opcode_totals: vec![OpcodeBucketTotal {
                    opcode: 0x55,
                    count: 3,
                    gas_baseline: 60_000,
                    gas_schedule: 75_000,
                }],
                state_gas_sum: None,
                state_gas_spillover_sum: None,
                multiplier_log2_hist: None,
                tx_count_creation: None,
                tx_count_authorization: None,
                tx_count_runtime_state: None,
                tx_count_no_state: None,
                cold_account_access_count: None,
                storage_drivers: None,
                account_drivers: None,
                tx_count_type_legacy: None,
                tx_count_type_access_list: None,
                tx_count_type_dynamic_fee: None,
                tx_count_type_blob: None,
                tx_count_type_set_code: None,
                tx_count_type_other: None,
                tx_count_simple_transfer: None,
                tx_count_contract_call: None,
                gas_delta_pct_hist: None,
                baseline_gas_used_sum: None,
            }],
            drill_ins: vec![drill_in],
            recipients: vec![],
            tx_gas_results: vec![],
        };

        db.record_block_output(&output).unwrap();

        let conn = db.conn.lock().unwrap();

        let (tx_count, stored): (i64, i64) = conn
            .query_row(
                "SELECT tx_count, tx_count_stored
                 FROM block_coverage WHERE block_number = 100",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tx_count, 1);
        assert_eq!(stored, 1);

        let summary_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM block_summaries WHERE block_number = 100", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(summary_count, 1);

        // Verify the JSON-encoded histogram round-trips through SQLite.
        let hist_json: String = conn
            .query_row(
                "SELECT gas_delta_log2_hist FROM block_summaries WHERE block_number = 100",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let hist: Vec<i32> = serde_json::from_str(&hist_json).unwrap();
        assert_eq!(hist.len(), 12);

        let div_count: i64 =
            conn.query_row("SELECT COUNT(*) FROM divergences", [], |row| row.get(0)).unwrap();
        assert_eq!(div_count, 1);

        let frame_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM divergence_call_frames", [], |row| row.get(0))
            .unwrap();
        assert_eq!(frame_count, 1);

        let opc_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM divergence_opcode_counts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(opc_count, 1);

        let log_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM divergence_event_logs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(log_count, 1);
    }

    /// Baseline call frames (F15) and opcode counts (F11) coexist with the
    /// schedule rows under one `divergence_id`, partitioned by `trace_kind`.
    #[test]
    fn baseline_trace_stored_with_trace_kind() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let frame = |from: u8| CallFrameRow {
            call_index: 0,
            from_address: Address::repeat_byte(from),
            to_address: Address::repeat_byte(0x22),
            call_type: "CALL".to_string(),
            success: true,
            ..Default::default()
        };
        let opc = |count: u64| OpcodeCountRow {
            call_index: 0,
            opcode: 0x54,
            count,
            gas_baseline: 100,
            gas_schedule: 100,
        };

        let drill_in = DrillInRecord {
            divergence: fixture_divergence(7, 0, false, 42),
            call_frames: vec![frame(0x11)],
            opcode_counts: vec![opc(5)],
            baseline_call_frames: vec![frame(0xaa)],
            baseline_opcode_counts: vec![opc(3)],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        let output = BlockOutput {
            coverage: fixture_coverage("test", 7, 1, 0),
            summaries: vec![],
            drill_ins: vec![drill_in],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        db.record_block_output(&output).unwrap();

        let conn = db.conn.lock().unwrap();
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };

        assert_eq!(
            count("SELECT COUNT(*) FROM divergence_call_frames WHERE trace_kind='schedule'"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM divergence_call_frames WHERE trace_kind='baseline'"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM divergence_opcode_counts WHERE trace_kind='baseline'"),
            1
        );
        // The baseline opcode row carries the baseline count (3), distinct from
        // the schedule row's count (5).
        assert_eq!(
            count(
                "SELECT count FROM divergence_opcode_counts \
                 WHERE trace_kind='baseline' AND opcode=84"
            ),
            3
        );
    }

    /// F3/F14: under a DELEGATECALL the call frame splits `to_address` (proxy)
    /// from `code_address` (implementation), and the metadata backfill keys on
    /// the implementation — not the proxy.
    #[test]
    fn metadata_backfill_keys_on_code_address() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let proxy = Address::repeat_byte(0x11);
        let implementation = Address::repeat_byte(0x22);

        let drill_in = DrillInRecord {
            divergence: fixture_divergence(5, 0, false, 1),
            call_frames: vec![CallFrameRow {
                call_index: 0,
                from_address: Address::repeat_byte(0x01),
                to_address: proxy,
                code_address: Some(implementation),
                codehash: Some(B256::repeat_byte(0xcc)),
                call_type: "DELEGATECALL".to_string(),
                success: true,
                ..Default::default()
            }],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 5, 1, 0),
            summaries: vec![],
            drill_ins: vec![drill_in],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        // The backfill returns the implementation (code_address), not the proxy.
        let addrs = db.distinct_call_frame_addresses().unwrap();
        assert_eq!(addrs, vec![format!("{implementation:#x}")]);
        assert!(!addrs.contains(&format!("{proxy:#x}")));
    }

    /// `BlockSummaryRow` fixture with every v11 column populated with
    /// pairwise-distinct values, so a positional swap or missed binding in
    /// the insert/upsert paths can't cancel out. Deliberately does NOT
    /// satisfy the partition laws (Σ type counts == tx_count etc.) —
    /// `SQLite` doesn't enforce them, and distinctness matters more here.
    fn fixture_v11_summary(block: u64) -> BlockSummaryRow {
        BlockSummaryRow {
            schedule_name: "test".to_string(),
            block_number: block,
            class: AggregateClass::GasOnly,
            tx_count: 4,
            gas_delta_sum: Some(40),
            gas_delta_sum_sq: Some(400),
            gas_delta_min: Some(-10),
            gas_delta_max: Some(30),
            gas_delta_log2_hist: Some([0; 12]),
            opcode_totals: vec![],
            state_gas_sum: None,
            state_gas_spillover_sum: None,
            multiplier_log2_hist: None,
            tx_count_creation: Some(1),
            tx_count_authorization: Some(0),
            tx_count_runtime_state: Some(0),
            tx_count_no_state: Some(4),
            cold_account_access_count: None,
            storage_drivers: None,
            account_drivers: None,
            tx_count_type_legacy: Some(1),
            tx_count_type_access_list: Some(2),
            tx_count_type_dynamic_fee: Some(3),
            tx_count_type_blob: Some(4),
            tx_count_type_set_code: Some(5),
            tx_count_type_other: Some(6),
            tx_count_simple_transfer: Some(7),
            tx_count_contract_call: Some(8),
            gas_delta_pct_hist: Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
            baseline_gas_used_sum: Some(84_000),
        }
    }

    #[test]
    fn pre_v11_summary_json_decodes_with_new_fields_none() {
        // Pins the `#[serde(default)]` back-compat contract: a pre-v11 outbox
        // payload (no v11 keys) must decode with the new columns None, not
        // fail — a decode error would permanently block the outbox row.
        let mut v = serde_json::to_value(fixture_v11_summary(1)).unwrap();
        let obj = v.as_object_mut().unwrap();
        for key in [
            "tx_count_type_legacy",
            "tx_count_type_access_list",
            "tx_count_type_dynamic_fee",
            "tx_count_type_blob",
            "tx_count_type_set_code",
            "tx_count_type_other",
            "tx_count_simple_transfer",
            "tx_count_contract_call",
            "gas_delta_pct_hist",
            "baseline_gas_used_sum",
        ] {
            obj.remove(key).expect("v11 field name drifted");
        }
        let row: BlockSummaryRow = serde_json::from_value(v).unwrap();
        assert_eq!(row.tx_count_type_legacy, None);
        assert_eq!(row.tx_count_type_access_list, None);
        assert_eq!(row.tx_count_type_dynamic_fee, None);
        assert_eq!(row.tx_count_type_blob, None);
        assert_eq!(row.tx_count_type_set_code, None);
        assert_eq!(row.tx_count_type_other, None);
        assert_eq!(row.tx_count_simple_transfer, None);
        assert_eq!(row.tx_count_contract_call, None);
        assert_eq!(row.gas_delta_pct_hist, None);
        assert_eq!(row.baseline_gas_used_sum, None);
        // Pre-v11 fields survive untouched.
        assert_eq!(row.tx_count, 4);
        assert_eq!(row.class, AggregateClass::GasOnly);
    }

    #[test]
    fn v11_summary_columns_round_trip() {
        let db = DivergenceDatabase::in_memory().unwrap();
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 11, 0, 4),
            summaries: vec![fixture_v11_summary(11)],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        let conn = db.conn.lock().unwrap();
        let (counts, hist, baseline): ([i64; 8], String, i64) = conn
            .query_row(
                "SELECT tx_count_type_legacy, tx_count_type_access_list, \
                 tx_count_type_dynamic_fee, tx_count_type_blob, tx_count_type_set_code, \
                 tx_count_type_other, tx_count_simple_transfer, tx_count_contract_call, \
                 gas_delta_pct_hist, baseline_gas_used_sum \
                 FROM block_summaries WHERE block_number = 11",
                [],
                |r| {
                    Ok((
                        [
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ],
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();
        // All 10 columns read back exactly; full-array equality on the hist
        // (len/sum checks would pass on a reordered array).
        assert_eq!(counts, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(baseline, 84_000);
        let bins: Vec<i32> = serde_json::from_str(&hist).unwrap();
        assert_eq!(bins, (1..=13).collect::<Vec<i32>>());
    }

    #[test]
    fn summary_upsert_overwrites_new_columns() {
        // Re-touching a (schedule, block, class) row must overwrite the v11
        // columns too — guards the ON CONFLICT DO UPDATE list, whose omission
        // would silently keep stale values on reorg re-analysis.
        let db = DivergenceDatabase::in_memory().unwrap();
        let mut summary = fixture_v11_summary(12);
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 12, 0, 4),
            summaries: vec![summary.clone()],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        // Overwrite ALL 10 v11 columns with fresh distinct values so a
        // forgotten SET line on any one of them is caught.
        summary.tx_count_type_legacy = Some(11);
        summary.tx_count_type_access_list = Some(12);
        summary.tx_count_type_dynamic_fee = Some(13);
        summary.tx_count_type_blob = Some(14);
        summary.tx_count_type_set_code = Some(15);
        summary.tx_count_type_other = Some(16);
        summary.tx_count_simple_transfer = Some(17);
        summary.tx_count_contract_call = Some(18);
        summary.gas_delta_pct_hist = Some([21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33]);
        summary.baseline_gas_used_sum = Some(99_000);
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 12, 0, 4),
            summaries: vec![summary],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        let conn = db.conn.lock().unwrap();
        let (counts, hist, baseline): ([i64; 8], String, i64) = conn
            .query_row(
                "SELECT tx_count_type_legacy, tx_count_type_access_list, \
                 tx_count_type_dynamic_fee, tx_count_type_blob, tx_count_type_set_code, \
                 tx_count_type_other, tx_count_simple_transfer, tx_count_contract_call, \
                 gas_delta_pct_hist, baseline_gas_used_sum \
                 FROM block_summaries WHERE block_number = 12",
                [],
                |r| {
                    Ok((
                        [
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                        ],
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(counts, [11, 12, 13, 14, 15, 16, 17, 18]);
        assert_eq!(baseline, 99_000);
        let bins: Vec<i32> = serde_json::from_str(&hist).unwrap();
        assert_eq!(bins, (21..=33).collect::<Vec<i32>>());
    }

    /// F8 storage-reprice drivers + F2/F3 account drivers round-trip on both
    /// `divergences` (per-tx) and `block_summaries` (per-class); the F1 tier1
    /// forensic columns round-trip on `divergences`.
    #[test]
    fn storage_drivers_round_trip() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let sd = StorageDrivers {
            sload_cold: 2,
            sload_warm: 5,
            sstore_cold: 1,
            sstore_set: 3,
            sstore_clear: 1,
            ..Default::default()
        };
        let ad = AccountDrivers {
            warm_account_access: 4,
            value_transfer: 2,
            create_opcode: 1,
            access_list_address: 3,
            access_list_storage_key: 7,
        };

        let mut div = fixture_divergence(9, 0, false, 7);
        div.storage_drivers = Some(sd);
        div.account_drivers = Some(ad);
        // F1: a 1×-failure forensic capture (rescued at a bumped tier).
        div.tier1_failure_reason = Some("OutOfGas".to_string());
        div.tier1_oog_opcode = Some(0x54);
        div.tier1_failing_selector = Some([0xde, 0xad, 0xbe, 0xef]);
        div.tier1_failing_gas_provided = Some(21_000);
        let drill_in = DrillInRecord {
            divergence: div,
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        let summary = BlockSummaryRow {
            schedule_name: "test".to_string(),
            block_number: 9,
            class: AggregateClass::GasOnly,
            tx_count: 1,
            gas_delta_sum: Some(1),
            gas_delta_sum_sq: Some(1),
            gas_delta_min: Some(1),
            gas_delta_max: Some(1),
            gas_delta_log2_hist: Some([0; 12]),
            opcode_totals: vec![],
            state_gas_sum: None,
            state_gas_spillover_sum: None,
            multiplier_log2_hist: None,
            tx_count_creation: None,
            tx_count_authorization: None,
            tx_count_runtime_state: None,
            tx_count_no_state: None,
            cold_account_access_count: None,
            storage_drivers: Some(sd),
            account_drivers: Some(ad),
            tx_count_type_legacy: None,
            tx_count_type_access_list: None,
            tx_count_type_dynamic_fee: None,
            tx_count_type_blob: None,
            tx_count_type_set_code: None,
            tx_count_type_other: None,
            tx_count_simple_transfer: None,
            tx_count_contract_call: None,
            gas_delta_pct_hist: None,
            baseline_gas_used_sum: None,
        };
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 9, 1, 1),
            summaries: vec![summary],
            drill_ins: vec![drill_in],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        let conn = db.conn.lock().unwrap();
        let row = |table: &str| -> (i64, i64, i64, i64) {
            conn.query_row(
                &format!(
                    "SELECT sload_cold_count, sstore_set_count, sstore_clear_count, \
                     sstore_dirty_count FROM {table} WHERE block_number = 9"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap()
        };
        // Same drivers land on both the per-tx and per-class tables.
        assert_eq!(row("divergences"), (2, 3, 1, 0));
        assert_eq!(row("block_summaries"), (2, 3, 1, 0));

        // F2/F3 account drivers land on both tables too.
        let acc = |table: &str| -> (i64, i64, i64, i64, i64) {
            conn.query_row(
                &format!(
                    "SELECT warm_account_access_count, value_transfer_count, \
                     create_opcode_count, access_list_address_count, \
                     access_list_storage_key_count FROM {table} WHERE block_number = 9"
                ),
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
        };
        assert_eq!(acc("divergences"), (4, 2, 1, 3, 7));
        assert_eq!(acc("block_summaries"), (4, 2, 1, 3, 7));

        // F1 tier1 forensics round-trip on divergences only.
        let (reason, opcode, sel, provided): (
            Option<String>,
            Option<i64>,
            Option<Vec<u8>>,
            Option<i64>,
        ) = conn
            .query_row(
                "SELECT tier1_failure_reason, tier1_oog_opcode, tier1_failing_selector, \
                 tier1_failing_gas_provided FROM divergences WHERE block_number = 9",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(reason.as_deref(), Some("OutOfGas"));
        assert_eq!(opcode, Some(0x54));
        assert_eq!(sel, Some(vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(provided, Some(21_000));
    }

    #[test]
    fn delete_block_range_clears_all_per_block_tables() {
        let db = DivergenceDatabase::in_memory().unwrap();

        for block in [100u64, 200u64] {
            let drill_in = DrillInRecord {
                divergence: fixture_divergence(block, 0, false, 100),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: Address::ZERO,
                    code_address: None,
                    codehash: None,
                    call_type: "CALL".to_string(),
                    selector: None,
                    value_wei: None,
                    gas_provided: 100,
                    gas_used: 100,
                    gas_margin: None,
                    success: false,
                    parent_gas_at_call: None,
                    gas_requested_on_stack: None,
                    eip150_cap_binding: None,
                    deployed_bytecode_len: None,
                    repricing_gas_delta: 0,
                    ..Default::default()
                }],
                opcode_counts: vec![],
                baseline_call_frames: vec![],
                baseline_opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", block, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                recipients: vec![],
                tx_gas_results: vec![],
            };
            db.record_block_output(&output).unwrap();
        }

        {
            let conn = db.conn.lock().unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM block_coverage", [], |row| row.get(0))
                .unwrap();
            assert_eq!(n, 2);
        }

        let counts = db.delete_block_range(100, 150).unwrap();
        assert_eq!(counts.coverage, 1);
        assert_eq!(counts.divergences, 1);
        assert_eq!(counts.call_frames, 1);

        let conn = db.conn.lock().unwrap();
        let remaining: i64 =
            conn.query_row("SELECT COUNT(*) FROM block_coverage", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 1);
        let remaining_block: i64 = conn
            .query_row("SELECT block_number FROM block_coverage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining_block, 200);
    }

    #[test]
    fn has_block_coverage_with_config_matches_strict() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: fixture_coverage("test", 42, 0, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        db.record_block_output(&output).unwrap();

        assert!(db.has_block_coverage_with_config("test", 42, "config").unwrap());
        assert!(!db.has_block_coverage_with_config("test", 42, "config-v2").unwrap());
        assert!(!db.has_block_coverage_with_config("test", 43, "config").unwrap());
        assert!(!db.has_block_coverage_with_config("other", 42, "config").unwrap());
    }

    #[test]
    fn opcode_count_row_from_frames_skips_zero_opcodes() {
        let mut frame = FrameOpcodeCounts::new(0);
        frame.counts[0x20] = 5;
        frame.gas_baseline[0x20] = 150;
        frame.gas_schedule[0x20] = 180;

        let rows = OpcodeCountRow::from_frames(&[frame]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].opcode, 0x20);
        assert_eq!(rows[0].count, 5);
        assert_eq!(rows[0].gas_baseline, 150);
        assert_eq!(rows[0].gas_schedule, 180);
    }

    #[test]
    fn contract_metadata_upsert_is_idempotent_per_codehash() {
        use crate::contract_metadata::ContractMetadata;
        let db = DivergenceDatabase::in_memory().unwrap();
        let codehash = [0xabu8; 32];
        let meta = ContractMetadata {
            solc_version: Some("0.8.21".to_string()),
            solc_commit: None,
            evm_target: None,
            cbor_present: true,
            has_metadata_hash: true,
        };

        assert!(!db.has_contract_metadata(codehash).unwrap());

        db.upsert_contract_metadata(codehash, "0xdead", 1234, &meta, 1_700_000_000).unwrap();
        assert!(db.has_contract_metadata(codehash).unwrap());

        let meta2 = ContractMetadata { solc_version: Some("0.8.25".to_string()), ..meta };
        db.upsert_contract_metadata(codehash, "0xdead", 4321, &meta2, 1_700_000_500).unwrap();

        let conn = db.conn.lock().unwrap();
        let (sol, blen): (String, i64) = conn
            .query_row(
                "SELECT solc_version, bytecode_len FROM contract_metadata WHERE codehash = ?",
                params![codehash.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sol, "0.8.25");
        assert_eq!(blen, 4321);
    }

    #[test]
    fn distinct_call_frame_addresses_dedupes() {
        let db = DivergenceDatabase::in_memory().unwrap();
        for tx_index in [0u32, 1u32] {
            let drill_in = DrillInRecord {
                divergence: fixture_divergence(99, tx_index, false, 0),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: Address::repeat_byte(0xaa),
                    code_address: Some(Address::repeat_byte(0xaa)),
                    codehash: None,
                    call_type: "CALL".to_string(),
                    selector: None,
                    value_wei: None,
                    gas_provided: 0,
                    gas_used: 0,
                    gas_margin: None,
                    success: false,
                    parent_gas_at_call: None,
                    gas_requested_on_stack: None,
                    eip150_cap_binding: None,
                    deployed_bytecode_len: None,
                    repricing_gas_delta: 0,
                    ..Default::default()
                }],
                opcode_counts: vec![],
                baseline_call_frames: vec![],
                baseline_opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                recipients: vec![],
                tx_gas_results: vec![],
            };
            db.record_block_output(&output).unwrap();
        }

        let addrs = db.distinct_call_frame_addresses().unwrap();
        assert_eq!(addrs.len(), 1, "duplicate to_address should be de-duplicated");
        assert_eq!(addrs[0], format!("{:#x}", Address::repeat_byte(0xaa)));
    }

    #[test]
    fn distinct_unlabeled_call_frame_addresses_filters_labeled_codehashes() {
        use crate::contract_metadata::ContractMetadata;

        let db = DivergenceDatabase::in_memory().unwrap();
        let labeled_addr = Address::repeat_byte(0xaa);
        let labeled_codehash = B256::repeat_byte(0xcc);
        let unlabeled_addr = Address::repeat_byte(0xbb);
        let unlabeled_codehash = B256::repeat_byte(0xdd);
        let null_codehash_addr = Address::repeat_byte(0xee);

        let mut tx_index = 0u32;
        for (addr, codehash) in [
            (labeled_addr, Some(labeled_codehash)),
            (unlabeled_addr, Some(unlabeled_codehash)),
            (null_codehash_addr, None),
        ] {
            let drill_in = DrillInRecord {
                divergence: fixture_divergence(99, tx_index, false, 0),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: addr,
                    code_address: Some(addr),
                    codehash,
                    call_type: "CALL".to_string(),
                    selector: None,
                    value_wei: None,
                    gas_provided: 0,
                    gas_used: 0,
                    gas_margin: None,
                    success: false,
                    parent_gas_at_call: None,
                    gas_requested_on_stack: None,
                    eip150_cap_binding: None,
                    deployed_bytecode_len: None,
                    repricing_gas_delta: 0,
                    ..Default::default()
                }],
                opcode_counts: vec![],
                baseline_call_frames: vec![],
                baseline_opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                recipients: vec![],
                tx_gas_results: vec![],
            };
            db.record_block_output(&output).unwrap();
            tx_index += 1;
        }

        // Pre-label only `labeled_codehash`. The other two addresses should
        // remain in the unlabeled set.
        let meta = ContractMetadata {
            solc_version: Some("0.8.21".to_string()),
            solc_commit: None,
            evm_target: None,
            cbor_present: true,
            has_metadata_hash: true,
        };
        db.upsert_contract_metadata(
            labeled_codehash.0,
            &format!("{labeled_addr:#x}"),
            1234,
            &meta,
            1_700_000_000,
        )
        .unwrap();

        let unlabeled = db.distinct_unlabeled_call_frame_addresses().unwrap();
        assert_eq!(unlabeled.len(), 2);
        assert!(unlabeled.contains(&format!("{unlabeled_addr:#x}")));
        assert!(unlabeled.contains(&format!("{null_codehash_addr:#x}")));
        assert!(!unlabeled.contains(&format!("{labeled_addr:#x}")));
    }

    #[test]
    fn unlabeled_addresses_for_labels_unions_to_and_code_address() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let proxy = Address::repeat_byte(0xaa);
        let implementation = Address::repeat_byte(0xbb);
        let drill_in = DrillInRecord {
            divergence: fixture_divergence(99, 0, false, 0),
            call_frames: vec![CallFrameRow {
                call_index: 0,
                parent_call_index: None,
                depth: 0,
                from_address: Address::ZERO,
                to_address: proxy,
                code_address: Some(implementation),
                codehash: None,
                call_type: "DELEGATECALL".to_string(),
                selector: Some([0xa9, 0x05, 0x9c, 0xbb]),
                value_wei: None,
                gas_provided: 0,
                gas_used: 0,
                gas_margin: None,
                success: true,
                parent_gas_at_call: None,
                gas_requested_on_stack: None,
                eip150_cap_binding: None,
                deployed_bytecode_len: None,
                repricing_gas_delta: 0,
                ..Default::default()
            }],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 99, 1, 0),
            summaries: vec![],
            drill_ins: vec![drill_in],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        let unlabeled = db.distinct_unlabeled_addresses_for_labels().unwrap();
        assert_eq!(unlabeled.len(), 2, "both proxy and impl should be queued");
        assert!(unlabeled.contains(&format!("{proxy:#x}")));
        assert!(unlabeled.contains(&format!("{implementation:#x}")));

        // Once the proxy is labeled, only the implementation remains.
        db.upsert_contract_label(
            &format!("{proxy:#x}"),
            Some("FiatTokenProxy"),
            Some("USDC"),
            true,
            Some(&format!("{implementation:#x}")),
            "blockscout",
            1_700_000_000,
        )
        .unwrap();
        let unlabeled = db.distinct_unlabeled_addresses_for_labels().unwrap();
        assert_eq!(unlabeled.len(), 1);
        assert_eq!(unlabeled[0], format!("{implementation:#x}"));
    }

    #[test]
    fn unresolved_selectors_skips_already_resolved() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let transfer = [0xa9u8, 0x05, 0x9c, 0xbb];
        let approve = [0x09u8, 0x5e, 0xa7, 0xb3];
        for (i, sel) in [transfer, approve].iter().enumerate() {
            let drill_in = DrillInRecord {
                divergence: fixture_divergence(99, i as u32, false, 0),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: Address::repeat_byte(0xcc),
                    code_address: None,
                    codehash: None,
                    call_type: "CALL".to_string(),
                    selector: Some(*sel),
                    value_wei: None,
                    gas_provided: 0,
                    gas_used: 0,
                    gas_margin: None,
                    success: true,
                    parent_gas_at_call: None,
                    gas_requested_on_stack: None,
                    eip150_cap_binding: None,
                    deployed_bytecode_len: None,
                    repricing_gas_delta: 0,
                    ..Default::default()
                }],
                opcode_counts: vec![],
                baseline_call_frames: vec![],
                baseline_opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            db.record_block_output(&BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                recipients: vec![],
                tx_gas_results: vec![],
            })
            .unwrap();
        }

        let unresolved = db.distinct_unresolved_selectors().unwrap();
        assert_eq!(unresolved.len(), 2);

        db.upsert_function_signature(
            &transfer,
            Some("transfer(address,uint256)"),
            "openchain",
            1_700_000_000,
        )
        .unwrap();

        let unresolved = db.distinct_unresolved_selectors().unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0], approve.to_vec());
    }

    // ─────────────────────────────────────────────────────────────────────
    // Export outbox (schema v10)
    // ─────────────────────────────────────────────────────────────────────

    fn make_export(export_id: &str, payload: &[u8]) -> EncodedExportEnvelope {
        EncodedExportEnvelope {
            export_id: export_id.to_string(),
            analysis_config_hash: "0xach".to_string(),
            schedule_config_hash: "config".to_string(),
            payload_version: 1,
            payload_zstd: payload.to_vec(),
            payload_hash: keccak256(payload),
            payload_bytes: payload.len(),
        }
    }

    fn count(db: &DivergenceDatabase, sql: &str) -> i64 {
        let conn = db.conn.lock().unwrap();
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn outbox_state(db: &DivergenceDatabase, export_id: &str) -> Option<String> {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT state FROM export_outbox WHERE export_id = ?",
            params![export_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn sample_manifest(hash: &str, json: &str) -> AnalysisManifestRecord {
        AnalysisManifestRecord {
            analysis_config_hash: hash.to_string(),
            schema_version: SCHEMA_VERSION,
            chain_id: 1,
            producer_git_commit: "deadbeef".to_string(),
            replay_semantics: "canonical_pre_tx_state".to_string(),
            manifest_json: json.to_string(),
        }
    }

    #[test]
    fn fresh_db_creates_outbox_tables_and_index() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let conn = db.conn.lock().unwrap();
        for table in ["analysis_manifests", "export_outbox"] {
            let found: Option<String> = conn
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(found.as_deref(), Some(table));
        }
        let index: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_export_outbox_due'",
                [],
                |r| r.get(0),
            )
            .optional()
            .unwrap();
        assert!(index.is_some());
    }

    #[test]
    fn manifest_upsert_is_idempotent_and_rejects_changed_json() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let m = sample_manifest("0xhash1", "{\"a\":1}");
        db.upsert_analysis_manifest(&m).unwrap();
        // Same JSON → idempotent.
        db.upsert_analysis_manifest(&m).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM analysis_manifests"), 1);

        // Same hash, different JSON → invariant conflict.
        let conflicting = sample_manifest("0xhash1", "{\"a\":2}");
        let err = db.upsert_analysis_manifest(&conflicting).unwrap_err();
        assert!(matches!(err, DatabaseError::ManifestConflict { .. }));

        let loaded = db.analysis_manifest("0xhash1").unwrap().unwrap();
        assert_eq!(loaded.manifest_json, "{\"a\":1}");
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert!(db.analysis_manifest("0xmissing").unwrap().is_none());
    }

    #[test]
    fn pending_row_loads_its_original_manifest_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);
        {
            let db = DivergenceDatabase::open(&path).unwrap();
            // make_export stamps analysis_config_hash = "0xach"; store the
            // matching manifest so the pending row can resolve it later.
            db.upsert_analysis_manifest(&sample_manifest("0xach", "{\"old\":true}")).unwrap();
            db.record_block_output_with_export(
                &BlockOutput {
                    coverage: fixture_coverage("test", 10, 0, 0),
                    summaries: vec![],
                    drill_ins: vec![],
                    recipients: vec![],
                    tx_gas_results: vec![],
                },
                &make_export("e-old", b"payload"),
            )
            .unwrap();
        }
        // Reopen the same on-disk DB (simulating a restart with no live config).
        let db = DivergenceDatabase::open(&path).unwrap();
        let item = db.next_due_export(current_unix_seconds()).unwrap().unwrap();
        assert_eq!(item.export_id, "e-old");
        // The pending row's stored identity resolves to its original manifest,
        // independent of the current configuration.
        let manifest = db.analysis_manifest(&item.analysis_config_hash).unwrap().unwrap();
        assert_eq!(manifest.manifest_json, "{\"old\":true}");
    }

    #[test]
    fn analytical_rows_and_outbox_row_commit_together() {
        let db = DivergenceDatabase::in_memory().unwrap();
        db.record_block_output_with_export(
            &BlockOutput {
                coverage: fixture_coverage("test", 5, 0, 0),
                summaries: vec![],
                drill_ins: vec![],
                recipients: vec![],
                tx_gas_results: vec![],
            },
            &make_export("e-5", b"payload-5"),
        )
        .unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM block_coverage"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM export_outbox"), 1);
        assert_eq!(outbox_state(&db, "e-5").as_deref(), Some("pending"));
    }

    #[test]
    fn failed_analytical_insert_rolls_back_outbox_row() {
        let db = DivergenceDatabase::in_memory().unwrap();
        // First write a drill-in at (test, block 7, tx_index 0, config).
        let drill_in = DrillInRecord {
            divergence: fixture_divergence(7, 0, false, 1),
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 7, 1, 0),
            summaries: vec![],
            drill_ins: vec![drill_in.clone()],
            recipients: vec![],
            tx_gas_results: vec![],
        })
        .unwrap();

        // Re-recording the same divergence violates the UNIQUE constraint and
        // must fail; the accompanying outbox row must not survive.
        let err = db.record_block_output_with_export(
            &BlockOutput {
                coverage: fixture_coverage("test", 7, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                recipients: vec![],
                tx_gas_results: vec![],
            },
            &make_export("e-7", b"payload-7"),
        );
        assert!(err.is_err());
        assert_eq!(count(&db, "SELECT COUNT(*) FROM export_outbox"), 0);
    }

    #[test]
    fn same_export_id_and_hash_is_idempotent_and_preserves_exported_state() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: fixture_coverage("test", 8, 0, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        let export = make_export("e-8", b"same-payload");
        db.record_block_output_with_export(&output, &export).unwrap();
        db.mark_exported("e-8", current_unix_seconds()).unwrap();
        assert_eq!(outbox_state(&db, "e-8").as_deref(), Some("exported"));

        // Re-recording the identical payload must not resurrect the row.
        db.record_block_output_with_export(&output, &export).unwrap();
        assert_eq!(outbox_state(&db, "e-8").as_deref(), Some("exported"));
        assert_eq!(count(&db, "SELECT COUNT(*) FROM export_outbox"), 1);
    }

    #[test]
    fn changed_payload_for_same_export_id_resets_to_pending() {
        let db = DivergenceDatabase::in_memory().unwrap();
        let output = BlockOutput {
            coverage: fixture_coverage("test", 9, 0, 0),
            summaries: vec![],
            drill_ins: vec![],
            recipients: vec![],
            tx_gas_results: vec![],
        };
        db.record_block_output_with_export(&output, &make_export("e-9", b"v1")).unwrap();
        db.mark_exported("e-9", current_unix_seconds()).unwrap();
        // A different payload under the same deterministic id is an invariant
        // violation: reset to pending so the new payload gets re-exported.
        db.record_block_output_with_export(&output, &make_export("e-9", b"v2-different")).unwrap();
        assert_eq!(outbox_state(&db, "e-9").as_deref(), Some("pending"));
        let conn = db.conn.lock().unwrap();
        let bytes: i64 = conn
            .query_row("SELECT payload_bytes FROM export_outbox WHERE export_id = 'e-9'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(bytes as usize, b"v2-different".len());
    }

    #[test]
    fn due_ordering_respects_next_attempt_then_created() {
        let db = DivergenceDatabase::in_memory().unwrap();
        for (i, id) in ["e-a", "e-b", "e-c"].iter().enumerate() {
            db.record_block_output_with_export(
                &BlockOutput {
                    coverage: fixture_coverage("test", 20 + i as u64, 0, 0),
                    summaries: vec![],
                    drill_ins: vec![],
                    recipients: vec![],
                    tx_gas_results: vec![],
                },
                &make_export(id, format!("p{i}").as_bytes()),
            )
            .unwrap();
        }
        // Push e-a into the future; e-b stays due now. e-b should come first.
        db.mark_export_retry("e-a", 1, current_unix_seconds() + 10_000, "later").unwrap();
        let item = db.next_due_export(current_unix_seconds()).unwrap().unwrap();
        assert_eq!(item.export_id, "e-b");
    }

    #[test]
    fn retry_blocked_exported_and_backlog_transitions() {
        let db = DivergenceDatabase::in_memory().unwrap();
        db.record_block_output_with_export(
            &BlockOutput {
                coverage: fixture_coverage("test", 30, 0, 0),
                summaries: vec![],
                drill_ins: vec![],
                recipients: vec![],
                tx_gas_results: vec![],
            },
            &make_export("e-30", b"0123456789"),
        )
        .unwrap();

        let now = current_unix_seconds();
        let stats = db.export_backlog_stats(now).unwrap();
        assert_eq!(stats.pending_count, 1);
        assert_eq!(stats.pending_bytes, 10);
        assert_eq!(stats.blocked_count, 0);

        // Retry: still counts as pending backlog.
        db.mark_export_retry("e-30", 2, now + 5, "boom").unwrap();
        assert_eq!(db.export_backlog_stats(now).unwrap().pending_count, 1);
        // Not due yet (next_attempt_at in the future).
        assert!(db.next_due_export(now).unwrap().is_none());

        // Blocked: leaves the pending backlog, counted as blocked.
        db.mark_export_blocked("e-30", "too big").unwrap();
        let stats = db.export_backlog_stats(now).unwrap();
        assert_eq!(stats.pending_count, 0);
        assert_eq!(stats.blocked_count, 1);

        // Exported + prune.
        db.mark_exported("e-30", now - 100).unwrap();
        assert_eq!(db.prune_exported_before(now).unwrap(), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM export_outbox"), 0);
    }

    #[test]
    fn delete_block_range_drops_pending_but_keeps_exported() {
        let db = DivergenceDatabase::in_memory().unwrap();
        for (id, block, exported) in [("e-keep", 40u64, true), ("e-drop", 41u64, false)] {
            db.record_block_output_with_export(
                &BlockOutput {
                    coverage: fixture_coverage("test", block, 0, 0),
                    summaries: vec![],
                    drill_ins: vec![],
                    recipients: vec![],
                    tx_gas_results: vec![],
                },
                &make_export(id, id.as_bytes()),
            )
            .unwrap();
            if exported {
                db.mark_exported(id, current_unix_seconds()).unwrap();
            }
        }
        let counts = db.delete_block_range(40, 41).unwrap();
        assert_eq!(counts.outbox_pending, 1);
        // Exported audit row survives; pending row is gone.
        assert_eq!(outbox_state(&db, "e-keep").as_deref(), Some("exported"));
        assert!(outbox_state(&db, "e-drop").is_none());
    }
}
