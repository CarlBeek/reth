//! Core traits for gas schedule definitions.

use super::context::{OpcodeContext, TxContext};
use alloy_primitives::Address;
use std::fmt::Debug;

/// The kind of modifications a schedule makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleKind {
    /// Schedule only modifies intrinsic gas (e.g., EIP-2780)
    IntrinsicOnly,
    /// Schedule only modifies execution gas (e.g., CSV-based opcode repricing)
    ExecutionOnly,
    /// Schedule modifies both intrinsic and execution gas
    Both,
    /// Schedule makes no modifications (baseline)
    None,
}

/// Defines how gas costs are modified for a specific experiment.
///
/// Implementations of this trait represent different gas schedule experiments
/// that can be tested against baseline Ethereum execution.
///
/// # Design
///
/// The trait is designed to be:
/// - **Composable**: Multiple schedules can be tracked simultaneously
/// - **Efficient**: Methods return deltas/overrides, not full recalculations
/// - **Extensible**: New schedule types can be added without core changes
///
/// # Intrinsic vs Execution Gas
///
/// - **Intrinsic gas**: The upfront cost charged before execution (21000 for regular tx)
/// - **Execution gas**: Gas charged during EVM execution (per opcode/precompile)
///
/// A schedule can modify one or both of these.
pub trait GasSchedule: Send + Sync + Debug {
    /// Unique identifier for this schedule.
    ///
    /// Used in database records and CLI output.
    /// Should be URL-safe and lowercase (e.g., "eip-2780", "7904-v1").
    fn name(&self) -> &str;

    /// Human-readable description of what this schedule tests.
    fn description(&self) -> &str;

    /// What kind of modifications this schedule makes.
    fn kind(&self) -> ScheduleKind;

    /// Calculate intrinsic gas for a transaction.
    ///
    /// Returns `Some(gas)` if this schedule overrides intrinsic gas,
    /// or `None` to use the default calculation.
    ///
    /// # Arguments
    /// * `ctx` - Transaction context including sender, recipient, value, etc.
    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        let _ = ctx;
        None
    }

    /// Get additional gas to charge for an opcode.
    ///
    /// Returns the additional gas delta (can be positive or negative).
    /// Returns 0 if this opcode is unaffected by this schedule.
    ///
    /// # Arguments
    /// * `opcode` - The opcode byte (0x00-0xFF)
    /// * `ctx` - Opcode execution context
    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        let _ = (opcode, ctx);
        0
    }

    /// Get additional gas to charge for a precompile call.
    ///
    /// Returns the additional gas delta (can be positive or negative).
    /// Returns 0 if this precompile is unaffected by this schedule.
    ///
    /// # Arguments
    /// * `address` - The precompile address
    /// * `input` - The call input data
    fn precompile_gas_delta(&self, address: &Address, input: &[u8]) -> i64 {
        let _ = (address, input);
        0
    }

    /// Whether this schedule modifies intrinsic gas.
    fn modifies_intrinsic(&self) -> bool {
        matches!(self.kind(), ScheduleKind::IntrinsicOnly | ScheduleKind::Both)
    }

    /// Whether this schedule modifies execution gas (opcodes/precompiles).
    fn modifies_execution(&self) -> bool {
        matches!(self.kind(), ScheduleKind::ExecutionOnly | ScheduleKind::Both)
    }

    /// Get the transaction category name for this schedule, if applicable.
    ///
    /// Used by intrinsic-modifying schedules (like EIP-2780) to categorize
    /// transactions for analysis.
    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        let _ = ctx;
        None
    }

    /// List of opcodes affected by this schedule.
    ///
    /// Used for reporting which opcodes have modified costs.
    fn affected_opcodes(&self) -> Vec<u8> {
        Vec::new()
    }

    /// List of precompile addresses affected by this schedule.
    ///
    /// Used for reporting which precompiles have modified costs.
    fn affected_precompiles(&self) -> Vec<Address> {
        Vec::new()
    }
}

/// Allow `GasSchedule` to be used as a trait object.
impl GasSchedule for Box<dyn GasSchedule> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn description(&self) -> &str {
        (**self).description()
    }

    fn kind(&self) -> ScheduleKind {
        (**self).kind()
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        (**self).intrinsic_gas(ctx)
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        (**self).opcode_gas_delta(opcode, ctx)
    }

    fn precompile_gas_delta(&self, address: &Address, input: &[u8]) -> i64 {
        (**self).precompile_gas_delta(address, input)
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        (**self).tx_category(ctx)
    }

    fn affected_opcodes(&self) -> Vec<u8> {
        (**self).affected_opcodes()
    }

    fn affected_precompiles(&self) -> Vec<Address> {
        (**self).affected_precompiles()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestSchedule;

    impl GasSchedule for TestSchedule {
        fn name(&self) -> &str {
            "test"
        }

        fn description(&self) -> &str {
            "Test schedule"
        }

        fn kind(&self) -> ScheduleKind {
            ScheduleKind::ExecutionOnly
        }

        fn opcode_gas_delta(&self, opcode: u8, _ctx: &OpcodeContext) -> i64 {
            if opcode == 0x04 {
                // DIV
                10 // Add 10 gas
            } else {
                0
            }
        }
    }

    #[test]
    fn test_schedule_kind() {
        let schedule = TestSchedule;
        assert!(schedule.modifies_execution());
        assert!(!schedule.modifies_intrinsic());
    }

    #[test]
    fn test_opcode_delta() {
        let schedule = TestSchedule;
        let ctx = OpcodeContext::default();

        assert_eq!(schedule.opcode_gas_delta(0x04, &ctx), 10);
        assert_eq!(schedule.opcode_gas_delta(0x01, &ctx), 0);
    }

    #[test]
    fn test_boxed_schedule() {
        let schedule: Box<dyn GasSchedule> = Box::new(TestSchedule);
        assert_eq!(schedule.name(), "test");
        assert!(schedule.modifies_execution());
    }
}
