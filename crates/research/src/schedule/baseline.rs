//! Baseline gas schedule (no modifications).

use super::traits::{GasSchedule, ScheduleKind};

/// Baseline gas schedule that makes no modifications.
///
/// Used as a reference point for comparison. All gas costs remain
/// at their standard Ethereum values.
#[derive(Debug, Clone, Default)]
pub struct BaselineSchedule;

impl BaselineSchedule {
    /// Create a new baseline schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for BaselineSchedule {
    fn name(&self) -> &str {
        "baseline"
    }

    fn description(&self) -> &str {
        "Standard Ethereum gas schedule (no modifications)"
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{OpcodeContext, TxContext};
    use alloy_primitives::Address;

    #[test]
    fn test_baseline_name() {
        let schedule = BaselineSchedule::new();
        assert_eq!(schedule.name(), "baseline");
    }

    #[test]
    fn test_baseline_kind() {
        let schedule = BaselineSchedule::new();
        assert_eq!(schedule.kind(), ScheduleKind::None);
        assert!(!schedule.modifies_intrinsic());
        assert!(!schedule.modifies_execution());
    }

    #[test]
    fn test_baseline_no_intrinsic_override() {
        let schedule = BaselineSchedule::new();
        let ctx = TxContext::default();
        assert_eq!(schedule.intrinsic_gas(&ctx), None);
    }

    #[test]
    fn test_baseline_no_opcode_delta() {
        let schedule = BaselineSchedule::new();
        let ctx = OpcodeContext::default();

        // No opcode should have any delta
        for opcode in 0x00..=0xFF {
            assert_eq!(schedule.opcode_gas_delta(opcode, &ctx), 0);
        }
    }

    #[test]
    fn test_baseline_no_precompile_delta() {
        let schedule = BaselineSchedule::new();

        // No precompile should have any delta
        for i in 1..=20 {
            let mut addr_bytes = [0u8; 20];
            addr_bytes[19] = i;
            let addr = Address::from(addr_bytes);
            assert_eq!(schedule.precompile_gas_delta(&addr, &[]), 0);
        }
    }

    #[test]
    fn test_baseline_affected_empty() {
        let schedule = BaselineSchedule::new();
        assert!(schedule.affected_opcodes().is_empty());
        assert!(schedule.affected_precompiles().is_empty());
    }
}
