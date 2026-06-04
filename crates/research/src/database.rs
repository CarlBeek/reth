//! SQLite-backed storage for the research data model.
//!
//! Implements the schema described in `crates/research/docs/storage-redesign.md`:
//! per-block aggregates for the bucketed cohort (wallet-fixable / gas-only /
//! trace-only / unchanged) and per-tx drill-in rows for the event-logs-changed,
//! inconclusive, and contract-broken cohorts.
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

use crate::divergence::{Bucket, EventLog, FrameOpcodeCounts};
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
/// - v10: bump so the `tx_count_aa_gas_reestimation` column is properly version-gated. A database
///   written under an older version is detected by [`verify_schema_version`] as a mismatch and
///   re-replayed, rather than silently failing every write.
/// - v11: added block-capacity columns `block_coverage.block_gas_used` / `block_gas_limit` (the
///   native header values — utilization, fullness and gas-limit-exceedance analysis) and the
///   `block_bucket_recipients` rollup table (per-(block,bucket) top recipients + 4-byte selector,
///   ranked/truncated by `tx_count` with the tail folded into an `__other__` row) so the two
///   aggregate-only cohorts (`gas_only`, `wallet_fixable_shallow`) gain protocol/selector
///   attribution they previously lacked. All additions are nullable / new-table, so a pre-v11
///   database is forward-compatible after an additive `ALTER TABLE … ADD COLUMN`; rows for blocks
///   not re-touched simply read NULL.
pub const SCHEMA_VERSION: u32 = 11;

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
    /// `SCHEMA_VERSION`. Replay is required.
    #[error(
        "schema version mismatch: producer compiled for v{expected}, \
         database last written under v{found}. Re-replay required."
    )]
    SchemaVersionMismatch {
        /// Version the running binary expects.
        expected: u32,
        /// Version found in the existing database.
        found: u32,
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
            tx_count_unchanged                 INTEGER NOT NULL,
            tx_count_trace_only                INTEGER NOT NULL,
            tx_count_gas_only                  INTEGER NOT NULL,
            tx_count_event_logs_changed        INTEGER NOT NULL,
            tx_count_schedule_rescued          INTEGER NOT NULL,
            tx_count_wallet_fixable_shallow    INTEGER NOT NULL,
            tx_count_wallet_fixable_deep_chain INTEGER NOT NULL,
            tx_count_inconclusive_needs_higher_sweep INTEGER NOT NULL,
            tx_count_contract_broken           INTEGER NOT NULL,
            tx_count_aa_gas_reestimation       INTEGER NOT NULL DEFAULT 0,
            -- Native block header values (NULL on rows written before v11 / not
            -- re-touched). gas_used is the actual mainnet baseline the block
            -- consumed; gas_limit is the protocol cap. Together they anchor
            -- utilization, block-fullness shift, and gas-limit-exceedance under
            -- a repricing schedule. Nullable so an additive ADD COLUMN on an
            -- existing DB leaves historical rows readable as 'n/a'.
            block_gas_used                     INTEGER,
            block_gas_limit                    INTEGER,
            PRIMARY KEY (schedule_name, block_number, block_hash)
        );",
    )?;

    // One row per (schedule, block, bucket). Only emitted for buckets that
    // had at least one tx in the block.
    //
    // `opcode_totals_7904` is a sparse JSON array of
    // `{opcode, count, gas_baseline, gas_schedule}` objects — one entry per
    // distinct opcode the bucket touched in this block, summed across every
    // frame of every tx. Lets the dashboard compute both the share of gas
    // each opcode burned and the delta the schedule introduced.
    // `multiplier_log2_hist` and `gas_delta_log2_hist` are JSON arrays of
    // exactly 12 ints (fixed-size
    // log2 bins) so the CDF charts read pre-binned data.
    //
    // `gas_delta_sum_sq` is REAL (loses precision past 2^53) rather than
    // a hypothetical i128: variance/stddev derived from it are already
    // approximate.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_summaries (
            schedule_name TEXT    NOT NULL,
            block_number  INTEGER NOT NULL,
            bucket        TEXT    NOT NULL,
            tx_count      INTEGER NOT NULL,
            gas_delta_sum       INTEGER,
            gas_delta_sum_sq    REAL,
            gas_delta_min       INTEGER,
            gas_delta_max       INTEGER,
            gas_delta_log2_hist TEXT,
            opcode_totals_7904      TEXT,
            state_gas_sum           INTEGER,
            state_gas_spillover_sum INTEGER,
            multiplier_log2_hist    TEXT,
            tx_count_creation       INTEGER,
            tx_count_authorization  INTEGER,
            tx_count_runtime_state  INTEGER,
            tx_count_no_state       INTEGER,
            PRIMARY KEY (schedule_name, block_number, bucket)
        );",
    )?;

    // Per-(schedule, block, bucket) recipient attribution for the
    // aggregate-only cohorts that have no per-tx rows (`gas_only`,
    // `wallet_fixable_shallow`, …). One row per distinct destination, ranked
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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_bucket_recipients (
            schedule_name TEXT    NOT NULL,
            block_number  INTEGER NOT NULL,
            bucket        TEXT    NOT NULL,
            recipient     TEXT    NOT NULL,
            top_selector  BLOB    NOT NULL,
            tx_count      INTEGER NOT NULL,
            gas_delta_sum_succeeding INTEGER NOT NULL,
            PRIMARY KEY (schedule_name, block_number, bucket, recipient, top_selector)
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
            bucket               TEXT    NOT NULL,

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
            gas_delta                INTEGER NOT NULL,
            baseline_total_gas_spent INTEGER,
            baseline_gas_refunded    INTEGER,
            schedule_total_gas_spent INTEGER,
            schedule_gas_refunded    INTEGER,
            schedule_intrinsic_gas   INTEGER,
            schedule_floor_gas       INTEGER,
            would_fit_in_original_limit INTEGER,
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

            UNIQUE (schedule_name, block_number, tx_index, schedule_config_hash)
        );",
    )?;

    // One row per call frame in the schedule trace of a drill-in
    // divergence. Baseline frames are NOT stored separately — derived
    // baseline costs live in divergence_opcode_counts.gas_baseline.
    //
    // FK constraints from child tables to `divergences` are documented in
    // the design doc but omitted at the DDL level; the producer maintains
    // referential integrity at the application layer via the per-block
    // transactional delete pattern.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_call_frames (
            divergence_id          INTEGER NOT NULL,
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
            state_gas_running      INTEGER,
            deployed_bytecode_len  INTEGER,
            PRIMARY KEY (divergence_id, call_index)
        );",
    )?;

    // Sparse opcode counts keyed by frame. Zero rows omitted by the
    // producer at insert time.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_opcode_counts (
            divergence_id INTEGER NOT NULL,
            call_index    INTEGER NOT NULL,
            opcode        INTEGER NOT NULL,
            count         INTEGER NOT NULL,
            gas_baseline  INTEGER NOT NULL,
            gas_schedule  INTEGER NOT NULL,
            PRIMARY KEY (divergence_id, call_index, opcode)
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
         CREATE INDEX IF NOT EXISTS idx_div_recipient     ON divergences(recipient);
         CREATE INDEX IF NOT EXISTS idx_div_tx_hash       ON divergences(tx_hash);
         CREATE INDEX IF NOT EXISTS idx_div_bucket        ON divergences(bucket);
         CREATE INDEX IF NOT EXISTS idx_div_sched_bucket_recipient
             ON divergences(schedule_name, bucket, recipient);
         CREATE INDEX IF NOT EXISTS idx_div_sched_bucket_oog
             ON divergences(schedule_name, bucket, oog_call_depth, oog_bottleneck_kind);
         CREATE INDEX IF NOT EXISTS idx_div_sched_bucket_opcode
             ON divergences(schedule_name, bucket, divergence_opcode);
         CREATE INDEX IF NOT EXISTS idx_dcf_to_addr       ON divergence_call_frames(to_address);
         CREATE INDEX IF NOT EXISTS idx_dcf_codehash      ON divergence_call_frames(codehash);
         CREATE INDEX IF NOT EXISTS idx_doc_opcode        ON divergence_opcode_counts(opcode);
         CREATE INDEX IF NOT EXISTS idx_bc_schedule_block ON block_coverage(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_block ON block_summaries(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_bucket ON block_summaries(schedule_name, bucket);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_opcode_totals
             ON block_summaries(schedule_name)
             WHERE opcode_totals_7904 IS NOT NULL AND opcode_totals_7904 <> '[]';
         CREATE INDEX IF NOT EXISTS idx_bbr_sched_bucket_recipient
             ON block_bucket_recipients(schedule_name, bucket, recipient);",
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
#[derive(Debug, Clone)]
pub struct BlockOutput {
    /// Always present. Counts go to `block_coverage`.
    pub coverage: BlockCoverageRow,
    /// One per non-empty bucket. Go to `block_summaries`.
    pub summaries: Vec<BlockSummaryRow>,
    /// Drill-in records — one per `EventLogsChanged`, `InconclusiveNeedsHigherSweep`, or
    /// `ContractBroken` tx.
    pub drill_ins: Vec<DrillInRecord>,
    /// Per-(bucket) top-recipient rollup rows. Go to `block_bucket_recipients`.
    pub bucket_recipients: Vec<BucketRecipientRow>,
}

/// One row of the `block_bucket_recipients` rollup: a single destination's
/// share of a (schedule, block, bucket). See the table DDL for the ranking /
/// truncation rules and the `gas_delta_sum_succeeding` semantics.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct BucketRecipientRow {
    pub schedule_name: String,
    pub block_number: u64,
    pub bucket: Bucket,
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
#[derive(Debug, Clone)]
pub struct BlockCoverageRow {
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub tx_count: u32,
    pub tx_count_unchanged: u32,
    pub tx_count_trace_only: u32,
    pub tx_count_gas_only: u32,
    pub tx_count_event_logs_changed: u32,
    pub tx_count_schedule_rescued: u32,
    pub tx_count_wallet_fixable_shallow: u32,
    pub tx_count_wallet_fixable_deep_chain: u32,
    pub tx_count_inconclusive_needs_higher_sweep: u32,
    pub tx_count_contract_broken: u32,
    pub tx_count_aa_gas_reestimation: u32,
    /// Native block header `gas_used` — the actual gas the block consumed on
    /// mainnet (the baseline for utilization and the repricing-tax fraction).
    pub block_gas_used: u64,
    /// Native block header `gas_limit` — the protocol cap, for block-fullness
    /// and gas-limit-exceedance analysis under a repricing schedule.
    pub block_gas_limit: u64,
}

/// Per-opcode totals for one (block, bucket) row in `block_summaries`,
/// emitted as JSON array. Sparse — only opcodes that actually executed
/// in this bucket on this block appear.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize)]
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

/// Aggregate summary for one (schedule, block, bucket).
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct BlockSummaryRow {
    pub schedule_name: String,
    pub block_number: u64,
    pub bucket: Bucket,
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
    /// Per-opcode totals across this bucket in this block. Empty when
    /// no opcodes ran (e.g. the aggregator has nothing to fold in).
    /// JSON-encoded in the DB as a sparse list.
    pub opcode_totals_7904: Vec<OpcodeBucketTotal>,
    pub state_gas_sum: Option<u64>,
    pub state_gas_spillover_sum: Option<u64>,
    pub multiplier_log2_hist: Option<[i32; 12]>,
    pub tx_count_creation: Option<u32>,
    pub tx_count_authorization: Option<u32>,
    pub tx_count_runtime_state: Option<u32>,
    pub tx_count_no_state: Option<u32>,
}

/// Per-tx drill-in record: the `divergences` row plus its dependent
/// `divergence_call_frames`, `divergence_opcode_counts`, and
/// `divergence_event_logs` rows. Emitted only for the
/// `EventLogsChanged`, `InconclusiveNeedsHigherSweep`, and
/// `ContractBroken` buckets — aggregate buckets collapse into
/// `BlockSummaryRow`.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct DrillInRecord {
    pub divergence: DivergenceRow,
    pub call_frames: Vec<CallFrameRow>,
    /// Per-frame opcode counts. The producer omits zero-count rows; this
    /// vec is what `FrameOpcodeCounts::nonzero()` yielded for the tx.
    pub opcode_counts: Vec<OpcodeCountRow>,
    pub baseline_event_logs: Vec<EventLog>,
    pub schedule_event_logs: Vec<EventLog>,
}

/// One row destined for `divergences`. Most fields mirror the column
/// names; the `schema_version` / `divergence_id` / timestamp are filled in by
/// the writer.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct DivergenceRow {
    pub schedule_name: String,
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub tx_index: u32,
    pub tx_hash: B256,
    pub timestamp: u64,
    pub bucket: Bucket,

    pub sender: Address,
    pub recipient: Option<Address>,
    pub is_create: bool,
    pub tx_gas_limit: u64,

    pub baseline_success: bool,
    pub schedule_success: bool,
    pub status_changed: bool,
    pub event_logs_changed: bool,
    pub output_changed: bool,
    pub logs_bloom_changed: bool,

    pub baseline_gas_used: u64,
    pub schedule_gas_used: u64,
    pub gas_delta: i64,
    pub baseline_total_gas_spent: Option<u64>,
    pub baseline_gas_refunded: Option<u64>,
    pub schedule_total_gas_spent: Option<u64>,
    pub schedule_gas_refunded: Option<u64>,
    pub schedule_intrinsic_gas: Option<u64>,
    pub schedule_floor_gas: Option<u64>,
    pub would_fit_in_original_limit: Option<bool>,
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
}

/// One frame row destined for `divergence_call_frames`. `call_index` and
/// `parent_call_index` match the inspector's frame-open order (root = 0).
#[allow(missing_docs)]
#[derive(Debug, Clone)]
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
    pub gas_margin: Option<i64>,
    pub success: bool,
    pub parent_gas_at_call: Option<u64>,
    pub gas_requested_on_stack: Option<u64>,
    pub eip150_cap_binding: Option<bool>,
    pub state_gas_running: Option<u64>,
    /// Length of the deployed bytecode in bytes. Set only on successful
    /// CREATE / CREATE2 frames (`call_type` is `CREATE`/`CREATE2` and
    /// `success` is true); `None` everywhere else. Used by the EIP-8037
    /// dashboard to plot true deployment cost without approximating
    /// bytecode size from baseline gas.
    pub deployed_bytecode_len: Option<u32>,
}

/// One row destined for `divergence_opcode_counts`. Producer omits zero-
/// count rows.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
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
        let mut conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let tx = conn.transaction()?;

        insert_block_coverage(&tx, &output.coverage)?;
        for summary in &output.summaries {
            insert_block_summary(&tx, summary)?;
        }
        for recipient in &output.bucket_recipients {
            insert_bucket_recipient(&tx, recipient)?;
        }
        for drill_in in &output.drill_ins {
            let divergence_id = insert_divergence(&tx, &drill_in.divergence)?;
            for frame in &drill_in.call_frames {
                insert_call_frame(&tx, divergence_id, frame)?;
            }
            for opc in &drill_in.opcode_counts {
                insert_opcode_count(&tx, divergence_id, opc)?;
            }
            for log in &drill_in.baseline_event_logs {
                insert_event_log(&tx, divergence_id, "baseline", log)?;
            }
            for log in &drill_in.schedule_event_logs {
                insert_event_log(&tx, divergence_id, "schedule", log)?;
            }
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
        let summaries_deleted = tx.execute(
            "DELETE FROM block_summaries WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        let bucket_recipients_deleted = tx.execute(
            "DELETE FROM block_bucket_recipients WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        let coverage_deleted = tx.execute(
            "DELETE FROM block_coverage WHERE block_number >= ? AND block_number <= ?",
            params![from_block as i64, to_block as i64],
        )?;
        tx.commit()?;

        Ok(BlockRangeDeleteCounts {
            coverage: coverage_deleted,
            summaries: summaries_deleted,
            bucket_recipients: bucket_recipients_deleted,
            divergences: divergences_deleted,
            call_frames: call_frames_deleted,
            opcode_counts: opcode_counts_deleted,
            event_logs: event_logs_deleted,
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

    /// Distinct `to_address` values seen across `divergence_call_frames`.
    /// Backfill iterates these, fetches bytecode from reth state, hashes
    /// it, and upserts a `contract_metadata` row keyed by codehash.
    pub fn distinct_call_frame_addresses(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT to_address FROM divergence_call_frames
             WHERE to_address IS NOT NULL AND to_address <> ''
             ORDER BY to_address",
        )?;
        let rows =
            stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Addresses with at least one call frame whose codehash is not yet
    /// represented in `contract_metadata`. Drives the periodic incremental
    /// backfill so each tick only re-fetches bytecode for genuinely new
    /// (or never-labeled) contracts. NULL `codehash` values match too —
    /// those rows need a fresh fetch to derive a codehash from current
    /// state.
    pub fn distinct_unlabeled_call_frame_addresses(&self) -> Result<Vec<String>, DatabaseError> {
        let conn = self.conn.lock().expect("SQLite connection mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT DISTINCT dcf.to_address
             FROM divergence_call_frames dcf
             LEFT JOIN contract_metadata cm ON cm.codehash = dcf.codehash
             WHERE dcf.to_address IS NOT NULL AND dcf.to_address <> ''
               AND cm.codehash IS NULL
             ORDER BY dcf.to_address",
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

/// Per-table row counts returned by
/// [`DivergenceDatabase::delete_block_range`]. Used for logging.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BlockRangeDeleteCounts {
    pub coverage: usize,
    pub summaries: usize,
    pub bucket_recipients: usize,
    pub divergences: usize,
    pub call_frames: usize,
    pub opcode_counts: usize,
    pub event_logs: usize,
}

fn insert_block_coverage(
    tx: &Transaction<'_>,
    row: &BlockCoverageRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO block_coverage (
            schedule_name, schedule_config_hash, block_number, block_hash,
            parent_hash, timestamp, tx_count,
            tx_count_unchanged, tx_count_trace_only, tx_count_gas_only,
            tx_count_event_logs_changed, tx_count_schedule_rescued,
            tx_count_wallet_fixable_shallow,
            tx_count_wallet_fixable_deep_chain,
            tx_count_inconclusive_needs_higher_sweep,
            tx_count_contract_broken,
            tx_count_aa_gas_reestimation,
            block_gas_used, block_gas_limit
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, block_hash) DO UPDATE SET
            schedule_config_hash               = excluded.schedule_config_hash,
            parent_hash                        = excluded.parent_hash,
            timestamp                          = excluded.timestamp,
            tx_count                           = excluded.tx_count,
            tx_count_unchanged                 = excluded.tx_count_unchanged,
            tx_count_trace_only                = excluded.tx_count_trace_only,
            tx_count_gas_only                  = excluded.tx_count_gas_only,
            tx_count_event_logs_changed        = excluded.tx_count_event_logs_changed,
            tx_count_schedule_rescued          = excluded.tx_count_schedule_rescued,
            tx_count_wallet_fixable_shallow    = excluded.tx_count_wallet_fixable_shallow,
            tx_count_wallet_fixable_deep_chain = excluded.tx_count_wallet_fixable_deep_chain,
            tx_count_inconclusive_needs_higher_sweep =
                excluded.tx_count_inconclusive_needs_higher_sweep,
            tx_count_contract_broken           = excluded.tx_count_contract_broken,
            tx_count_aa_gas_reestimation       = excluded.tx_count_aa_gas_reestimation,
            block_gas_used                     = excluded.block_gas_used,
            block_gas_limit                    = excluded.block_gas_limit",
        params![
            row.schedule_name,
            row.schedule_config_hash,
            row.block_number as i64,
            row.block_hash.as_slice(),
            row.parent_hash.as_slice(),
            row.timestamp as i64,
            row.tx_count as i64,
            row.tx_count_unchanged as i64,
            row.tx_count_trace_only as i64,
            row.tx_count_gas_only as i64,
            row.tx_count_event_logs_changed as i64,
            row.tx_count_schedule_rescued as i64,
            row.tx_count_wallet_fixable_shallow as i64,
            row.tx_count_wallet_fixable_deep_chain as i64,
            row.tx_count_inconclusive_needs_higher_sweep as i64,
            row.tx_count_contract_broken as i64,
            row.tx_count_aa_gas_reestimation as i64,
            row.block_gas_used as i64,
            row.block_gas_limit as i64,
        ],
    )?;
    Ok(())
}

/// Insert one `block_bucket_recipients` rollup row. Upsert on the natural key
/// so re-touching a block (idempotent re-replay) refreshes counts in place.
fn insert_bucket_recipient(
    tx: &Transaction<'_>,
    row: &BucketRecipientRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO block_bucket_recipients (
            schedule_name, block_number, bucket, recipient, top_selector,
            tx_count, gas_delta_sum_succeeding
        ) VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, bucket, recipient, top_selector)
        DO UPDATE SET
            tx_count                 = excluded.tx_count,
            gas_delta_sum_succeeding = excluded.gas_delta_sum_succeeding",
        params![
            row.schedule_name,
            row.block_number as i64,
            row.bucket.as_str(),
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
    let opcode_totals_7904 = serde_json::to_string(&row.opcode_totals_7904)?;
    let gas_delta_sum_sq_real = row.gas_delta_sum_sq.map(|v| v as f64);

    tx.execute(
        "INSERT INTO block_summaries (
            schedule_name, block_number, bucket, tx_count,
            gas_delta_sum, gas_delta_sum_sq, gas_delta_min, gas_delta_max,
            gas_delta_log2_hist,
            opcode_totals_7904,
            state_gas_sum, state_gas_spillover_sum,
            multiplier_log2_hist,
            tx_count_creation, tx_count_authorization,
            tx_count_runtime_state, tx_count_no_state
        ) VALUES (?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?,
                  ?,
                  ?, ?,
                  ?,
                  ?, ?, ?, ?)
        ON CONFLICT (schedule_name, block_number, bucket) DO UPDATE SET
            tx_count                = excluded.tx_count,
            gas_delta_sum           = excluded.gas_delta_sum,
            gas_delta_sum_sq        = excluded.gas_delta_sum_sq,
            gas_delta_min           = excluded.gas_delta_min,
            gas_delta_max           = excluded.gas_delta_max,
            gas_delta_log2_hist     = excluded.gas_delta_log2_hist,
            opcode_totals_7904      = excluded.opcode_totals_7904,
            state_gas_sum           = excluded.state_gas_sum,
            state_gas_spillover_sum = excluded.state_gas_spillover_sum,
            multiplier_log2_hist    = excluded.multiplier_log2_hist,
            tx_count_creation       = excluded.tx_count_creation,
            tx_count_authorization  = excluded.tx_count_authorization,
            tx_count_runtime_state  = excluded.tx_count_runtime_state,
            tx_count_no_state       = excluded.tx_count_no_state",
        params![
            row.schedule_name,
            row.block_number as i64,
            row.bucket.as_str(),
            row.tx_count as i64,
            row.gas_delta_sum,
            gas_delta_sum_sq_real,
            row.gas_delta_min,
            row.gas_delta_max,
            gas_delta_log2_hist,
            opcode_totals_7904,
            row.state_gas_sum.map(|v| v as i64),
            row.state_gas_spillover_sum.map(|v| v as i64),
            multiplier_log2_hist,
            row.tx_count_creation.map(|v| v as i64),
            row.tx_count_authorization.map(|v| v as i64),
            row.tx_count_runtime_state.map(|v| v as i64),
            row.tx_count_no_state.map(|v| v as i64),
        ],
    )?;
    Ok(())
}

fn insert_divergence(tx: &Transaction<'_>, row: &DivergenceRow) -> Result<u64, DatabaseError> {
    tx.execute(
        "INSERT INTO divergences (
            schedule_name, schedule_config_hash, block_number, tx_index, tx_hash,
            timestamp, bucket,
            sender, recipient, is_create, tx_gas_limit,
            baseline_success, schedule_success,
            status_changed, event_logs_changed, output_changed, logs_bloom_changed,
            baseline_gas_used, schedule_gas_used, gas_delta,
            baseline_total_gas_spent, baseline_gas_refunded,
            schedule_total_gas_spent, schedule_gas_refunded,
            schedule_intrinsic_gas, schedule_floor_gas,
            would_fit_in_original_limit, min_multiplier_to_succeed,
            divergence_contract, divergence_pc, divergence_call_depth, divergence_opcode,
            oog_contract, oog_pc, oog_call_depth, oog_opcode, oog_pattern, oog_gas_remaining,
            oog_chain_proportional, oog_bottleneck_depth, oog_bottleneck_kind,
            schedule_state_gas_spent, schedule_state_gas_demanded,
            schedule_initial_state_gas, schedule_initial_reservoir,
            runtime_state_gas, runtime_state_gas_spillover,
            state_gas_category, reservoir_exhausted,
            replay_halt_oog
        ) VALUES (?, ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?,
                  ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?,
                  ?, ?, ?, ?,
                  ?, ?, ?, ?, ?, ?, ?,
                  ?)",
        params![
            row.schedule_name,
            row.schedule_config_hash,
            row.block_number as i64,
            row.tx_index as i64,
            row.tx_hash.as_slice(),
            row.timestamp as i64,
            row.bucket.as_str(),
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
            row.would_fit_in_original_limit,
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
        ],
    )?;
    Ok(tx.last_insert_rowid() as u64)
}

fn insert_call_frame(
    tx: &Transaction<'_>,
    divergence_id: u64,
    row: &CallFrameRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO divergence_call_frames (
            divergence_id, call_index, parent_call_index, depth,
            from_address, to_address, code_address, codehash, call_type,
            selector, value_wei, gas_provided, gas_used, gas_margin,
            success, parent_gas_at_call, gas_requested_on_stack,
            eip150_cap_binding, state_gas_running, deployed_bytecode_len
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            divergence_id as i64,
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
            row.state_gas_running.map(|v| v as i64),
            row.deployed_bytecode_len.map(|v| v as i64),
        ],
    )?;
    Ok(())
}

fn insert_opcode_count(
    tx: &Transaction<'_>,
    divergence_id: u64,
    row: &OpcodeCountRow,
) -> Result<(), DatabaseError> {
    tx.execute(
        "INSERT INTO divergence_opcode_counts (
            divergence_id, call_index, opcode, count, gas_baseline, gas_schedule
        ) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            divergence_id as i64,
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
            "analysis_runs",
            "block_coverage",
            "block_summaries",
            "contract_metadata",
            "divergence_call_frames",
            "divergence_event_logs",
            "divergence_opcode_counts",
            "divergences",
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

    /// Minimal `BlockCoverageRow` fixture for write-path tests.
    fn fixture_coverage(
        schedule: &str,
        block: u64,
        broken: u32,
        log_changed: u32,
    ) -> BlockCoverageRow {
        BlockCoverageRow {
            schedule_name: schedule.to_string(),
            schedule_config_hash: "config".to_string(),
            block_number: block,
            block_hash: B256::repeat_byte(0xb0 + (block as u8 & 0x0f)),
            parent_hash: B256::repeat_byte(0xa0),
            timestamp: 1_700_000_000 + block,
            tx_count: broken + log_changed,
            tx_count_unchanged: 0,
            tx_count_trace_only: 0,
            tx_count_gas_only: 0,
            tx_count_event_logs_changed: log_changed,
            tx_count_schedule_rescued: 0,
            tx_count_wallet_fixable_shallow: 0,
            tx_count_wallet_fixable_deep_chain: 0,
            tx_count_inconclusive_needs_higher_sweep: 0,
            tx_count_contract_broken: broken,
            tx_count_aa_gas_reestimation: 0,
            block_gas_used: 15_000_000,
            block_gas_limit: 30_000_000,
        }
    }

    /// Minimal `DivergenceRow` fixture for write-path tests. All optional
    /// columns left as `None`.
    fn fixture_divergence(
        block: u64,
        tx_index: u32,
        bucket: Bucket,
        gas_delta: i64,
    ) -> DivergenceRow {
        DivergenceRow {
            schedule_name: "test".to_string(),
            schedule_config_hash: "config".to_string(),
            block_number: block,
            tx_index,
            tx_hash: B256::repeat_byte(0xdd),
            timestamp: 1_700_000_000,
            bucket,
            sender: Address::repeat_byte(0x11),
            recipient: Some(Address::repeat_byte(0x22)),
            is_create: false,
            tx_gas_limit: 500_000,
            baseline_success: true,
            schedule_success: matches!(bucket, Bucket::EventLogsChanged),
            status_changed: matches!(bucket, Bucket::ContractBroken),
            event_logs_changed: matches!(bucket, Bucket::EventLogsChanged),
            output_changed: false,
            logs_bloom_changed: false,
            baseline_gas_used: 100_000,
            schedule_gas_used: (100_000i64 + gas_delta).max(0) as u64,
            gas_delta,
            baseline_total_gas_spent: None,
            baseline_gas_refunded: None,
            schedule_total_gas_spent: None,
            schedule_gas_refunded: None,
            schedule_intrinsic_gas: None,
            schedule_floor_gas: None,
            would_fit_in_original_limit: None,
            min_multiplier_to_succeed: None,
            divergence_contract: None,
            divergence_pc: None,
            divergence_call_depth: None,
            divergence_opcode: None,
            oog_contract: None,
            oog_pc: None,
            oog_call_depth: None,
            oog_opcode: None,
            oog_pattern: None,
            oog_gas_remaining: None,
            oog_chain_proportional: None,
            oog_bottleneck_depth: None,
            oog_bottleneck_kind: None,
            schedule_state_gas_spent: None,
            schedule_state_gas_demanded: None,
            schedule_initial_state_gas: None,
            schedule_initial_reservoir: None,
            runtime_state_gas: None,
            runtime_state_gas_spillover: None,
            state_gas_category: None,
            reservoir_exhausted: None,
            replay_halt_oog: None,
        }
    }

    #[test]
    fn record_block_output_writes_coverage_summaries_and_drill_in() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let drill_in = DrillInRecord {
            divergence: fixture_divergence(100, 0, Bucket::ContractBroken, 12_345),
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
                state_gas_running: None,
                deployed_bytecode_len: None,
            }],
            opcode_counts: vec![OpcodeCountRow {
                call_index: 0,
                opcode: 0x55,
                count: 3,
                gas_baseline: 60_000,
                gas_schedule: 75_000,
            }],
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
                bucket: Bucket::ContractBroken,
                tx_count: 1,
                gas_delta_sum: Some(12_345),
                gas_delta_sum_sq: Some(12_345i64 * 12_345i64),
                gas_delta_min: Some(12_345),
                gas_delta_max: Some(12_345),
                gas_delta_log2_hist: Some([0; 12]),
                opcode_totals_7904: vec![OpcodeBucketTotal {
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
            }],
            drill_ins: vec![drill_in],
            bucket_recipients: vec![],
        };

        db.record_block_output(&output).unwrap();

        let conn = db.conn.lock().unwrap();

        let (tx_count, broken): (i64, i64) = conn
            .query_row(
                "SELECT tx_count, tx_count_contract_broken
                 FROM block_coverage WHERE block_number = 100",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tx_count, 1);
        assert_eq!(broken, 1);

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

    #[test]
    fn delete_block_range_clears_all_per_block_tables() {
        let db = DivergenceDatabase::in_memory().unwrap();

        for block in [100u64, 200u64] {
            let drill_in = DrillInRecord {
                divergence: fixture_divergence(block, 0, Bucket::ContractBroken, 100),
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
                    state_gas_running: None,
                    deployed_bytecode_len: None,
                }],
                opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", block, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                bucket_recipients: vec![],
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
            bucket_recipients: vec![],
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
                divergence: fixture_divergence(99, tx_index, Bucket::ContractBroken, 0),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: Address::repeat_byte(0xaa),
                    code_address: None,
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
                    state_gas_running: None,
                    deployed_bytecode_len: None,
                }],
                opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                bucket_recipients: vec![],
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
                divergence: fixture_divergence(99, tx_index, Bucket::ContractBroken, 0),
                call_frames: vec![CallFrameRow {
                    call_index: 0,
                    parent_call_index: None,
                    depth: 0,
                    from_address: Address::ZERO,
                    to_address: addr,
                    code_address: None,
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
                    state_gas_running: None,
                    deployed_bytecode_len: None,
                }],
                opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            let output = BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                bucket_recipients: vec![],
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
            divergence: fixture_divergence(99, 0, Bucket::ContractBroken, 0),
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
                state_gas_running: None,
                deployed_bytecode_len: None,
            }],
            opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        };
        db.record_block_output(&BlockOutput {
            coverage: fixture_coverage("test", 99, 1, 0),
            summaries: vec![],
            drill_ins: vec![drill_in],
            bucket_recipients: vec![],
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
                divergence: fixture_divergence(99, i as u32, Bucket::ContractBroken, 0),
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
                    state_gas_running: None,
                    deployed_bytecode_len: None,
                }],
                opcode_counts: vec![],
                baseline_event_logs: vec![],
                schedule_event_logs: vec![],
            };
            db.record_block_output(&BlockOutput {
                coverage: fixture_coverage("test", 99, 1, 0),
                summaries: vec![],
                drill_ins: vec![drill_in],
                bucket_recipients: vec![],
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
}
