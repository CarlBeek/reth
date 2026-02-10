//! Multi-schedule analyzer for comparing transaction execution.
//!
//! This module provides the high-level analyzer that coordinates
//! schedule comparison and divergence recording.

use crate::{
    comparison::{
        ExecutionComparison, ExecutionSummary, IntrinsicComparison, MultiScheduleComparisonResult,
        ScheduleComparisonResult,
    },
    database::{DivergenceDatabase, ScheduleDivergence},
    divergence::{DivergenceType, OperationCounts},
    multi_schedule_inspector::{MultiScheduleInspector, ScheduleResult},
    schedule::{GasSchedule, ScheduleRegistry, TxContext},
};
use alloy_primitives::B256;
use std::sync::Arc;
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

    /// Create an inspector for a transaction.
    ///
    /// The inspector tracks gas deltas for all execution-modifying schedules.
    pub fn create_inspector(&self) -> MultiScheduleInspector {
        let schedules: Vec<Arc<dyn GasSchedule>> = self
            .registry
            .all()
            .into_iter()
            .filter(|s| s.modifies_execution())
            .collect();

        MultiScheduleInspector::new(schedules)
    }

    /// Analyze a transaction after execution.
    ///
    /// This compares the baseline execution against all configured schedules.
    ///
    /// # Arguments
    /// * `block_number` - The block containing this transaction
    /// * `tx_index` - Transaction index within the block
    /// * `tx_hash` - Transaction hash
    /// * `timestamp` - Block timestamp
    /// * `baseline` - Baseline execution summary
    /// * `tx_context` - Transaction context (for intrinsic gas calculation)
    /// * `inspector_results` - Results from the multi-schedule inspector
    pub fn analyze_transaction(
        &mut self,
        block_number: u64,
        tx_index: u64,
        tx_hash: B256,
        timestamp: u64,
        baseline: ExecutionSummary,
        tx_context: Option<&TxContext>,
        inspector_results: &[ScheduleResult],
    ) -> Result<MultiScheduleComparisonResult, AnalyzerError> {
        let mut result =
            MultiScheduleComparisonResult::new(block_number, tx_index, tx_hash, timestamp, baseline.clone());

        // Analyze each schedule
        for schedule in self.registry.all() {
            let schedule_name = schedule.name().to_string();
            let mut comparison = ScheduleComparisonResult::new(schedule_name.clone());

            // Check intrinsic gas for intrinsic-modifying schedules
            if schedule.modifies_intrinsic() {
                if let Some(ctx) = tx_context {
                    if let Some(schedule_intrinsic) = schedule.intrinsic_gas(ctx) {
                        let intrinsic_comp = IntrinsicComparison::new(
                            baseline.intrinsic_gas,
                            schedule_intrinsic,
                        );

                        // Add category if available
                        let intrinsic_comp = if let Some(category) = schedule.tx_category(ctx) {
                            intrinsic_comp.with_category(category)
                        } else {
                            intrinsic_comp
                        };

                        // Check if intrinsic gas change would cause failure
                        // (tx would fail if schedule_intrinsic > gas_limit)
                        if intrinsic_comp.delta != 0 {
                            comparison = comparison.with_intrinsic(intrinsic_comp);
                            comparison.has_divergence = true;
                            comparison.divergence_type = Some(DivergenceType::GasPattern);
                        }
                    }
                }
            }

            // Check execution gas for execution-modifying schedules
            if schedule.modifies_execution() {
                if let Some(insp_result) = inspector_results.iter().find(|r| r.schedule_name == schedule_name) {
                    let schedule_success = !insp_result.would_oog && baseline.success;

                    let exec_comp = ExecutionComparison::new(
                        insp_result.additional_gas,
                        baseline.success,
                        schedule_success,
                        baseline.gas_used,
                    )
                    .with_affected_opcodes(schedule.affected_opcodes())
                    .with_affected_precompiles(schedule.affected_precompiles());

                    // Check for divergence
                    if insp_result.would_oog || insp_result.additional_gas != 0 {
                        comparison = comparison.with_execution(exec_comp);
                        comparison.has_divergence = true;

                        if insp_result.would_oog {
                            comparison.divergence_type = Some(DivergenceType::Status);

                            // Record OOG stats
                            *self.stats.oog_by_schedule.entry(schedule_name.clone()).or_insert(0) += 1;
                        } else {
                            comparison.divergence_type = Some(DivergenceType::GasPattern);
                        }

                        // Add OOG info and divergence location
                        if let Some(ref oog_info) = insp_result.oog_info {
                            comparison = comparison.with_oog(oog_info.clone());
                        }
                        if let Some(ref location) = insp_result.divergence_location {
                            comparison = comparison.with_location(location.clone());
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

                // Update stats
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
            oog_info: comparison
                .oog_info
                .as_ref()
                .and_then(|o| serde_json::to_string(o).ok()),
            divergence_location: comparison
                .divergence_location
                .as_ref()
                .and_then(|l| serde_json::to_string(l).ok()),
            operation_counts: None,
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
    fn test_create_inspector() {
        let registry = create_test_registry();
        let analyzer = MultiScheduleAnalyzer::new(registry).unwrap();

        let inspector = analyzer.create_inspector();
        // Multiplier schedule modifies execution, but returns 0 delta
        // so it may or may not be included depending on implementation
        assert!(inspector.results().len() >= 1);
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
            .analyze_transaction(
                1000000,
                0,
                B256::ZERO,
                1234567890,
                baseline,
                None,
                &[],
            )
            .unwrap();

        assert_eq!(result.block_number, 1000000);
        assert_eq!(result.schedule_results.len(), 2);
        assert_eq!(analyzer.stats().transactions_analyzed, 1);
    }
}
