//! Multi-schedule analyzer for comparing transaction execution.
//!
//! This module provides the high-level analyzer that coordinates
//! schedule comparison and divergence recording.

use crate::{
    comparison::{
        ExecutionSummary, IntrinsicComparison, MultiScheduleComparisonResult,
        ScheduleComparisonResult,
    },
    database::{DivergenceDatabase, ScheduleDivergence},
    divergence::{DivergenceType, OperationCounts},
    schedule::{ScheduleRegistry, TxContext},
};
use alloy_primitives::B256;
use thiserror::Error;
use tracing::{debug, info, warn};

/// Errors that can occur during analysis.
#[derive(Debug, Error)]
pub enum AnalyzerError {
    /// Database error
    #[error("Database error: {0}")]
    Database(#[from] crate::database::DatabaseError),

    /// No schedules configured
    #[error("No schedules configured for analysis")]
    NoSchedules,
}

/// Multi-schedule analyzer for comparing transaction execution.
///
/// This analyzer coordinates the comparison of transactions across
/// multiple gas schedules and records divergences.
#[derive(Debug)]
pub struct MultiScheduleAnalyzer {
    /// Schedule registry
    registry: ScheduleRegistry,

    /// Divergence database (optional)
    database: Option<DivergenceDatabase>,

    /// Statistics
    stats: AnalyzerStats,
}

/// Statistics from analysis.
#[derive(Debug, Clone, Default)]
pub struct AnalyzerStats {
    /// Total transactions analyzed
    pub transactions_analyzed: u64,

    /// Total divergences found
    pub divergences_found: u64,

    /// Divergences per schedule
    pub divergences_by_schedule: std::collections::HashMap<String, u64>,

    /// OOG occurrences per schedule
    pub oog_by_schedule: std::collections::HashMap<String, u64>,
}

impl MultiScheduleAnalyzer {
    /// Create a new multi-schedule analyzer.
    pub fn new(registry: ScheduleRegistry) -> Result<Self, AnalyzerError> {
        if registry.is_empty() {
            return Err(AnalyzerError::NoSchedules);
        }

        info!(
            target: "reth::research",
            schedules = %registry.summary(),
            "Created multi-schedule analyzer"
        );

        Ok(Self { registry, database: None, stats: AnalyzerStats::default() })
    }

    /// Attach a database for recording divergences.
    pub fn with_database(mut self, database: DivergenceDatabase) -> Self {
        self.database = Some(database);
        self
    }

    /// Get the schedule registry.
    pub fn registry(&self) -> &ScheduleRegistry {
        &self.registry
    }

    /// Get analysis statistics.
    pub fn stats(&self) -> &AnalyzerStats {
        &self.stats
    }

    /// Analyze intrinsic gas for a transaction across all configured schedules.
    ///
    /// This handles **intrinsic gas comparison only**. Execution gas comparison
    /// (opcode-level deltas, subcall propagation, OOG detection) is performed by
    /// the ExEx in `main.rs` via per-schedule re-execution with [`ScheduleInspector`].
    /// The split exists because execution comparison requires actual EVM re-execution
    /// (one pass per schedule), which the analyzer cannot perform without a database
    /// and EVM environment.
    ///
    /// [`ScheduleInspector`]: crate::multi_schedule_inspector::ScheduleInspector
    pub fn analyze_transaction(
        &mut self,
        block_number: u64,
        tx_index: u64,
        tx_hash: B256,
        timestamp: u64,
        baseline: ExecutionSummary,
        tx_context: Option<&TxContext>,
    ) -> Result<MultiScheduleComparisonResult, AnalyzerError> {
        let mut result = MultiScheduleComparisonResult::new(
            block_number,
            tx_index,
            tx_hash,
            timestamp,
            baseline.clone(),
        );

        // Analyze intrinsic gas for each schedule
        for schedule in self.registry.all() {
            let schedule_name = schedule.name().to_string();
            let mut comparison = ScheduleComparisonResult::new(schedule_name.clone());

            if schedule.modifies_intrinsic() {
                if let Some(ctx) = tx_context {
                    if let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx) {
                        let intrinsic_comp =
                            IntrinsicComparison::new(baseline.intrinsic_gas, schedule_intrinsic);

                        let intrinsic_comp = if let Some(category) = schedule.tx_category(ctx) {
                            intrinsic_comp.with_category(category)
                        } else {
                            intrinsic_comp
                        };

                        if intrinsic_comp.delta != 0 {
                            comparison = comparison.with_intrinsic(intrinsic_comp);
                            comparison.has_divergence = true;
                            comparison.divergence_type = Some(DivergenceType::GasPattern);
                        }
                    }
                }
            }

            // Record divergence to database if there was one
            if comparison.has_divergence {
                self.record_divergence(
                    block_number,
                    tx_index,
                    tx_hash,
                    timestamp,
                    &baseline,
                    &comparison,
                )?;

                *self.stats.divergences_by_schedule.entry(schedule_name).or_insert(0) += 1;
                self.stats.divergences_found += 1;
            }

            result.add_schedule_result(comparison);
        }

        self.stats.transactions_analyzed += 1;

        Ok(result)
    }

    /// Record a divergence to the database.
    fn record_divergence(
        &self,
        block_number: u64,
        tx_index: u64,
        tx_hash: B256,
        timestamp: u64,
        baseline: &ExecutionSummary,
        comparison: &ScheduleComparisonResult,
    ) -> Result<(), AnalyzerError> {
        let Some(ref db) = self.database else {
            return Ok(());
        };

        let gas_delta = comparison.gas_delta();
        let schedule_gas_used = if gas_delta >= 0 {
            baseline.gas_used.saturating_add(gas_delta as u64)
        } else {
            baseline.gas_used.saturating_sub((-gas_delta) as u64)
        };

        let gas_efficiency_ratio = if baseline.gas_used > 0 {
            Some(schedule_gas_used as f64 / baseline.gas_used as f64)
        } else {
            None
        };

        let divergence = ScheduleDivergence {
            schedule_name: comparison.schedule_name.clone(),
            block_number,
            tx_index,
            tx_hash,
            timestamp,
            divergence_type: comparison.divergence_type.unwrap_or(DivergenceType::GasPattern),
            schedule_kind: "unknown".to_string(),
            schedule_description: comparison.schedule_name.clone(),
            schedule_config_hash: String::new(),
            block_hash: B256::ZERO,
            parent_hash: B256::ZERO,
            baseline_success: baseline.success,
            baseline_gas_used: baseline.gas_used,
            baseline_intrinsic_gas: baseline.intrinsic_gas,
            schedule_success: comparison
                .execution_comparison
                .as_ref()
                .map(|e| e.schedule_success)
                .unwrap_or(baseline.success),
            schedule_gas_used,
            schedule_intrinsic_gas: comparison
                .intrinsic_comparison
                .as_ref()
                .map(|i| i.schedule_intrinsic),
            gas_delta,
            gas_efficiency_ratio,
            tx_category: comparison
                .intrinsic_comparison
                .as_ref()
                .and_then(|i| i.tx_category.clone()),
            affected_opcodes: comparison
                .execution_comparison
                .as_ref()
                .and_then(|e| serde_json::to_string(&e.affected_opcodes).ok()),
            affected_precompiles: comparison
                .execution_comparison
                .as_ref()
                .and_then(|e| serde_json::to_string(&e.affected_precompiles).ok()),
            oog_info: comparison.oog_info.as_ref().and_then(|o| serde_json::to_string(o).ok()),
            divergence_location: comparison
                .divergence_location
                .as_ref()
                .and_then(|l| serde_json::to_string(l).ok()),
            operation_counts: None,
            baseline_call_frames: None,
            schedule_call_frames: None,
            baseline_event_logs: None,
            schedule_event_logs: None,
            baseline_call_frames_hash: None,
            schedule_call_frames_hash: None,
            baseline_event_logs_hash: None,
            schedule_event_logs_hash: None,
            status_changed: comparison
                .execution_comparison
                .as_ref()
                .map(|e| e.schedule_success != baseline.success)
                .unwrap_or(false),
            gas_changed: gas_delta != 0,
            call_tree_changed: false,
            event_logs_changed: false,
            output_changed: false,
            created_address_changed: false,
            logs_bloom_changed: false,
            sender: String::new(),
            recipient: None,
            value_wei: "0".to_string(),
            input_len: 0,
            input_zero_bytes: 0,
            input_nonzero_bytes: 0,
            tx_gas_limit: 0,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
            is_create: false,
            baseline_output_len: None,
            schedule_output_len: None,
            baseline_output_hash: None,
            schedule_output_hash: None,
            baseline_created_address: None,
            schedule_created_address: None,
            baseline_log_count: 0,
            schedule_log_count: 0,
            baseline_logs_bloom: String::new(),
            schedule_logs_bloom: String::new(),
            would_fit_in_original_limit: comparison
                .execution_comparison
                .as_ref()
                .map(|e| e.schedule_success)
                .unwrap_or(baseline.success),
            min_multiplier_to_succeed: None,
        };

        match db.record_schedule_divergence(&divergence) {
            Ok(id) => {
                debug!(
                    target: "reth::research",
                    id = id,
                    schedule = %comparison.schedule_name,
                    tx_hash = ?tx_hash,
                    "Recorded schedule divergence"
                );
            }
            Err(e) => {
                warn!(
                    target: "reth::research",
                    error = %e,
                    schedule = %comparison.schedule_name,
                    "Failed to record divergence"
                );
                return Err(e.into());
            }
        }

        Ok(())
    }

    /// Log a summary of analysis results.
    pub fn log_summary(&self) {
        info!(
            target: "reth::research",
            transactions = self.stats.transactions_analyzed,
            divergences = self.stats.divergences_found,
            "Analysis summary"
        );

        for (schedule, count) in &self.stats.divergences_by_schedule {
            info!(
                target: "reth::research",
                schedule = %schedule,
                divergences = count,
                oog = self.stats.oog_by_schedule.get(schedule).unwrap_or(&0),
                "Schedule divergence count"
            );
        }
    }
}

/// Builder for creating ExecutionSummary from execution results.
#[derive(Debug)]
pub struct ExecutionSummaryBuilder {
    success: bool,
    gas_used: u64,
    intrinsic_gas: u64,
    operation_counts: Option<OperationCounts>,
}

impl ExecutionSummaryBuilder {
    /// Create a new builder.
    pub fn new(success: bool, gas_used: u64, intrinsic_gas: u64) -> Self {
        Self { success, gas_used, intrinsic_gas, operation_counts: None }
    }

    /// Set operation counts from inspector.
    pub fn with_operation_counts(mut self, counts: &OperationCounts) -> Self {
        self.operation_counts = Some(counts.clone());
        self
    }

    /// Build the execution summary.
    pub fn build(self) -> ExecutionSummary {
        let mut summary = ExecutionSummary::new(self.success, self.gas_used, self.intrinsic_gas);
        if let Some(counts) = self.operation_counts {
            summary = summary.with_operation_counts(counts);
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Eip2780Schedule, MultiplierSchedule};

    fn create_test_registry() -> ScheduleRegistry {
        let mut registry = ScheduleRegistry::new();
        registry.register(Eip2780Schedule::new()).unwrap();
        registry.register(MultiplierSchedule::with_multiplier(128)).unwrap();
        registry
    }

    #[test]
    fn test_analyzer_creation() {
        let registry = create_test_registry();
        let analyzer = MultiScheduleAnalyzer::new(registry).unwrap();

        assert_eq!(analyzer.registry().len(), 2);
    }

    #[test]
    fn test_analyzer_empty_registry() {
        let registry = ScheduleRegistry::new();
        let result = MultiScheduleAnalyzer::new(registry);

        assert!(result.is_err());
    }

    #[test]
    fn test_analyzer_stats_default() {
        let stats = AnalyzerStats::default();
        assert_eq!(stats.transactions_analyzed, 0);
        assert_eq!(stats.divergences_found, 0);
    }

    #[test]
    fn test_execution_summary_builder() {
        let summary = ExecutionSummaryBuilder::new(true, 50000, 21000)
            .with_operation_counts(&OperationCounts { sload_count: 5, ..Default::default() })
            .build();

        assert!(summary.success);
        assert_eq!(summary.gas_used, 50000);
        assert_eq!(summary.operation_counts.sload_count, 5);
    }

    #[test]
    fn test_analyze_transaction_basic() {
        let registry = create_test_registry();
        let mut analyzer = MultiScheduleAnalyzer::new(registry).unwrap();

        let baseline = ExecutionSummary::new(true, 50000, 21000);
        let result = analyzer
            .analyze_transaction(1000000, 0, B256::ZERO, 1234567890, baseline, None)
            .unwrap();

        assert_eq!(result.block_number, 1000000);
        assert_eq!(result.schedule_results.len(), 2);
        assert_eq!(analyzer.stats().transactions_analyzed, 1);
    }
}
