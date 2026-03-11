//! Comparison result types for multi-schedule analysis.
//!
//! These types represent the results of comparing transaction execution
//! across multiple gas schedules.

use crate::divergence::{DivergenceLocation, DivergenceType, OperationCounts, OutOfGasInfo};
use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// Result of comparing a transaction across multiple gas schedules.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiScheduleComparisonResult {
    /// Block number
    pub block_number: u64,

    /// Transaction index within the block
    pub tx_index: u64,

    /// Transaction hash
    pub tx_hash: B256,

    /// Block timestamp
    pub timestamp: u64,

    /// Baseline execution summary
    pub baseline: ExecutionSummary,

    /// Per-schedule comparison results
    pub schedule_results: Vec<ScheduleComparisonResult>,
}

impl MultiScheduleComparisonResult {
    /// Create a new comparison result.
    pub fn new(
        block_number: u64,
        tx_index: u64,
        tx_hash: B256,
        timestamp: u64,
        baseline: ExecutionSummary,
    ) -> Self {
        Self { block_number, tx_index, tx_hash, timestamp, baseline, schedule_results: Vec::new() }
    }

    /// Add a schedule comparison result.
    pub fn add_schedule_result(&mut self, result: ScheduleComparisonResult) {
        self.schedule_results.push(result);
    }

    /// Get schedules that diverged.
    pub fn divergent_schedules(&self) -> Vec<&str> {
        self.schedule_results
            .iter()
            .filter(|r| r.has_divergence)
            .map(|r| r.schedule_name.as_str())
            .collect()
    }

    /// Check if any schedule diverged.
    pub fn has_any_divergence(&self) -> bool {
        self.schedule_results.iter().any(|r| r.has_divergence)
    }

    /// Get total gas delta across all schedules.
    pub fn total_gas_delta(&self) -> i64 {
        self.schedule_results
            .iter()
            .filter_map(|r| r.execution_comparison.as_ref())
            .map(|e| e.gas_delta)
            .sum()
    }
}

/// Summary of baseline execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    /// Whether execution succeeded
    pub success: bool,

    /// Gas used by execution
    pub gas_used: u64,

    /// Intrinsic gas (pre-execution cost)
    pub intrinsic_gas: u64,

    /// Operation counts
    pub operation_counts: OperationCounts,

    /// Contract addresses involved
    pub contracts_called: Vec<Address>,
}

impl ExecutionSummary {
    /// Create a new execution summary.
    pub fn new(success: bool, gas_used: u64, intrinsic_gas: u64) -> Self {
        Self {
            success,
            gas_used,
            intrinsic_gas,
            operation_counts: OperationCounts::default(),
            contracts_called: Vec::new(),
        }
    }

    /// Set operation counts.
    pub fn with_operation_counts(mut self, counts: OperationCounts) -> Self {
        self.operation_counts = counts;
        self
    }

    /// Set contracts called.
    pub fn with_contracts(mut self, contracts: Vec<Address>) -> Self {
        self.contracts_called = contracts;
        self
    }
}

/// Comparison result for a single schedule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleComparisonResult {
    /// Schedule name
    pub schedule_name: String,

    /// Intrinsic gas comparison (for intrinsic-modifying schedules)
    pub intrinsic_comparison: Option<IntrinsicComparison>,

    /// Execution comparison (for execution-modifying schedules)
    pub execution_comparison: Option<ExecutionComparison>,

    /// Whether divergence occurred
    pub has_divergence: bool,

    /// Type of divergence (if any)
    pub divergence_type: Option<DivergenceType>,

    /// Location of first divergence
    pub divergence_location: Option<DivergenceLocation>,

    /// OOG information (if applicable)
    pub oog_info: Option<OutOfGasInfo>,
}

impl ScheduleComparisonResult {
    /// Create a new schedule comparison result.
    pub fn new(schedule_name: String) -> Self {
        Self {
            schedule_name,
            intrinsic_comparison: None,
            execution_comparison: None,
            has_divergence: false,
            divergence_type: None,
            divergence_location: None,
            oog_info: None,
        }
    }

    /// Set intrinsic comparison.
    pub fn with_intrinsic(mut self, comparison: IntrinsicComparison) -> Self {
        self.intrinsic_comparison = Some(comparison);
        self
    }

    /// Set execution comparison.
    pub fn with_execution(mut self, comparison: ExecutionComparison) -> Self {
        self.execution_comparison = Some(comparison);
        self
    }

    /// Mark as diverged.
    pub fn with_divergence(mut self, divergence_type: DivergenceType) -> Self {
        self.has_divergence = true;
        self.divergence_type = Some(divergence_type);
        self
    }

    /// Set divergence location.
    pub fn with_location(mut self, location: DivergenceLocation) -> Self {
        self.divergence_location = Some(location);
        self
    }

    /// Set OOG info.
    pub fn with_oog(mut self, oog_info: OutOfGasInfo) -> Self {
        self.oog_info = Some(oog_info);
        self
    }

    /// Get gas delta (from either intrinsic or execution comparison).
    pub fn gas_delta(&self) -> i64 {
        self.execution_comparison
            .as_ref()
            .map(|e| e.gas_delta)
            .or_else(|| self.intrinsic_comparison.as_ref().map(|i| i.delta))
            .unwrap_or(0)
    }
}

/// Comparison of intrinsic gas costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntrinsicComparison {
    /// Baseline intrinsic gas
    pub baseline_intrinsic: u64,

    /// Schedule's intrinsic gas
    pub schedule_intrinsic: u64,

    /// Delta (schedule - baseline), positive means schedule costs more
    pub delta: i64,

    /// Transaction category (for EIP-2780 style schedules)
    pub tx_category: Option<String>,
}

impl IntrinsicComparison {
    /// Create a new intrinsic comparison.
    pub fn new(baseline_intrinsic: u64, schedule_intrinsic: u64) -> Self {
        let delta = schedule_intrinsic as i64 - baseline_intrinsic as i64;
        Self { baseline_intrinsic, schedule_intrinsic, delta, tx_category: None }
    }

    /// Set transaction category.
    pub fn with_category(mut self, category: String) -> Self {
        self.tx_category = Some(category);
        self
    }

    /// Check if this is a gas saving (negative delta).
    pub fn is_saving(&self) -> bool {
        self.delta < 0
    }

    /// Check if this is a gas increase (positive delta).
    pub fn is_increase(&self) -> bool {
        self.delta > 0
    }

    /// Get the percentage change.
    pub fn percentage_change(&self) -> f64 {
        if self.baseline_intrinsic == 0 {
            return 0.0;
        }
        (self.delta as f64 / self.baseline_intrinsic as f64) * 100.0
    }
}

/// Comparison of execution gas costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionComparison {
    /// Additional gas charged under this schedule
    pub additional_gas_charged: i64,

    /// Baseline execution success
    pub baseline_success: bool,

    /// Would execution succeed under this schedule?
    pub schedule_success: bool,

    /// Gas delta (additional gas)
    pub gas_delta: i64,

    /// Gas efficiency ratio (schedule_gas / baseline_gas)
    pub gas_efficiency_ratio: f64,

    /// Affected opcodes (opcodes that had different costs)
    pub affected_opcodes: Vec<u8>,

    /// Affected precompiles (precompiles that had different costs)
    pub affected_precompiles: Vec<Address>,
}

impl ExecutionComparison {
    /// Create a new execution comparison.
    pub fn new(
        additional_gas_charged: i64,
        baseline_success: bool,
        schedule_success: bool,
        baseline_gas_used: u64,
    ) -> Self {
        let schedule_gas = (baseline_gas_used as i64 + additional_gas_charged).max(0) as u64;
        let gas_efficiency_ratio = if baseline_gas_used == 0 {
            1.0
        } else {
            schedule_gas as f64 / baseline_gas_used as f64
        };

        Self {
            additional_gas_charged,
            baseline_success,
            schedule_success,
            gas_delta: additional_gas_charged,
            gas_efficiency_ratio,
            affected_opcodes: Vec::new(),
            affected_precompiles: Vec::new(),
        }
    }

    /// Set affected opcodes.
    pub fn with_affected_opcodes(mut self, opcodes: Vec<u8>) -> Self {
        self.affected_opcodes = opcodes;
        self
    }

    /// Set affected precompiles.
    pub fn with_affected_precompiles(mut self, precompiles: Vec<Address>) -> Self {
        self.affected_precompiles = precompiles;
        self
    }

    /// Check if execution status changed (divergence).
    pub fn status_changed(&self) -> bool {
        self.baseline_success != self.schedule_success
    }

    /// Check if this is a significant gas change (>5%).
    pub fn is_significant_change(&self) -> bool {
        (self.gas_efficiency_ratio - 1.0).abs() > 0.05
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_intrinsic_comparison() {
        // EIP-2780 saves gas for simple transfers
        let comp = IntrinsicComparison::new(21000, 6000);
        assert!(comp.is_saving());
        assert_eq!(comp.delta, -15000);
        assert!((comp.percentage_change() - (-71.43)).abs() < 0.1);
    }

    #[test]
    fn test_intrinsic_comparison_with_category() {
        let comp =
            IntrinsicComparison::new(21000, 6000).with_category("transfer_to_eoa".to_string());
        assert_eq!(comp.tx_category, Some("transfer_to_eoa".to_string()));
    }

    #[test]
    fn test_execution_comparison() {
        let comp = ExecutionComparison::new(1000, true, true, 50000);
        assert_eq!(comp.gas_delta, 1000);
        assert!(!comp.status_changed());
        assert!((comp.gas_efficiency_ratio - 1.02).abs() < 0.01);
    }

    #[test]
    fn test_execution_comparison_status_change() {
        let comp = ExecutionComparison::new(1000000, true, false, 50000);
        assert!(comp.status_changed());
    }

    #[test]
    fn test_schedule_comparison_result() {
        let result = ScheduleComparisonResult::new("eip-2780".to_string())
            .with_intrinsic(IntrinsicComparison::new(21000, 6000))
            .with_divergence(DivergenceType::GasPattern);

        assert!(result.has_divergence);
        assert_eq!(result.gas_delta(), -15000);
    }

    #[test]
    fn test_multi_schedule_comparison() {
        let baseline = ExecutionSummary::new(true, 50000, 21000);
        let mut result =
            MultiScheduleComparisonResult::new(1000000, 0, B256::ZERO, 1234567890, baseline);

        result.add_schedule_result(
            ScheduleComparisonResult::new("eip-2780".to_string())
                .with_intrinsic(IntrinsicComparison::new(21000, 6000))
                .with_divergence(DivergenceType::GasPattern),
        );

        result.add_schedule_result(ScheduleComparisonResult::new("7904-v1".to_string()));

        assert_eq!(result.divergent_schedules(), vec!["eip-2780"]);
        assert!(result.has_any_divergence());
    }

    #[test]
    fn test_execution_summary() {
        let summary = ExecutionSummary::new(true, 50000, 21000)
            .with_operation_counts(OperationCounts { sload_count: 10, ..Default::default() })
            .with_contracts(vec![Address::ZERO]);

        assert!(summary.success);
        assert_eq!(summary.operation_counts.sload_count, 10);
        assert_eq!(summary.contracts_called.len(), 1);
    }
}
