//! Database for storing divergence data.

use crate::divergence::{CallFrame, Divergence, DivergenceType, EventLog};
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::divergence::{GasAnalysis, OperationCounts};
    use alloy_primitives::B256;

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
}
