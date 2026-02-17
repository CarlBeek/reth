//! Database for storing divergence data.

use crate::divergence::{CallFrame, Divergence, DivergenceType, EventLog};
use alloy_primitives::B256;
use rusqlite::{params, Connection};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use thiserror::Error;

/// Errors that can occur when working with the divergence database.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// SQLite database error
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// I/O error
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON serialization/deserialization error
    #[error("Serialization error: {0}")]
    Serialization(String),

    /// Database has not been initialized
    #[error("Database not initialized")]
    NotInitialized,
}

/// Result of comparing a transaction under a specific gas schedule.
#[derive(Debug, Clone)]
pub struct ScheduleDivergence {
    /// Name of the gas schedule that caused this divergence
    pub schedule_name: String,
    /// Block number
    pub block_number: u64,
    /// Transaction index within block
    pub tx_index: u64,
    /// Transaction hash
    pub tx_hash: B256,
    /// Block timestamp
    pub timestamp: u64,
    /// Type of divergence
    pub divergence_type: DivergenceType,

    /// Baseline execution succeeded
    pub baseline_success: bool,
    /// Baseline gas used
    pub baseline_gas_used: u64,
    /// Baseline intrinsic gas
    pub baseline_intrinsic_gas: u64,

    /// Schedule execution succeeded (simulated)
    pub schedule_success: bool,
    /// Additional gas charged under this schedule
    pub schedule_gas_used: u64,
    /// Schedule intrinsic gas (if modified)
    pub schedule_intrinsic_gas: Option<u64>,

    /// Gas delta (baseline - schedule, positive = savings)
    pub gas_delta: i64,
    /// Gas efficiency ratio
    pub gas_efficiency_ratio: Option<f64>,

    /// Transaction category (for EIP-2780 style schedules)
    pub tx_category: Option<String>,
    /// Affected opcodes (JSON array)
    pub affected_opcodes: Option<String>,
    /// Affected precompiles (JSON array)
    pub affected_precompiles: Option<String>,

    /// Out-of-gas info (JSON)
    pub oog_info: Option<String>,
    /// Divergence location (JSON)
    pub divergence_location: Option<String>,
    /// Operation counts (JSON)
    pub operation_counts: Option<String>,
}

/// Database for storing divergence data.
#[derive(Debug, Clone)]
pub struct DivergenceDatabase {
    conn: Arc<Mutex<Connection>>,
}

impl DivergenceDatabase {
    /// Open or create a database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, DatabaseError> {
        let conn = Connection::open(path)?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.initialize_schema()?;
        Ok(db)
    }

    /// Create an in-memory database (for testing).
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, DatabaseError> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn: Arc::new(Mutex::new(conn)) };
        db.initialize_schema()?;
        Ok(db)
    }

    /// Initialize the database schema.
    fn initialize_schema(&self) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().unwrap();

        // Main divergences table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS divergences (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                block_number INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                tx_hash BLOB NOT NULL,
                timestamp INTEGER NOT NULL,

                -- Divergence classification (comma-separated types)
                divergence_types TEXT NOT NULL,

                -- Gas analysis
                normal_gas_used INTEGER NOT NULL,
                experimental_gas_used INTEGER NOT NULL,
                gas_efficiency_ratio REAL NOT NULL,

                -- Operation counts (normal)
                normal_sload_count INTEGER,
                normal_sstore_count INTEGER,
                normal_call_count INTEGER,
                normal_log_count INTEGER,
                normal_total_ops INTEGER,
                normal_memory_words INTEGER,
                normal_create_count INTEGER,

                -- Operation counts (experimental)
                exp_sload_count INTEGER,
                exp_sstore_count INTEGER,
                exp_call_count INTEGER,
                exp_log_count INTEGER,
                exp_total_ops INTEGER,
                exp_memory_words INTEGER,
                exp_create_count INTEGER,

                -- Divergence location
                divergence_contract BLOB,
                divergence_function_selector BLOB,
                divergence_function_selectors_json TEXT,
                divergence_pc INTEGER,
                divergence_call_depth INTEGER,
                divergence_opcode INTEGER,
                divergence_opcode_name TEXT,

                -- OOG analysis
                oog_occurred BOOLEAN,
                oog_opcode INTEGER,
                oog_opcode_name TEXT,
                oog_pc INTEGER,
                oog_contract BLOB,
                oog_call_depth INTEGER,
                oog_gas_remaining INTEGER,
                oog_pattern TEXT,

                created_at INTEGER DEFAULT (strftime('%s', 'now'))
            )",
            [],
        )?;

        // Indexes
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_divergences_block ON divergences(block_number)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_divergences_types ON divergences(divergence_types)",
            [],
        )?;

        // Call trees table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS call_trees (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                divergence_id INTEGER NOT NULL,
                is_experimental BOOLEAN NOT NULL,
                call_index INTEGER NOT NULL,
                depth INTEGER NOT NULL,
                from_addr BLOB NOT NULL,
                to_addr BLOB,
                call_type TEXT NOT NULL,
                gas_provided INTEGER,
                gas_used INTEGER,
                success BOOLEAN,
                input BLOB,
                output BLOB,
                FOREIGN KEY (divergence_id) REFERENCES divergences(id) ON DELETE CASCADE
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_call_trees_divergence ON call_trees(divergence_id)",
            [],
        )?;

        // Event logs table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS event_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                divergence_id INTEGER NOT NULL,
                is_experimental BOOLEAN NOT NULL,
                log_index INTEGER NOT NULL,
                contract_address BLOB NOT NULL,
                topic0 BLOB,
                topic1 BLOB,
                topic2 BLOB,
                topic3 BLOB,
                data BLOB,
                FOREIGN KEY (divergence_id) REFERENCES divergences(id) ON DELETE CASCADE
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_event_logs_divergence ON event_logs(divergence_id)",
            [],
        )?;

        // Gas loops table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS gas_loops (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                block_number INTEGER NOT NULL,
                tx_hash BLOB NOT NULL,
                contract_address BLOB NOT NULL,
                function_selector BLOB,
                first_seen_block INTEGER NOT NULL,
                gas_threshold INTEGER,
                loop_pattern TEXT,
                created_at INTEGER DEFAULT (strftime('%s', 'now'))
            )",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_gas_loops_contract ON gas_loops(contract_address)",
            [],
        )?;

        // Multi-schedule divergences table (new schema)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schedule_divergences (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_name TEXT NOT NULL,
                block_number INTEGER NOT NULL,
                tx_index INTEGER NOT NULL,
                tx_hash BLOB NOT NULL,
                timestamp INTEGER NOT NULL,

                -- Divergence classification
                divergence_type TEXT NOT NULL,

                -- Baseline execution
                baseline_success BOOLEAN NOT NULL,
                baseline_gas_used INTEGER NOT NULL,
                baseline_intrinsic_gas INTEGER NOT NULL,

                -- Schedule execution
                schedule_success BOOLEAN NOT NULL,
                schedule_gas_used INTEGER NOT NULL,
                schedule_intrinsic_gas INTEGER,

                -- Analysis
                gas_delta INTEGER NOT NULL,
                gas_efficiency_ratio REAL,

                -- Context
                tx_category TEXT,
                affected_opcodes TEXT,
                affected_precompiles TEXT,

                -- Detailed info (optional based on trace_detail)
                oog_info TEXT,
                divergence_location TEXT,
                operation_counts TEXT,

                created_at INTEGER DEFAULT (strftime('%s', 'now'))
            )",
            [],
        )?;

        // Indexes for multi-schedule divergences
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sched_div_schedule ON schedule_divergences(schedule_name)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sched_div_block ON schedule_divergences(block_number)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sched_div_type ON schedule_divergences(divergence_type)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sched_div_category ON schedule_divergences(tx_category)",
            [],
        )?;

        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_sched_div_tx ON schedule_divergences(tx_hash)",
            [],
        )?;

        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_sched_div_unique
             ON schedule_divergences(schedule_name, block_number, tx_index, tx_hash)",
            [],
        )?;

        // Schedule statistics table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS schedule_stats (
                schedule_name TEXT PRIMARY KEY,
                total_transactions INTEGER NOT NULL DEFAULT 0,
                total_divergences INTEGER NOT NULL DEFAULT 0,
                status_divergences INTEGER NOT NULL DEFAULT 0,
                total_gas_saved INTEGER NOT NULL DEFAULT 0,
                total_gas_increase INTEGER NOT NULL DEFAULT 0,
                last_block_processed INTEGER,
                updated_at INTEGER
            )",
            [],
        )?;

        Ok(())
    }

    /// Record a divergence.
    pub fn record_divergence(&self, divergence: &Divergence) -> Result<i64, DatabaseError> {
        let conn = self.conn.lock().unwrap();

        // Format divergence types as comma-separated string
        let types_str =
            divergence.divergence_types.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(",");

        // Prepare function selector fields
        let deepest_selector = divergence
            .divergence_location
            .as_ref()
            .and_then(|l| l.function_selectors.last().and_then(|s| *s));

        let selectors_json = divergence
            .divergence_location
            .as_ref()
            .map(|l| serde_json::to_string(&l.function_selectors).unwrap_or_default());

        let _divergence_id = conn.execute(
            "INSERT INTO divergences (
                block_number, tx_index, tx_hash, timestamp,
                divergence_types,
                normal_gas_used, experimental_gas_used, gas_efficiency_ratio,
                normal_sload_count, normal_sstore_count, normal_call_count,
                normal_log_count, normal_total_ops, normal_memory_words, normal_create_count,
                exp_sload_count, exp_sstore_count, exp_call_count,
                exp_log_count, exp_total_ops, exp_memory_words, exp_create_count,
                divergence_contract, divergence_function_selector, divergence_function_selectors_json, divergence_pc,
                divergence_call_depth, divergence_opcode, divergence_opcode_name,
                oog_occurred, oog_opcode, oog_opcode_name, oog_pc,
                oog_contract, oog_call_depth, oog_gas_remaining, oog_pattern
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28,
                ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37
            )",
            params![
                divergence.block_number,
                divergence.tx_index,
                divergence.tx_hash.as_slice(),
                divergence.timestamp,
                types_str,
                divergence.gas_analysis.normal_gas_used,
                divergence.gas_analysis.experimental_gas_used,
                divergence.gas_analysis.gas_efficiency_ratio,
                divergence.normal_ops.sload_count,
                divergence.normal_ops.sstore_count,
                divergence.normal_ops.call_count,
                divergence.normal_ops.log_count,
                divergence.normal_ops.total_ops,
                divergence.normal_ops.memory_words_allocated,
                divergence.normal_ops.create_count,
                divergence.experimental_ops.sload_count,
                divergence.experimental_ops.sstore_count,
                divergence.experimental_ops.call_count,
                divergence.experimental_ops.log_count,
                divergence.experimental_ops.total_ops,
                divergence.experimental_ops.memory_words_allocated,
                divergence.experimental_ops.create_count,
                divergence.divergence_location.as_ref().map(|l| l.contract.as_slice()),
                deepest_selector.as_ref().map(|s| s.as_slice()),
                selectors_json,
                divergence.divergence_location.as_ref().map(|l| l.pc as i64),
                divergence.divergence_location.as_ref().map(|l| l.call_depth as i64),
                divergence.divergence_location.as_ref().map(|l| l.opcode as i64),
                divergence.divergence_location.as_ref().map(|l| l.opcode_name.as_str()),
                divergence.oog_info.is_some(),
                divergence.oog_info.as_ref().map(|o| o.opcode as i64),
                divergence.oog_info.as_ref().map(|o| o.opcode_name.as_str()),
                divergence.oog_info.as_ref().map(|o| o.pc as i64),
                divergence.oog_info.as_ref().map(|o| o.contract.as_slice()),
                divergence.oog_info.as_ref().map(|o| o.call_depth as i64),
                divergence.oog_info.as_ref().map(|o| o.gas_remaining as i64),
                divergence.oog_info.as_ref().map(|o| o.pattern.to_string()),
            ],
        )?;

        let divergence_id = conn.last_insert_rowid();

        // Store call trees if present
        if let Some(ref call_trees) = divergence.call_trees {
            for (is_experimental, frames) in
                [(false, &call_trees.normal), (true, &call_trees.experimental)]
            {
                for frame in frames {
                    self.insert_call_frame(&conn, divergence_id, is_experimental, frame)?;
                }
            }
        }

        // Store event logs if present
        if let Some(ref event_logs) = divergence.event_logs {
            for (is_experimental, logs) in
                [(false, &event_logs.normal), (true, &event_logs.experimental)]
            {
                for log in logs {
                    self.insert_event_log(&conn, divergence_id, is_experimental, log)?;
                }
            }
        }

        Ok(divergence_id)
    }

    /// Insert a call frame.
    fn insert_call_frame(
        &self,
        conn: &Connection,
        divergence_id: i64,
        is_experimental: bool,
        frame: &CallFrame,
    ) -> Result<(), DatabaseError> {
        conn.execute(
            "INSERT INTO call_trees (
                divergence_id, is_experimental, call_index, depth,
                from_addr, to_addr, call_type, gas_provided,
                gas_used, success, input, output
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                divergence_id,
                is_experimental,
                frame.call_index,
                frame.depth,
                frame.from.as_slice(),
                frame.to.as_ref().map(|a| a.as_slice()),
                frame.call_type.to_string(),
                frame.gas_provided,
                frame.gas_used,
                frame.success,
                frame.input.as_ref().map(|b| b.as_ref()),
                frame.output.as_ref().map(|b| b.as_ref()),
            ],
        )?;

        Ok(())
    }

    /// Insert an event log.
    fn insert_event_log(
        &self,
        conn: &Connection,
        divergence_id: i64,
        is_experimental: bool,
        log: &EventLog,
    ) -> Result<(), DatabaseError> {
        conn.execute(
            "INSERT INTO event_logs (
                divergence_id, is_experimental, log_index, contract_address,
                topic0, topic1, topic2, topic3, data
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                divergence_id,
                is_experimental,
                log.log_index,
                log.address.as_slice(),
                log.topics.get(0).map(|t| t.as_slice()),
                log.topics.get(1).map(|t| t.as_slice()),
                log.topics.get(2).map(|t| t.as_slice()),
                log.topics.get(3).map(|t| t.as_slice()),
                log.data.as_ref(),
            ],
        )?;

        Ok(())
    }

    /// Get divergence count by block range.
    pub fn count_divergences(&self, from_block: u64, to_block: u64) -> Result<u64, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM divergences WHERE block_number >= ?1 AND block_number <= ?2",
            params![from_block, to_block],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Get divergence count by type.
    pub fn count_by_type(&self, dtype: DivergenceType) -> Result<u64, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", dtype);
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM divergences WHERE divergence_types LIKE ?1",
            params![pattern],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    // ==================== Multi-Schedule Methods ====================

    /// Record a schedule-specific divergence.
    pub fn record_schedule_divergence(
        &self,
        divergence: &ScheduleDivergence,
    ) -> Result<i64, DatabaseError> {
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO schedule_divergences (
                schedule_name, block_number, tx_index, tx_hash, timestamp,
                divergence_type,
                baseline_success, baseline_gas_used, baseline_intrinsic_gas,
                schedule_success, schedule_gas_used, schedule_intrinsic_gas,
                gas_delta, gas_efficiency_ratio,
                tx_category, affected_opcodes, affected_precompiles,
                oog_info, divergence_location, operation_counts
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20
            )
            ON CONFLICT(schedule_name, block_number, tx_index, tx_hash) DO UPDATE SET
                timestamp = excluded.timestamp,
                divergence_type = excluded.divergence_type,
                baseline_success = excluded.baseline_success,
                baseline_gas_used = excluded.baseline_gas_used,
                baseline_intrinsic_gas = excluded.baseline_intrinsic_gas,
                schedule_success = excluded.schedule_success,
                schedule_gas_used = excluded.schedule_gas_used,
                schedule_intrinsic_gas = excluded.schedule_intrinsic_gas,
                gas_delta = excluded.gas_delta,
                gas_efficiency_ratio = excluded.gas_efficiency_ratio,
                tx_category = excluded.tx_category,
                affected_opcodes = excluded.affected_opcodes,
                affected_precompiles = excluded.affected_precompiles,
                oog_info = excluded.oog_info,
                divergence_location = excluded.divergence_location,
                operation_counts = excluded.operation_counts",
            params![
                divergence.schedule_name,
                divergence.block_number,
                divergence.tx_index,
                divergence.tx_hash.as_slice(),
                divergence.timestamp,
                divergence.divergence_type.to_string(),
                divergence.baseline_success,
                divergence.baseline_gas_used,
                divergence.baseline_intrinsic_gas,
                divergence.schedule_success,
                divergence.schedule_gas_used,
                divergence.schedule_intrinsic_gas,
                divergence.gas_delta,
                divergence.gas_efficiency_ratio,
                divergence.tx_category,
                divergence.affected_opcodes,
                divergence.affected_precompiles,
                divergence.oog_info,
                divergence.divergence_location,
                divergence.operation_counts,
            ],
        )?;

        Ok(conn.last_insert_rowid())
    }

    /// Record multiple schedule divergences in a batch.
    pub fn record_schedule_divergences_batch(
        &self,
        divergences: &[ScheduleDivergence],
    ) -> Result<usize, DatabaseError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "INSERT INTO schedule_divergences (
                schedule_name, block_number, tx_index, tx_hash, timestamp,
                divergence_type,
                baseline_success, baseline_gas_used, baseline_intrinsic_gas,
                schedule_success, schedule_gas_used, schedule_intrinsic_gas,
                gas_delta, gas_efficiency_ratio,
                tx_category, affected_opcodes, affected_precompiles,
                oog_info, divergence_location, operation_counts
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20
            )
            ON CONFLICT(schedule_name, block_number, tx_index, tx_hash) DO UPDATE SET
                timestamp = excluded.timestamp,
                divergence_type = excluded.divergence_type,
                baseline_success = excluded.baseline_success,
                baseline_gas_used = excluded.baseline_gas_used,
                baseline_intrinsic_gas = excluded.baseline_intrinsic_gas,
                schedule_success = excluded.schedule_success,
                schedule_gas_used = excluded.schedule_gas_used,
                schedule_intrinsic_gas = excluded.schedule_intrinsic_gas,
                gas_delta = excluded.gas_delta,
                gas_efficiency_ratio = excluded.gas_efficiency_ratio,
                tx_category = excluded.tx_category,
                affected_opcodes = excluded.affected_opcodes,
                affected_precompiles = excluded.affected_precompiles,
                oog_info = excluded.oog_info,
                divergence_location = excluded.divergence_location,
                operation_counts = excluded.operation_counts",
        )?;
        let mut count = 0;
        for divergence in divergences {
            stmt.execute(params![
                divergence.schedule_name,
                divergence.block_number,
                divergence.tx_index,
                divergence.tx_hash.as_slice(),
                divergence.timestamp,
                divergence.divergence_type.to_string(),
                divergence.baseline_success,
                divergence.baseline_gas_used,
                divergence.baseline_intrinsic_gas,
                divergence.schedule_success,
                divergence.schedule_gas_used,
                divergence.schedule_intrinsic_gas,
                divergence.gas_delta,
                divergence.gas_efficiency_ratio,
                divergence.tx_category,
                divergence.affected_opcodes,
                divergence.affected_precompiles,
                divergence.oog_info,
                divergence.divergence_location,
                divergence.operation_counts,
            ])?;
            count += 1;
        }

        drop(stmt);
        tx.commit()?;
        Ok(count)
    }

    /// Get divergence count by schedule name.
    pub fn count_by_schedule(&self, schedule_name: &str) -> Result<u64, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schedule_divergences WHERE schedule_name = ?1",
            params![schedule_name],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Get divergence count by schedule and block range.
    pub fn count_schedule_divergences(
        &self,
        schedule_name: &str,
        from_block: u64,
        to_block: u64,
    ) -> Result<u64, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM schedule_divergences
             WHERE schedule_name = ?1 AND block_number >= ?2 AND block_number <= ?3",
            params![schedule_name, from_block, to_block],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

    /// Delete schedule divergences in an inclusive block range.
    pub fn delete_schedule_divergences_in_block_range(
        &self,
        from_block: u64,
        to_block: u64,
    ) -> Result<usize, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM schedule_divergences WHERE block_number >= ?1 AND block_number <= ?2",
            params![from_block, to_block],
        )?;
        Ok(deleted)
    }

    /// Get total gas delta for a schedule.
    pub fn total_gas_delta_for_schedule(&self, schedule_name: &str) -> Result<i64, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COALESCE(SUM(gas_delta), 0) FROM schedule_divergences WHERE schedule_name = ?1",
            params![schedule_name],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    /// Get divergence counts grouped by schedule.
    pub fn divergence_counts_by_schedule(&self) -> Result<Vec<(String, u64)>, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT schedule_name, COUNT(*) as count
             FROM schedule_divergences
             GROUP BY schedule_name
             ORDER BY count DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            let name: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            Ok((name, count as u64))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Get divergence counts grouped by category for a schedule.
    pub fn divergence_counts_by_category(
        &self,
        schedule_name: &str,
    ) -> Result<Vec<(String, u64, i64)>, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tx_category, COUNT(*) as count, COALESCE(SUM(gas_delta), 0) as total_delta
             FROM schedule_divergences
             WHERE schedule_name = ?1 AND tx_category IS NOT NULL
             GROUP BY tx_category
             ORDER BY count DESC",
        )?;

        let rows = stmt.query_map(params![schedule_name], |row| {
            let category: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let delta: i64 = row.get(2)?;
            Ok((category, count as u64, delta))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Update schedule statistics.
    pub fn update_schedule_stats(
        &self,
        schedule_name: &str,
        transactions: u64,
        divergences: u64,
        status_divergences: u64,
        gas_saved: i64,
        gas_increase: i64,
        last_block: u64,
    ) -> Result<(), DatabaseError> {
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO schedule_stats (
                schedule_name, total_transactions, total_divergences,
                status_divergences, total_gas_saved, total_gas_increase,
                last_block_processed, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s', 'now'))
            ON CONFLICT(schedule_name) DO UPDATE SET
                total_transactions = total_transactions + excluded.total_transactions,
                total_divergences = total_divergences + excluded.total_divergences,
                status_divergences = status_divergences + excluded.status_divergences,
                total_gas_saved = total_gas_saved + excluded.total_gas_saved,
                total_gas_increase = total_gas_increase + excluded.total_gas_increase,
                last_block_processed = excluded.last_block_processed,
                updated_at = strftime('%s', 'now')",
            params![
                schedule_name,
                transactions,
                divergences,
                status_divergences,
                gas_saved,
                gas_increase,
                last_block
            ],
        )?;

        Ok(())
    }

    /// Get statistics for a schedule.
    pub fn get_schedule_stats(
        &self,
        schedule_name: &str,
    ) -> Result<Option<ScheduleStats>, DatabaseError> {
        let conn = self.conn.lock().unwrap();

        let result = conn.query_row(
            "SELECT total_transactions, total_divergences, status_divergences,
                    total_gas_saved, total_gas_increase, last_block_processed
             FROM schedule_stats WHERE schedule_name = ?1",
            params![schedule_name],
            |row| {
                Ok(ScheduleStats {
                    schedule_name: schedule_name.to_string(),
                    total_transactions: row.get::<_, i64>(0)? as u64,
                    total_divergences: row.get::<_, i64>(1)? as u64,
                    status_divergences: row.get::<_, i64>(2)? as u64,
                    total_gas_saved: row.get(3)?,
                    total_gas_increase: row.get(4)?,
                    last_block_processed: row.get::<_, Option<i64>>(5)?.map(|b| b as u64),
                })
            },
        );

        match result {
            Ok(stats) => Ok(Some(stats)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all schedule statistics.
    pub fn get_all_schedule_stats(&self) -> Result<Vec<ScheduleStats>, DatabaseError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT schedule_name, total_transactions, total_divergences,
                    status_divergences, total_gas_saved, total_gas_increase,
                    last_block_processed
             FROM schedule_stats
             ORDER BY total_divergences DESC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ScheduleStats {
                schedule_name: row.get(0)?,
                total_transactions: row.get::<_, i64>(1)? as u64,
                total_divergences: row.get::<_, i64>(2)? as u64,
                status_divergences: row.get::<_, i64>(3)? as u64,
                total_gas_saved: row.get(4)?,
                total_gas_increase: row.get(5)?,
                last_block_processed: row.get::<_, Option<i64>>(6)?.map(|b| b as u64),
            })
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }
}

/// Statistics for a gas schedule.
#[derive(Debug, Clone)]
pub struct ScheduleStats {
    /// Schedule name
    pub schedule_name: String,
    /// Total transactions analyzed
    pub total_transactions: u64,
    /// Total divergences found
    pub total_divergences: u64,
    /// Divergences where execution status changed
    pub status_divergences: u64,
    /// Total gas saved (positive deltas)
    pub total_gas_saved: i64,
    /// Total gas increase (negative deltas, stored as positive)
    pub total_gas_increase: i64,
    /// Last block processed
    pub last_block_processed: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::divergence::{GasAnalysis, OperationCounts};

    #[test]
    fn test_database_creation() {
        let db = DivergenceDatabase::in_memory().unwrap();
        assert!(db.count_divergences(0, 1000).unwrap() == 0);
    }

    #[test]
    fn test_record_divergence() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let divergence = Divergence {
            block_number: 100,
            tx_index: 5,
            tx_hash: B256::ZERO,
            timestamp: 1234567890,
            divergence_types: vec![DivergenceType::StateRoot],
            gas_analysis: GasAnalysis {
                normal_gas_used: 21000,
                experimental_gas_used: 2688000,
                gas_efficiency_ratio: 1.0,
            },
            normal_ops: OperationCounts::default(),
            experimental_ops: OperationCounts::default(),
            divergence_location: None,
            oog_info: None,
            call_trees: None,
            event_logs: None,
        };

        let id = db.record_divergence(&divergence).unwrap();
        assert!(id > 0);

        assert_eq!(db.count_divergences(0, 1000).unwrap(), 1);
        assert_eq!(db.count_by_type(DivergenceType::StateRoot).unwrap(), 1);
    }

    #[test]
    fn test_record_schedule_divergence() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let divergence = ScheduleDivergence {
            schedule_name: "eip-2780".to_string(),
            block_number: 100,
            tx_index: 0,
            tx_hash: B256::ZERO,
            timestamp: 1234567890,
            divergence_type: DivergenceType::Status,
            baseline_success: true,
            baseline_gas_used: 21000,
            baseline_intrinsic_gas: 21000,
            schedule_success: false,
            schedule_gas_used: 30000,
            schedule_intrinsic_gas: Some(30000),
            gas_delta: -9000,
            gas_efficiency_ratio: Some(1.43),
            tx_category: Some("transfer_new_account".to_string()),
            affected_opcodes: None,
            affected_precompiles: None,
            oog_info: None,
            divergence_location: None,
            operation_counts: None,
        };

        let id = db.record_schedule_divergence(&divergence).unwrap();
        assert!(id > 0);

        assert_eq!(db.count_by_schedule("eip-2780").unwrap(), 1);
        assert_eq!(db.count_by_schedule("other").unwrap(), 0);
    }

    #[test]
    fn test_schedule_divergence_counts() {
        let db = DivergenceDatabase::in_memory().unwrap();

        // Add divergences for multiple schedules
        for (schedule, count) in [("eip-2780", 5), ("7904-v1", 3), ("7904-v2", 7)] {
            for i in 0..count {
                let divergence = ScheduleDivergence {
                    schedule_name: schedule.to_string(),
                    block_number: 100,
                    tx_index: i,
                    tx_hash: B256::repeat_byte(i as u8),
                    timestamp: 1234567890,
                    divergence_type: DivergenceType::Status,
                    baseline_success: true,
                    baseline_gas_used: 21000,
                    baseline_intrinsic_gas: 21000,
                    schedule_success: false,
                    schedule_gas_used: 30000,
                    schedule_intrinsic_gas: None,
                    gas_delta: -9000,
                    gas_efficiency_ratio: None,
                    tx_category: None,
                    affected_opcodes: None,
                    affected_precompiles: None,
                    oog_info: None,
                    divergence_location: None,
                    operation_counts: None,
                };
                db.record_schedule_divergence(&divergence).unwrap();
            }
        }

        let counts = db.divergence_counts_by_schedule().unwrap();
        assert_eq!(counts.len(), 3);
        // Should be ordered by count DESC
        assert_eq!(counts[0], ("7904-v2".to_string(), 7));
        assert_eq!(counts[1], ("eip-2780".to_string(), 5));
        assert_eq!(counts[2], ("7904-v1".to_string(), 3));
    }

    #[test]
    fn test_schedule_stats() {
        let db = DivergenceDatabase::in_memory().unwrap();

        // Update stats
        db.update_schedule_stats("eip-2780", 1000, 50, 10, 500000, 50000, 100).unwrap();

        let stats = db.get_schedule_stats("eip-2780").unwrap().unwrap();
        assert_eq!(stats.total_transactions, 1000);
        assert_eq!(stats.total_divergences, 50);
        assert_eq!(stats.status_divergences, 10);
        assert_eq!(stats.total_gas_saved, 500000);
        assert_eq!(stats.total_gas_increase, 50000);
        assert_eq!(stats.last_block_processed, Some(100));

        // Update again (should accumulate)
        db.update_schedule_stats("eip-2780", 500, 25, 5, 100000, 10000, 200).unwrap();

        let stats = db.get_schedule_stats("eip-2780").unwrap().unwrap();
        assert_eq!(stats.total_transactions, 1500);
        assert_eq!(stats.total_divergences, 75);
        assert_eq!(stats.last_block_processed, Some(200));
    }

    #[test]
    fn test_category_counts() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let categories = ["transfer_to_eoa", "call_to_contract", "transfer_to_eoa", "nop_to_self"];
        for (i, cat) in categories.iter().enumerate() {
            let divergence = ScheduleDivergence {
                schedule_name: "eip-2780".to_string(),
                block_number: 100,
                tx_index: i as u64,
                tx_hash: B256::repeat_byte(i as u8),
                timestamp: 1234567890,
                divergence_type: DivergenceType::GasPattern,
                baseline_success: true,
                baseline_gas_used: 21000,
                baseline_intrinsic_gas: 21000,
                schedule_success: true,
                schedule_gas_used: 6000,
                schedule_intrinsic_gas: Some(6000),
                gas_delta: 15000,
                gas_efficiency_ratio: None,
                tx_category: Some(cat.to_string()),
                affected_opcodes: None,
                affected_precompiles: None,
                oog_info: None,
                divergence_location: None,
                operation_counts: None,
            };
            db.record_schedule_divergence(&divergence).unwrap();
        }

        let counts = db.divergence_counts_by_category("eip-2780").unwrap();
        assert_eq!(counts.len(), 3);
        // transfer_to_eoa appears twice
        assert!(counts.iter().any(|(cat, count, _)| cat == "transfer_to_eoa" && *count == 2));
    }

    #[test]
    fn test_gas_delta_sum() {
        let db = DivergenceDatabase::in_memory().unwrap();

        // Add divergences with different gas deltas
        let deltas = [15000i64, -9000, 13900, 12900];
        for (i, delta) in deltas.iter().enumerate() {
            let divergence = ScheduleDivergence {
                schedule_name: "test".to_string(),
                block_number: 100,
                tx_index: i as u64,
                tx_hash: B256::repeat_byte(i as u8),
                timestamp: 1234567890,
                divergence_type: DivergenceType::GasPattern,
                baseline_success: true,
                baseline_gas_used: 21000,
                baseline_intrinsic_gas: 21000,
                schedule_success: true,
                schedule_gas_used: 0,
                schedule_intrinsic_gas: None,
                gas_delta: *delta,
                gas_efficiency_ratio: None,
                tx_category: None,
                affected_opcodes: None,
                affected_precompiles: None,
                oog_info: None,
                divergence_location: None,
                operation_counts: None,
            };
            db.record_schedule_divergence(&divergence).unwrap();
        }

        let total = db.total_gas_delta_for_schedule("test").unwrap();
        // 15000 - 9000 + 13900 + 12900 = 32800
        assert_eq!(total, 32800);
    }

    #[test]
    fn test_delete_schedule_divergences_in_block_range() {
        let db = DivergenceDatabase::in_memory().unwrap();

        for block in [100_u64, 101, 102, 200] {
            let divergence = ScheduleDivergence {
                schedule_name: "test".to_string(),
                block_number: block,
                tx_index: 0,
                tx_hash: B256::repeat_byte(block as u8),
                timestamp: 1234567890,
                divergence_type: DivergenceType::GasPattern,
                baseline_success: true,
                baseline_gas_used: 21000,
                baseline_intrinsic_gas: 21000,
                schedule_success: true,
                schedule_gas_used: 21000,
                schedule_intrinsic_gas: None,
                gas_delta: 0,
                gas_efficiency_ratio: None,
                tx_category: None,
                affected_opcodes: None,
                affected_precompiles: None,
                oog_info: None,
                divergence_location: None,
                operation_counts: None,
            };
            db.record_schedule_divergence(&divergence).unwrap();
        }

        let deleted = db.delete_schedule_divergences_in_block_range(100, 102).unwrap();
        assert_eq!(deleted, 3);
        assert_eq!(db.count_by_schedule("test").unwrap(), 1);
    }

    #[test]
    fn test_record_schedule_divergence_is_idempotent_for_same_tx() {
        let db = DivergenceDatabase::in_memory().unwrap();

        let key = (123_u64, 7_u64, B256::repeat_byte(0xAA));
        let mut divergence = ScheduleDivergence {
            schedule_name: "idempotent".to_string(),
            block_number: key.0,
            tx_index: key.1,
            tx_hash: key.2,
            timestamp: 1,
            divergence_type: DivergenceType::GasPattern,
            baseline_success: true,
            baseline_gas_used: 21000,
            baseline_intrinsic_gas: 21000,
            schedule_success: true,
            schedule_gas_used: 22000,
            schedule_intrinsic_gas: None,
            gas_delta: 1000,
            gas_efficiency_ratio: Some(1.047),
            tx_category: None,
            affected_opcodes: Some("[4]".to_string()),
            affected_precompiles: None,
            oog_info: None,
            divergence_location: None,
            operation_counts: None,
        };

        db.record_schedule_divergence(&divergence).unwrap();
        divergence.gas_delta = 2000;
        divergence.timestamp = 2;
        db.record_schedule_divergence(&divergence).unwrap();

        assert_eq!(db.count_by_schedule("idempotent").unwrap(), 1);
        assert_eq!(db.total_gas_delta_for_schedule("idempotent").unwrap(), 2000);
    }
}
