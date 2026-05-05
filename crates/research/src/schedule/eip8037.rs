//! EIP-8037: State Creation Gas Cost Increase schedule.
//!
//! This schedule uses revm's native EIP-8037 implementation so state gas,
//! reservoir accounting, CREATE code-deposit charging, SSTORE refills, and
//! EIP-7702 authorization refunds follow the same execution path as fork code.

use super::{
    context::TxContext,
    traits::{GasSchedule, ScheduleKind},
};
use reth_evm::EvmEnv;
use revm::{context_interface::Cfg, primitives::hardfork::SpecId};
use revm_interpreter::gas::calculate_initial_tx_gas;

/// Gas constants from the current EIP-8037 draft.
#[derive(Debug, Clone, Copy)]
pub struct Eip8037Constants;

impl Eip8037Constants {
    /// Static cost per state byte.
    pub const CPSB: u64 = 1_174;

    /// State bytes charged for one new storage slot.
    pub const STATE_BYTES_PER_STORAGE_SET: u64 = 32;

    /// State bytes charged for one new account.
    pub const STATE_BYTES_PER_NEW_ACCOUNT: u64 = 112;

    /// State bytes charged for an EIP-7702 authorization base.
    pub const STATE_BYTES_PER_AUTH_BASE: u64 = 23;

    /// Regular gas charged for CREATE and create transactions.
    pub const CREATE_REGULAR_GAS: u64 = 9_000;

    /// Regular gas charged for SSTORE 0 -> non-zero, excluding state gas.
    pub const SSTORE_SET_REGULAR_GAS: u64 = 2_900;

    /// Regular gas charged per EIP-7702 authorization base.
    pub const AUTH_BASE_REGULAR_GAS: u64 = 7_500;

    /// State gas for creating one account.
    pub const NEW_ACCOUNT_STATE_GAS: u64 = Self::STATE_BYTES_PER_NEW_ACCOUNT * Self::CPSB;

    /// State gas for one new storage slot.
    pub const STORAGE_SET_STATE_GAS: u64 = Self::STATE_BYTES_PER_STORAGE_SET * Self::CPSB;

    /// State gas for one EIP-7702 authorization.
    pub const AUTH_STATE_GAS: u64 =
        (Self::STATE_BYTES_PER_NEW_ACCOUNT + Self::STATE_BYTES_PER_AUTH_BASE) * Self::CPSB;
}

/// EIP-8037 schedule backed by native revm state-gas accounting.
#[derive(Debug, Clone, Copy, Default)]
pub struct Eip8037Schedule;

impl Eip8037Schedule {
    /// Create a new EIP-8037 schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for Eip8037Schedule {
    fn name(&self) -> &str {
        "eip-8037"
    }

    fn description(&self) -> &str {
        "EIP-8037: static CPSB state creation gas with reservoir accounting"
    }

    fn config_fingerprint(&self) -> String {
        format!(
            "description={}|cpsb={}|storage_bytes={}|account_bytes={}|auth_bytes={}|native_revm=true",
            self.description(),
            Eip8037Constants::CPSB,
            Eip8037Constants::STATE_BYTES_PER_STORAGE_SET,
            Eip8037Constants::STATE_BYTES_PER_NEW_ACCOUNT,
            Eip8037Constants::STATE_BYTES_PER_AUTH_BASE,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        Some(
            calculate_initial_tx_gas(
                SpecId::AMSTERDAM,
                &ctx.input,
                ctx.is_create,
                ctx.access_list_accounts,
                ctx.access_list_storage_slots,
                ctx.authorization_count,
            )
            .initial_total_gas,
        )
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        let baseline_max_code_size = env.cfg_env.max_code_size();
        let baseline_max_initcode_size = env.cfg_env.max_initcode_size();

        let mut cfg = env.cfg_env.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
        cfg.limit_contract_code_size = Some(baseline_max_code_size);
        cfg.limit_contract_initcode_size = Some(baseline_max_initcode_size);
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip7708_delayed_burn_disabled = true;

        env.cfg_env = cfg;
        true
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        true
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        if ctx.is_create {
            Some("contract_creation".to_string())
        } else if ctx.authorization_count > 0 {
            Some("authorization".to_string())
        } else if ctx.has_value() && ctx.recipient_info.as_ref().is_none_or(|info| !info.exists) {
            Some("transfer_new_account".to_string())
        } else {
            None
        }
    }

    fn affected_opcodes(&self) -> Vec<u8> {
        vec![0x55, 0xF0, 0xF1, 0xF2, 0xF4, 0xF5, 0xFA]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TxContext;
    use alloy_primitives::{Address, Bytes, U256};
    use reth_evm::EvmEnv;

    #[test]
    fn test_create_intrinsic_gas_uses_static_cpsb() {
        let schedule = Eip8037Schedule::new();
        let ctx = TxContext {
            baseline_intrinsic_gas: 53_000,
            sender: Address::repeat_byte(0x01),
            recipient: None,
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create: true,
            recipient_info: None,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        };

        let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
        assert_eq!(
            intrinsic,
            21_000 + Eip8037Constants::CREATE_REGULAR_GAS + Eip8037Constants::NEW_ACCOUNT_STATE_GAS
        );
    }

    #[test]
    fn test_authorization_intrinsic_gas_uses_static_cpsb() {
        let schedule = Eip8037Schedule::new();
        let ctx = TxContext {
            baseline_intrinsic_gas: 58_500,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create: false,
            recipient_info: None,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 1,
        };

        let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();
        assert_eq!(
            intrinsic,
            21_000 + Eip8037Constants::AUTH_BASE_REGULAR_GAS + Eip8037Constants::AUTH_STATE_GAS
        );
    }

    #[test]
    fn test_configure_evm_env_preserves_current_code_limits() {
        let schedule = Eip8037Schedule::new();
        let mut env = EvmEnv::default();
        env.cfg_env.limit_contract_code_size = Some(24_576);
        env.cfg_env.limit_contract_initcode_size = Some(49_152);

        assert!(schedule.configure_evm_env(&mut env));
        assert_eq!(env.cfg_env.spec, SpecId::AMSTERDAM);
        assert!(env.cfg_env.is_amsterdam_eip8037_enabled());
        assert_eq!(env.cfg_env.max_code_size(), 24_576);
        assert_eq!(env.cfg_env.max_initcode_size(), 49_152);
    }
}
