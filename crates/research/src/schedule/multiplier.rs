//! Uniform gas multiplier schedule.
//!
//! This schedule multiplies all gas costs by a fixed factor.
//! Useful for stress-testing contracts under high gas conditions.

use super::{
    context::OpcodeContext,
    traits::{GasSchedule, ScheduleKind},
};
use alloy_primitives::Address;

/// A schedule that multiplies all gas costs by a fixed factor.
///
/// This provides backwards compatibility with the original research mode
/// that used a uniform multiplier approach.
#[derive(Debug, Clone)]
pub struct MultiplierSchedule {
    /// Schedule name
    name: String,
    /// Gas multiplier (e.g., 128 means 128x normal costs)
    multiplier: u64,
}

impl MultiplierSchedule {
    /// Create a new multiplier schedule.
    ///
    /// # Arguments
    /// * `name` - Unique name for this schedule
    /// * `multiplier` - Factor to multiply gas costs by
    pub fn new(name: String, multiplier: u64) -> Self {
        Self { name, multiplier }
    }

    /// Create a schedule with a default name based on the multiplier.
    pub fn with_multiplier(multiplier: u64) -> Self {
        Self { name: format!("{}x", multiplier), multiplier }
    }

    /// Get the multiplier value.
    pub fn multiplier(&self) -> u64 {
        self.multiplier
    }
}

impl GasSchedule for MultiplierSchedule {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Uniform gas cost multiplier"
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::ExecutionOnly
    }

    fn opcode_gas_delta(&self, _opcode: u8, _ctx: &OpcodeContext) -> i64 {
        // Multiplier semantics are implemented through
        // `GasSchedule::execution_gas_multiplier()`, not per-opcode additive deltas.
        0
    }

    fn precompile_gas_delta(&self, _address: &Address, _input: &[u8]) -> i64 {
        0
    }

    fn execution_gas_multiplier(&self) -> Option<u64> {
        Some(self.multiplier)
    }

    fn modifies_execution(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multiplier_schedule_new() {
        let schedule = MultiplierSchedule::new("128x".to_string(), 128);
        assert_eq!(schedule.name(), "128x");
        assert_eq!(schedule.multiplier(), 128);
    }

    #[test]
    fn test_multiplier_schedule_with_multiplier() {
        let schedule = MultiplierSchedule::with_multiplier(256);
        assert_eq!(schedule.name(), "256x");
        assert_eq!(schedule.multiplier(), 256);
    }

    #[test]
    fn test_multiplier_schedule_kind() {
        let schedule = MultiplierSchedule::with_multiplier(128);
        assert_eq!(schedule.kind(), ScheduleKind::ExecutionOnly);
        assert!(schedule.modifies_execution());
        assert!(!schedule.modifies_intrinsic());
    }
}
