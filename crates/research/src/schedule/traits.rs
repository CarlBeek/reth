//! Core traits for gas schedule definitions.

use super::context::{OpcodeContext, TxContext};
use alloy_primitives::Address;
use reth_evm::EvmEnv;
use revm::{context_interface::cfg::gas::InitialAndFloorGas, primitives::hardfork::SpecId};
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

    /// Stable, configuration-specific fingerprint material for this schedule.
    ///
    /// This should include any parameters that materially change execution so
    /// persisted datasets can distinguish different schedule configurations.
    fn config_fingerprint(&self) -> String {
        let mut affected_opcodes = self.affected_opcodes();
        affected_opcodes.sort_unstable();

        let mut affected_precompiles: Vec<String> = self
            .affected_precompiles()
            .into_iter()
            .map(|address| format!("{address:#x}"))
            .collect();
        affected_precompiles.sort();

        format!(
            "name={}|description={}|kind={:?}|opcodes={affected_opcodes:?}|precompiles={affected_precompiles:?}",
            self.name(),
            self.description(),
            self.kind(),
        )
    }

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

    /// Calculate the full initial/floor gas split for a transaction.
    ///
    /// Schedules that override intrinsic gas but also need to preserve the
    /// regular-gas / state-gas split can return the full calculation here.
    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        self.intrinsic_gas(ctx).map(|intrinsic| InitialAndFloorGas::new(intrinsic, 0))
    }

    /// Get the additional gas to charge for an opcode beyond the EVM's own cost.
    ///
    /// Returns the gas delta (`new_cost - current_cost`) to apply on top of the
    /// EVM's built-in charge. Positive values increase cost, negative values
    /// decrease cost. Returns 0 if this opcode is unaffected by this schedule.
    ///
    /// # Important: deltas are additive, not replacements
    ///
    /// The EVM charges its own base cost for every opcode. This method returns
    /// the *additional* adjustment. For example, if SLOAD currently costs 2100
    /// and the schedule prices it at 2600, return +500 — not 2600.
    ///
    /// For CALL/CREATE opcodes specifically, the inspector applies this delta
    /// *before* the EVM executes the opcode, so it affects the 63/64 gas
    /// forwarding rule for subcalls. Schedule authors should still return only
    /// the delta (not the total cost), since the EVM will charge its own base
    /// cost separately.
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

    /// Multiply the EVM's observed execution gas for each opcode/precompile.
    ///
    /// This is intended for schedules that scale all execution gas uniformly
    /// after the EVM has computed the opcode/precompile's native cost.
    ///
    /// Returns `Some(multiplier)` for schedules such as `128x`, or `None` for
    /// schedules that only use explicit additive repricing via
    /// [`GasSchedule::opcode_gas_delta`] / [`GasSchedule::precompile_gas_delta`].
    fn execution_gas_multiplier(&self) -> Option<u64> {
        None
    }

    /// Configure the EVM environment used for this schedule's replay pass.
    ///
    /// Most schedules are implemented as explicit inspector deltas and leave the
    /// EVM environment unchanged. Native fork-style schedules can use this hook
    /// to enable protocol behavior that cannot be expressed as isolated opcode
    /// deltas, while still comparing the result against the baseline execution.
    ///
    /// Returns `true` if the schedule changed the environment.
    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        let _ = env;
        false
    }

    /// Whether the schedule's intrinsic gas is enforced by its configured EVM
    /// environment.
    ///
    /// When this is `false`, the research runner offsets the replay
    /// transaction gas limit to compensate for intrinsic-gas changes because the
    /// EVM still deducts baseline intrinsic gas internally.
    fn uses_native_intrinsic_gas(&self) -> bool {
        false
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

    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        (**self).initial_and_floor_gas(ctx)
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        (**self).opcode_gas_delta(opcode, ctx)
    }

    fn precompile_gas_delta(&self, address: &Address, input: &[u8]) -> i64 {
        (**self).precompile_gas_delta(address, input)
    }

    fn execution_gas_multiplier(&self) -> Option<u64> {
        (**self).execution_gas_multiplier()
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        (**self).configure_evm_env(env)
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        (**self).uses_native_intrinsic_gas()
    }

    fn config_fingerprint(&self) -> String {
        (**self).config_fingerprint()
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

    #[test]
    fn test_default_config_fingerprint_stable() {
        let schedule = TestSchedule;
        assert_eq!(
            schedule.config_fingerprint(),
            "name=test|description=Test schedule|kind=ExecutionOnly|opcodes=[]|precompiles=[]"
        );
    }
}
