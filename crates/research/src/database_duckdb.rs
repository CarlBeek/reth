//! DuckDB-backed storage for the new research data model.
//!
//! Implements the schema described in `crates/research/docs/storage-redesign.md`:
//! per-block aggregates for the bucketed cohort (wallet-fixable / gas-only /
//! trace-only / unchanged) and per-tx drill-in rows for the event-logs-changed
//! and contract-broken cohorts. The legacy SQLite database in
//! [`super::database`] stays the default path until the consumer dashboard has
//! migrated; this module is gated behind the `research-duckdb` feature.
//!
//! This module currently lands the **schema only** — `open` initialises every
//! table, sequence, and index, and stamps an `analysis_runs` row recording the
//! producer version. Write methods for `block_coverage`, `block_summaries`,
//! `divergences`, etc. land in a follow-up PR once the producer pipeline
//! switches over.
//!
//! Schema-version policy (per the doc): the producer refuses to open a DB
//! whose latest `analysis_runs.schema_version` doesn't match its compiled-in
//! version. No migration shims — a major schema change is a full re-replay.

use duckdb::{params, Connection};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use thiserror::Error;

/// Compiled-in schema version. Bumped on any column add/remove/type change.
///
/// Stored on each `analysis_runs` row; on open we verify the latest row
/// matches and reject DB files written by a different version (the doc's
/// "no migration shims; major schema change is a full re-replay" rule).
pub const SCHEMA_VERSION: u32 = 1;

/// Errors raised by the DuckDB-backed storage layer.
#[derive(Debug, Error)]
pub enum DuckDbDatabaseError {
    /// Underlying DuckDB driver error.
    #[error("DuckDB error: {0}")]
    Duck(#[from] duckdb::Error),
    /// I/O error opening or creating the database file.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
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

/// DuckDB-backed divergence storage.
///
/// `Clone` is cheap — the underlying connection is wrapped in
/// `Arc<Mutex<_>>`, mirroring the SQLite path. Backfill workers and the live
/// arm can hold their own handles to the same DB without contending on
/// open/close.
#[derive(Debug, Clone)]
pub struct DuckDbDivergenceDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl DuckDbDivergenceDatabase {
    /// Open (or create) a DuckDB database at `path`. Initialises the schema
    /// if the file is new and verifies the version of any existing data.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DuckDbDatabaseError> {
        let conn = Connection::open(path.as_ref())?;
        Self::initialize(conn)
    }

    /// Open an in-memory DuckDB database. Used in tests; on shutdown the
    /// data is discarded.
    pub fn in_memory() -> Result<Self, DuckDbDatabaseError> {
        let conn = Connection::open_in_memory()?;
        Self::initialize(conn)
    }

    fn initialize(conn: Connection) -> Result<Self, DuckDbDatabaseError> {
        initialize_schema(&conn)?;
        verify_schema_version(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
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
    ) -> Result<u64, DuckDbDatabaseError> {
        let conn = self.conn.lock().expect("DuckDB connection mutex poisoned");
        let started_at = current_unix_seconds();
        let run_id: i64 = conn.query_row(
            "INSERT INTO analysis_runs (
                schema_version, schedule_name, schedule_config_hash,
                reth_commit, run_started_at, notes
             ) VALUES (?, ?, ?, ?, ?, ?)
             RETURNING run_id",
            params![
                SCHEMA_VERSION,
                schedule_name,
                schedule_config_hash,
                reth_commit,
                started_at as i64,
                notes,
            ],
            |row| row.get(0),
        )?;
        Ok(run_id as u64)
    }

    /// Mark an analysis run as finished, recording the wall-clock end time
    /// and the total block count processed.
    pub fn record_analysis_run_end(
        &self,
        run_id: u64,
        blocks_processed: u64,
    ) -> Result<(), DuckDbDatabaseError> {
        let conn = self.conn.lock().expect("DuckDB connection mutex poisoned");
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
/// [`DuckDbDivergenceDatabase::record_analysis_run_start`] is called.
fn verify_schema_version(conn: &Connection) -> Result<(), DuckDbDatabaseError> {
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
        .ok();
    match latest {
        Some(found) if found != SCHEMA_VERSION => {
            Err(DuckDbDatabaseError::SchemaVersionMismatch { expected: SCHEMA_VERSION, found })
        }
        _ => Ok(()),
    }
}

/// Apply the full DDL. Idempotent via `CREATE TABLE IF NOT EXISTS` and
/// matching guards for sequences and indexes.
fn initialize_schema(conn: &Connection) -> Result<(), DuckDbDatabaseError> {
    // One row per (schedule, block). Always emitted, even for blocks with
    // zero divergences, so coverage joins work.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_coverage (
            schedule_name        VARCHAR NOT NULL,
            schedule_config_hash VARCHAR NOT NULL,
            block_number         UBIGINT NOT NULL,
            block_hash           BLOB    NOT NULL,
            parent_hash          BLOB    NOT NULL,
            timestamp            UBIGINT NOT NULL,
            tx_count             UINTEGER NOT NULL,
            tx_count_unchanged                 UINTEGER NOT NULL,
            tx_count_trace_only                UINTEGER NOT NULL,
            tx_count_gas_only                  UINTEGER NOT NULL,
            tx_count_event_logs_changed        UINTEGER NOT NULL,
            tx_count_wallet_fixable_shallow    UINTEGER NOT NULL,
            tx_count_wallet_fixable_deep_chain UINTEGER NOT NULL,
            tx_count_contract_broken           UINTEGER NOT NULL,
            PRIMARY KEY (schedule_name, block_number, block_hash)
        );",
    )?;

    // One row per (schedule, block, bucket). Only emitted for buckets that
    // had at least one tx in the block.
    //
    // `opcode_count_totals_7904` / `opcode_gas_delta_totals_7904` are
    // sparse — a tx that ran 5 unique opcodes contributes 5 entries.
    // `multiplier_log2_hist` and `gas_delta_log2_hist` are fixed 12-bin
    // arrays so the CDF charts read pre-binned data.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS block_summaries (
            schedule_name VARCHAR NOT NULL,
            block_number  UBIGINT NOT NULL,
            bucket        VARCHAR NOT NULL,
            tx_count      UINTEGER NOT NULL,
            gas_delta_sum         BIGINT,
            gas_delta_sum_sq      HUGEINT,
            gas_delta_min         BIGINT,
            gas_delta_max         BIGINT,
            gas_delta_log2_hist   INTEGER[12],
            opcode_count_totals_7904 STRUCT(opcode UTINYINT, count UBIGINT)[],
            opcode_gas_delta_totals_7904 STRUCT(opcode UTINYINT, delta BIGINT)[],
            state_gas_sum           UBIGINT,
            state_gas_spillover_sum UBIGINT,
            multiplier_log2_hist    INTEGER[12],
            tx_count_creation              UINTEGER,
            tx_count_authorization         UINTEGER,
            tx_count_runtime_state         UINTEGER,
            tx_count_no_state              UINTEGER,
            PRIMARY KEY (schedule_name, block_number, bucket)
        );",
    )?;

    // Surrogate divergence_id (sequence-based) so child tables can FK to a
    // single column. Natural key is (schedule_name, block_number, tx_index,
    // schedule_config_hash); a UNIQUE constraint enforces it.
    conn.execute_batch("CREATE SEQUENCE IF NOT EXISTS seq_divergence_id START 1;")?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergences (
            divergence_id        UBIGINT PRIMARY KEY DEFAULT nextval('seq_divergence_id'),
            schedule_name        VARCHAR NOT NULL,
            schedule_config_hash VARCHAR NOT NULL,
            block_number         UBIGINT NOT NULL,
            tx_index             UINTEGER NOT NULL,
            tx_hash              BLOB NOT NULL,
            timestamp            UBIGINT NOT NULL,
            bucket               VARCHAR NOT NULL,

            sender    VARCHAR NOT NULL,
            recipient VARCHAR,
            is_create BOOLEAN NOT NULL,
            tx_gas_limit UBIGINT NOT NULL,

            baseline_success BOOLEAN NOT NULL,
            schedule_success BOOLEAN NOT NULL,
            status_changed       BOOLEAN NOT NULL,
            event_logs_changed   BOOLEAN NOT NULL,
            output_changed       BOOLEAN NOT NULL,
            logs_bloom_changed   BOOLEAN NOT NULL,

            baseline_gas_used     UBIGINT NOT NULL,
            schedule_gas_used     UBIGINT NOT NULL,
            gas_delta             BIGINT NOT NULL,
            baseline_total_gas_spent UBIGINT,
            baseline_gas_refunded    UBIGINT,
            schedule_total_gas_spent UBIGINT,
            schedule_gas_refunded    UBIGINT,
            schedule_intrinsic_gas   UBIGINT,
            schedule_floor_gas       UBIGINT,
            would_fit_in_original_limit BOOLEAN,
            min_multiplier_to_succeed   DOUBLE,

            divergence_contract  VARCHAR,
            divergence_pc        UINTEGER,
            divergence_call_depth INTEGER,
            divergence_opcode    UTINYINT,
            oog_contract         VARCHAR,
            oog_pc               UINTEGER,
            oog_call_depth       INTEGER,
            oog_opcode           UTINYINT,
            oog_pattern          VARCHAR,
            oog_gas_remaining    UBIGINT,
            oog_chain_proportional BOOLEAN,
            oog_bottleneck_depth   INTEGER,
            oog_bottleneck_kind    VARCHAR,

            schedule_state_gas_spent     UBIGINT,
            schedule_initial_state_gas   UBIGINT,
            schedule_initial_reservoir   UBIGINT,
            runtime_state_gas            UBIGINT,
            runtime_state_gas_spillover  UBIGINT,
            state_gas_category           VARCHAR,
            reservoir_exhausted          BOOLEAN,

            UNIQUE (schedule_name, block_number, tx_index, schedule_config_hash)
        );",
    )?;

    // One row per call frame in the schedule trace of a drill-in
    // divergence. Baseline frames are NOT stored separately — derived
    // baseline costs live in divergence_opcode_counts.gas_baseline.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_call_frames (
            divergence_id        UBIGINT NOT NULL REFERENCES divergences(divergence_id),
            call_index           UINTEGER NOT NULL,
            parent_call_index    UINTEGER,
            depth                UINTEGER NOT NULL,
            from_address         VARCHAR NOT NULL,
            to_address           VARCHAR NOT NULL,
            code_address         VARCHAR,
            codehash             BLOB,
            call_type            VARCHAR NOT NULL,
            selector             BLOB,
            value_wei            VARCHAR,
            gas_provided         UBIGINT NOT NULL,
            gas_used             UBIGINT NOT NULL,
            gas_margin           BIGINT,
            success              BOOLEAN NOT NULL,
            parent_gas_at_call       UBIGINT,
            gas_requested_on_stack   UBIGINT,
            eip150_cap_binding       BOOLEAN,
            state_gas_running    UBIGINT,
            PRIMARY KEY (divergence_id, call_index)
        );",
    )?;

    // Sparse opcode counts keyed by frame. Zero rows omitted by the
    // producer at insert time. FK to (divergence_id, call_index) so a
    // single drop of a divergence cleans up all its rows.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_opcode_counts (
            divergence_id   UBIGINT NOT NULL,
            call_index      UINTEGER NOT NULL,
            opcode          UTINYINT NOT NULL,
            count           UBIGINT NOT NULL,
            gas_baseline    UBIGINT NOT NULL,
            gas_schedule    UBIGINT NOT NULL,
            PRIMARY KEY (divergence_id, call_index, opcode),
            FOREIGN KEY (divergence_id, call_index)
                REFERENCES divergence_call_frames(divergence_id, call_index)
        );",
    )?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS divergence_event_logs (
            divergence_id  UBIGINT NOT NULL REFERENCES divergences(divergence_id),
            trace_kind     VARCHAR NOT NULL,
            log_index      UINTEGER NOT NULL,
            address        VARCHAR NOT NULL,
            topic0         BLOB,
            topic1         BLOB,
            topic2         BLOB,
            topic3         BLOB,
            data_bytes     BLOB,
            data_hash      BLOB,
            PRIMARY KEY (divergence_id, trace_kind, log_index)
        );",
    )?;

    // Static contract metadata, keyed by codehash so identical
    // implementations dedupe across addresses. Backfilled by the
    // contract-metadata-backfill subcommand.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS contract_metadata (
            codehash               BLOB PRIMARY KEY,
            representative_address VARCHAR,
            solc_version           VARCHAR,
            solc_commit            VARCHAR,
            evm_target             VARCHAR,
            cbor_present           BOOLEAN NOT NULL,
            has_metadata_hash      BOOLEAN NOT NULL,
            bytecode_len           UINTEGER NOT NULL,
            extracted_at           UBIGINT NOT NULL
        );",
    )?;

    // Per-replay-run manifest. A consumer reads the latest row to detect
    // "is this lake written by the current code or do I need to migrate?".
    conn.execute_batch(
        "CREATE SEQUENCE IF NOT EXISTS seq_analysis_run_id START 1;
        CREATE TABLE IF NOT EXISTS analysis_runs (
            run_id               UBIGINT PRIMARY KEY DEFAULT nextval('seq_analysis_run_id'),
            schema_version       UINTEGER NOT NULL,
            schedule_name        VARCHAR NOT NULL,
            schedule_config_hash VARCHAR NOT NULL,
            reth_commit          VARCHAR,
            run_started_at       UBIGINT NOT NULL,
            run_finished_at      UBIGINT,
            blocks_processed     UBIGINT,
            notes                VARCHAR
        );",
    )?;

    // Indexes. DuckDB doesn't strictly need indexes for analytical scans,
    // but the per-recipient / per-codehash / per-bucket lookups (contract
    // pages, clustering queries) benefit.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_div_schedule      ON divergences(schedule_name);
         CREATE INDEX IF NOT EXISTS idx_div_block         ON divergences(schedule_name, block_number);
         CREATE INDEX IF NOT EXISTS idx_div_recipient     ON divergences(recipient);
         CREATE INDEX IF NOT EXISTS idx_div_bucket        ON divergences(bucket);
         CREATE INDEX IF NOT EXISTS idx_dcf_to_addr       ON divergence_call_frames(to_address);
         CREATE INDEX IF NOT EXISTS idx_dcf_codehash      ON divergence_call_frames(codehash);
         CREATE INDEX IF NOT EXISTS idx_doc_opcode        ON divergence_opcode_counts(opcode);
         CREATE INDEX IF NOT EXISTS idx_bs_schedule_block ON block_summaries(schedule_name, block_number);",
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
        let db = DuckDbDivergenceDatabase::in_memory().unwrap();
        let conn = db.conn.lock().unwrap();

        // The DDL is idempotent — calling initialize_schema again should
        // succeed without errors.
        initialize_schema(&conn).expect("initialize_schema is idempotent");

        // Every expected table is present in `duckdb_tables()`.
        let mut stmt =
            conn.prepare("SELECT table_name FROM duckdb_tables() ORDER BY table_name").unwrap();
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
        let db = DuckDbDivergenceDatabase::in_memory().unwrap();
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

    /// DuckDB refuses to open an empty file (it treats one as a corrupt
    /// existing DB), so on-disk tests need a path that doesn't yet exist
    /// inside a TempDir.
    fn fresh_db_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("test.duckdb")
    }

    #[test]
    fn schema_version_mismatch_is_detected() {
        // Open a fresh DB and forge an analysis_runs row written under a
        // newer schema version. The next open should reject it.
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_db_path(&dir);

        {
            let db = DuckDbDivergenceDatabase::open(&path).unwrap();
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

        let err = DuckDbDivergenceDatabase::open(&path).unwrap_err();
        match err {
            DuckDbDatabaseError::SchemaVersionMismatch { expected, found } => {
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
        let _db = DuckDbDivergenceDatabase::open(fresh_db_path(&dir)).unwrap();
    }
}
