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
        // The multiplier approach charges (multiplier - 1) * base_cost as additional gas.
        // However, we don't know the base cost here - we just know the multiplier.
        //
        // In the original implementation, the inspector would:
        // 1. Record gas before step
        // 2. Record gas after step
        // 3. Calculate actual_cost = before - after
        // 4. Charge additional = actual_cost * (multiplier - 1)
        //
        // Since we can't know the actual cost here, this schedule works differently
        // from the per-opcode approach. It's meant to be used with an inspector
        // that applies the multiplier during execution.
        //
        // For now, we return 0 and the inspector will handle the multiplication.
        // This schedule is mainly for backwards compatibility and configuration.
        0
    }

    fn precompile_gas_delta(&self, _address: &Address, _input: &[u8]) -> i64 {
        // Same as opcode_gas_delta - handled by inspector
        0
    }

    fn modifies_execution(&self) -> bool {
        // Even though opcode_gas_delta returns 0, this schedule does modify
        // execution through the inspector multiplier mechanism.
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
