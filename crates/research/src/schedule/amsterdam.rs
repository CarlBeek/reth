//! Amsterdam (Glamsterdam) repricing schedule — revm's native `SpecId::AMSTERDAM`.
//!
//! This lane replays a historical block under Amsterdam gas rules so the result
//! can be compared against the block's canonical (Osaka) execution.
//!
//! # Why this file carries no constant table and no gas overrides
//!
//! Since revm 43 the Amsterdam gas table *is* the merged spec. revm implements
//! EIP-2780 (intrinsic decomposition), EIP-8037 (state gas / reservoir) and
//! EIP-8038 (state-access repricing) natively, sourcing every value from
//! `revm::primitives::{eip2780, eip8037, eip8038}`, and it charges the EIP-8038
//! `EXTCODESIZE`/`EXTCODECOPY` second-database-read surcharge that has no
//! `GasId` slot.
//!
//! Earlier revisions of this crate hand-ported the constant table, overlaid ~25
//! `GasParams` slots, and added the EXT\* surcharge out of band, because revm
//! 40's Amsterdam was several devnet iterations stale. None of that is true any
//! more, and a duplicate table is strictly worse than none: it can only drift,
//! and an out-of-band surcharge on top of a charge revm now makes would
//! *double-charge*. So this schedule overrides nothing and adds nothing — it
//! switches the spec and gets out of the way.
//!
//! `tests/amsterdam_matches_execution_specs.rs` is what makes that safe. It pins
//! revm's constants, the composed table slots, the intrinsic for each recipient
//! arm, and — behaviorally, by measuring real EVM executions — the EXT\*
//! surcharge, so a revm bump that renumbers Amsterdam fails loudly instead of
//! silently re-pricing a dataset.
//!
//! # What is *not* reachable through the gas table
//!
//! `initial_tx_gas`'s EIP-2780 branch reads `TX_BASE`, `TX_VALUE_COST` and the
//! recipient's `COLD_ACCOUNT_ACCESS` as compile-time constants, not table slots
//! (`tx_base_stipend` stays at the legacy 21,000 to serve the pre-2780 branch).
//! A sweep over those three has to patch revm; overriding `GasParams` silently
//! does nothing. See the test module docs.

use super::{
    common::initial_and_floor_gas_for,
    context::TxContext,
    traits::{GasSchedule, ScheduleKind},
};
use reth_evm::EvmEnv;
use revm::{
    context_interface::{cfg::gas::InitialAndFloorGas, Cfg},
    primitives::{eip2780, eip8037, eip8038, hardfork::SpecId},
};

/// Replays under revm's native `SpecId::AMSTERDAM` gas rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct AmsterdamSchedule;

impl AmsterdamSchedule {
    /// Create a new Amsterdam schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for AmsterdamSchedule {
    fn name(&self) -> &str {
        "amsterdam"
    }

    fn description(&self) -> &str {
        "Amsterdam repricing (EIP-2780 + 7976 + 7981 + 8037 + 8038) via revm's native SpecId::AMSTERDAM"
    }

    fn config_fingerprint(&self) -> String {
        // Read straight off revm's constants rather than a local copy, so a
        // devnet renumber that lands in a revm bump re-keys the dataset instead
        // of silently mixing rows priced under different rules.
        format!(
            "name=amsterdam|source=revm-native|\
             warm={}|cold_account={}|cold_storage={}|storage_write={}|account_write={}|\
             call_value={}|create_access={}|storage_clear_refund={}|\
             al_address={}|al_key={}|auth_base_regular={}|token_floor={}|\
             tx_base={}|tx_value={}|\
             cpsb={}|account_bytes={}|storage_bytes={}|auth_bytes={}|\
             eip7708=disabled|eip8246=disabled",
            eip8038::WARM_ACCESS,
            eip8038::COLD_ACCOUNT_ACCESS,
            eip8038::COLD_STORAGE_ACCESS,
            eip8038::STORAGE_WRITE,
            eip8038::ACCOUNT_WRITE,
            eip8038::CALL_VALUE,
            eip8038::CREATE_ACCESS,
            eip8038::STORAGE_CLEAR_REFUND,
            eip8038::ACCESS_LIST_ADDRESS_COST,
            eip8038::ACCESS_LIST_STORAGE_KEY_COST,
            eip8038::EIP7702_PER_AUTH_BASE_REGULAR,
            eip8038::TX_DATA_TOKEN_FLOOR,
            eip2780::TX_BASE_COST,
            eip2780::TX_VALUE_COST,
            eip8037::CPSB_GLAMSTERDAM,
            eip8037::NEW_ACCOUNT_BYTES,
            eip8037::SSTORE_SET_BYTES,
            eip8037::AUTH_BASE_BYTES,
        )
    }

    fn kind(&self) -> ScheduleKind {
        // `Both`: the spec switch reprices the intrinsic *and* execution gas.
        // The registry only replays schedules whose kind covers execution.
        ScheduleKind::Both
    }

    fn replay_bump_multiplier(&self) -> Option<u64> {
        // The state-gas reservoir dominates the headroom question under
        // EIP-8037, so a replay at the original limit would OOG on transactions
        // the spec would run. 10x matches what the earlier 8037 lanes used, so
        // the datasets stay comparable.
        Some(10)
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        Some(self.initial_and_floor_gas(ctx)?.initial_total_gas())
    }

    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        Some(initial_and_floor_gas_for(ctx, SpecId::AMSTERDAM))
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        // Preserve the block's code-size limits across the spec switch — the
        // Amsterdam defaults would otherwise silently replace them.
        let baseline_max_code_size = env.cfg_env.max_code_size();
        let baseline_max_initcode_size = env.cfg_env.max_initcode_size();

        let mut cfg = env.cfg_env.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
        cfg.limit_contract_code_size = Some(baseline_max_code_size);
        cfg.limit_contract_initcode_size = Some(baseline_max_initcode_size);
        // EIP-7708 (ETH-transfer logs) and EIP-8246 (delayed clearing of
        // self-destructed accounts) are separate proposals riding the same
        // hardfork. Keeping both off isolates this lane to the gas repricing,
        // which is what the comparison against the Osaka baseline measures.
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip8246_delayed_clear_disabled = true;

        env.cfg_env = cfg;
        true
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        // The EVM charges the Amsterdam intrinsic itself, so the replay executes
        // on the correct budget: its OOG points and its cold/warm access pattern
        // are faithful, not just its reported totals. The runner must not
        // compensate.
        true
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        // Mirrors the spec's own intrinsic branching, so the category explains
        // which recipient-cost arm a transaction took.
        if ctx.is_create {
            Some("contract_creation".to_string())
        } else if ctx.is_self_transfer() {
            Some("self_transfer".to_string())
        } else if ctx.authorization_count > 0 {
            Some("authorization".to_string())
        } else if ctx.has_value() {
            Some("value_call".to_string())
        } else {
            Some("plain_call".to_string())
        }
    }
}
