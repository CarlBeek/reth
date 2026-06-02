//! EIP-8038: state access / write gas repricing schedule.
//!
//! EIP-8038 (latest draft) reprices the *regular* gas of state-access and
//! state-write operations to reflect client performance under a grown state. In
//! the draft every repriced constant is exactly a **3×** bump:
//!
//! ```text
//!   WARM_ACCESS                  100  →    300
//!   COLD_ACCOUNT_ACCESS        2_600  →  7_800
//!   COLD_STORAGE_ACCESS        2_100  →  6_300
//!   STORAGE_WRITE              2_800  →  8_400
//!   ACCOUNT_WRITE              6_700  → 20_100
//!   CREATE_ACCESS              7_000  → 21_000
//!   STORAGE_CLEAR_REFUND       4_800  → 14_400
//!   ACCESS_LIST_STORAGE_KEY    1_900  →  5_700
//!   ACCESS_LIST_ADDRESS        2_400  →  7_200
//! ```
//!
//! # Independence from EIP-8037
//!
//! This schedule is deliberately independent of [`Eip8037Schedule`](super::Eip8037Schedule):
//! it keeps the block's **native spec** (it does NOT switch to `AMSTERDAM`, which
//! would pull in 8037's state-gas reservoir and EIP-7976's calldata floor) and
//! only scales the affected gas-param slots. revm's own cold/warm /
//! zero→non-zero / new-account conditional logic still decides *when* each cost
//! applies; we scale only the per-slot magnitude.
//!
//! # Baseline of the write/create constants
//!
//! The *access* constants (`WARM_ACCESS`, `COLD_*`, `ACCESS_LIST_*`,
//! `STORAGE_CLEAR_REFUND`) are stated relative to today's mainnet values, so
//! tripling the native slot reproduces the draft's proposed column exactly. The
//! *write/create* constants (`STORAGE_WRITE`, `ACCOUNT_WRITE`, `CREATE_ACCESS`)
//! are stated relative to the EIP-8037 *split* (their "current" column is the
//! 8037 regular portion, e.g. `2_800`, not mainnet's `20_000`). To keep this
//! schedule independent of 8037 we apply the same uniform 3× to the *native*
//! mainnet cost of those operations — i.e. we measure "what if these ops cost
//! 3× what they cost on mainnet today". This is the faithful 8037-independent
//! reading of EIP-8038's intent; switch to absolute draft values only if the
//! schedule is ever rebased onto the 8037 split.

use super::{
    context::TxContext,
    traits::{GasSchedule, ScheduleKind},
};
use reth_evm::EvmEnv;
use revm::{
    context_interface::cfg::gas_params::{GasId, GasParams},
    primitives::hardfork::SpecId,
};

/// Reference spec used to read the protocol-stable per-item intrinsic costs
/// (access-list address/key, create-transaction). Only their *deltas* are used,
/// so the block-spec base/calldata costs cancel; any post-Berlin spec yields the
/// same values. `OSAKA` is the current mainnet default.
const REF_SPEC: SpecId = SpecId::OSAKA;

/// EIP-8038 gas constants (latest draft). Stored as `(current, proposed)` pairs
/// purely for documentation, the `config_fingerprint`, and golden tests; the
/// schedule applies the uniform [`Self::MULTIPLIER`] to the native slots at
/// runtime (see the module docs on the write/create baseline).
#[derive(Debug, Clone, Copy)]
pub struct Eip8038Constants;

impl Eip8038Constants {
    /// Uniform repricing multiplier — every EIP-8038 draft constant is `3×`.
    pub const MULTIPLIER: u64 = 3;

    /// Warm storage / account access.
    pub const WARM_ACCESS: (u64, u64) = (100, 300);
    /// Cold account access (account first touched in a tx).
    pub const COLD_ACCOUNT_ACCESS: (u64, u64) = (2_600, 7_800);
    /// Cold storage slot access (SLOAD/SSTORE first touch).
    pub const COLD_STORAGE_ACCESS: (u64, u64) = (2_100, 6_300);
    /// Regular gas for writing a storage slot (SSTORE set).
    pub const STORAGE_WRITE: (u64, u64) = (2_800, 8_400);
    /// Regular gas for creating a new account via CALL/CALLCODE/SELFDESTRUCT.
    pub const ACCOUNT_WRITE: (u64, u64) = (6_700, 20_100);
    /// Regular gas for CREATE / create transactions.
    pub const CREATE_ACCESS: (u64, u64) = (7_000, 21_000);
    /// Refund for clearing a storage slot (set → zero).
    pub const STORAGE_CLEAR_REFUND: (u64, u64) = (4_800, 14_400);
    /// Intrinsic cost per access-list storage key (EIP-2930).
    pub const ACCESS_LIST_STORAGE_KEY_COST: (u64, u64) = (1_900, 5_700);
    /// Intrinsic cost per access-list address (EIP-2930).
    pub const ACCESS_LIST_ADDRESS_COST: (u64, u64) = (2_400, 7_200);
}

/// Gas-param slots scaled at execution time (opcode costs). Intrinsic slots
/// (`tx_access_list_*`, `tx_create_cost`) are handled in [`Eip8038Schedule::intrinsic_gas`]
/// instead, because revm's initial-tx-gas helper ignores `cfg.gas_params` overrides.
const fn execution_slots() -> [GasId; 9] {
    [
        GasId::warm_storage_read_cost(),
        GasId::cold_account_additional_cost(),
        GasId::cold_storage_additional_cost(),
        GasId::cold_storage_cost(),
        GasId::sstore_static(),
        GasId::sstore_set_without_load_cost(),
        GasId::new_account_cost(),
        GasId::create(),
        GasId::sstore_clearing_slot_refund(),
    ]
}

/// EIP-8038 access/write repricing, applied as a uniform 3× scale on the block's
/// native gas-param table.
#[derive(Debug, Clone, Copy, Default)]
pub struct Eip8038Schedule;

impl Eip8038Schedule {
    /// Create a new EIP-8038 schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for Eip8038Schedule {
    fn name(&self) -> &str {
        "eip-8038"
    }

    fn description(&self) -> &str {
        "EIP-8038 latest draft: 3x state access/write/create gas (independent of EIP-8037 state-gas)"
    }

    fn config_fingerprint(&self) -> String {
        // Includes every repriced constant so a draft renumber invalidates only
        // the eip-8038 rows (the 8037 fingerprint is untouched).
        format!(
            "description={}|multiplier={}|warm={:?}|cold_account={:?}|cold_storage={:?}|\
             storage_write={:?}|account_write={:?}|create_access={:?}|storage_clear_refund={:?}|\
             access_list_key={:?}|access_list_address={:?}|baseline=native-3x|ref_spec={:?}",
            self.description(),
            Eip8038Constants::MULTIPLIER,
            Eip8038Constants::WARM_ACCESS,
            Eip8038Constants::COLD_ACCOUNT_ACCESS,
            Eip8038Constants::COLD_STORAGE_ACCESS,
            Eip8038Constants::STORAGE_WRITE,
            Eip8038Constants::ACCOUNT_WRITE,
            Eip8038Constants::CREATE_ACCESS,
            Eip8038Constants::STORAGE_CLEAR_REFUND,
            Eip8038Constants::ACCESS_LIST_STORAGE_KEY_COST,
            Eip8038Constants::ACCESS_LIST_ADDRESS_COST,
            REF_SPEC,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        // EIP-8038 reprices the access-list intrinsic costs and (for create
        // transactions) the create cost. revm's initial-tx-gas helper ignores
        // `cfg.gas_params` overrides, so the intrinsic change is modelled here
        // as a delta on top of the block's canonical intrinsic — the block-spec
        // base/calldata costs cancel, and the per-item native costs (read from a
        // recent reference spec) are protocol-stable.
        let base = GasParams::new_spec(REF_SPEC);
        let extra_per = |id: GasId| base.get(id).saturating_mul(Eip8038Constants::MULTIPLIER - 1);

        let mut extra = ctx
            .access_list_accounts
            .saturating_mul(extra_per(GasId::tx_access_list_address_cost()));
        extra = extra.saturating_add(
            ctx.access_list_storage_slots
                .saturating_mul(extra_per(GasId::tx_access_list_storage_key_cost())),
        );
        if ctx.is_create {
            extra = extra.saturating_add(extra_per(GasId::tx_create_cost()));
        }

        Some(ctx.baseline_intrinsic_gas.saturating_add(extra))
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        let mut cfg = env.cfg_env.clone();
        let m = Eip8038Constants::MULTIPLIER;

        // Read each native (block-spec) slot, then scale by the uniform 3×.
        // revm's conditional handlers decide *when* each cost applies; we only
        // change the magnitude. The intrinsic slots are intentionally left alone
        // here (see `intrinsic_gas`).
        let scaled = execution_slots().map(|id| (id, cfg.gas_params.get(id).saturating_mul(m)));
        cfg.gas_params.override_gas(scaled);

        env.cfg_env = cfg;
        true
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        if ctx.is_create {
            Some("contract_creation".to_string())
        } else if ctx.access_list_accounts > 0 || ctx.access_list_storage_slots > 0 {
            Some("access_list".to_string())
        } else {
            None
        }
    }

    fn affected_opcodes(&self) -> Vec<u8> {
        vec![
            0x31, // BALANCE
            0x3B, // EXTCODESIZE
            0x3C, // EXTCODECOPY
            0x3F, // EXTCODEHASH
            0x54, // SLOAD
            0x55, // SSTORE
            0xF0, // CREATE
            0xF1, // CALL
            0xF2, // CALLCODE
            0xF4, // DELEGATECALL
            0xF5, // CREATE2
            0xFA, // STATICCALL
            0xFF, // SELFDESTRUCT
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TxContext;
    use alloy_primitives::{Address, Bytes, U256};

    /// Lock the uniform-3× property: a draft change that breaks it must be a
    /// deliberate edit (the whole `configure_evm_env`/`intrinsic_gas` math
    /// assumes a single multiplier).
    #[test]
    fn constants_are_uniform_3x() {
        let pairs = [
            Eip8038Constants::WARM_ACCESS,
            Eip8038Constants::COLD_ACCOUNT_ACCESS,
            Eip8038Constants::COLD_STORAGE_ACCESS,
            Eip8038Constants::STORAGE_WRITE,
            Eip8038Constants::ACCOUNT_WRITE,
            Eip8038Constants::CREATE_ACCESS,
            Eip8038Constants::STORAGE_CLEAR_REFUND,
            Eip8038Constants::ACCESS_LIST_STORAGE_KEY_COST,
            Eip8038Constants::ACCESS_LIST_ADDRESS_COST,
        ];
        for (current, proposed) in pairs {
            assert_eq!(proposed, current * Eip8038Constants::MULTIPLIER, "ratio for {current}");
        }
    }

    #[test]
    fn schedule_metadata() {
        let s = Eip8038Schedule::new();
        assert_eq!(s.name(), "eip-8038");
        assert_eq!(s.kind(), ScheduleKind::Both);
        assert!(s.modifies_intrinsic());
        assert!(s.modifies_execution());
        // Native intrinsic is NOT enforced by the env (revm ignores gas_params
        // overrides for intrinsic), so the runner compensates via intrinsic_gas.
        assert!(!s.uses_native_intrinsic_gas());
        assert!(s.affected_opcodes().contains(&0x54)); // SLOAD
        assert!(s.affected_opcodes().contains(&0x55)); // SSTORE
    }

    #[test]
    fn configure_evm_env_triples_execution_slots() {
        let schedule = Eip8038Schedule::new();
        let mut env = EvmEnv::default();
        let original_spec = env.cfg_env.spec;

        // Snapshot native values, apply the schedule, assert each slot tripled.
        let native: Vec<(GasId, u64)> =
            execution_slots().iter().map(|&id| (id, env.cfg_env.gas_params.get(id))).collect();

        assert!(schedule.configure_evm_env(&mut env));
        // The spec is unchanged — EIP-8038 stays on the native fork.
        assert_eq!(env.cfg_env.spec, original_spec);

        for (id, before) in native {
            assert_eq!(
                env.cfg_env.gas_params.get(id),
                before * Eip8038Constants::MULTIPLIER,
                "slot {id:?} should be tripled"
            );
        }

        // Effective cold-access costs (warm base + cold additional) land on the
        // draft's proposed column for the access constants.
        let warm = env.cfg_env.gas_params.get(GasId::warm_storage_read_cost());
        let cold_account = warm + env.cfg_env.gas_params.get(GasId::cold_account_additional_cost());
        assert_eq!(warm, Eip8038Constants::WARM_ACCESS.1);
        assert_eq!(cold_account, Eip8038Constants::COLD_ACCOUNT_ACCESS.1);
        assert_eq!(
            env.cfg_env.gas_params.get(GasId::cold_storage_cost()),
            Eip8038Constants::COLD_STORAGE_ACCESS.1
        );
        assert_eq!(
            env.cfg_env.gas_params.get(GasId::sstore_clearing_slot_refund()),
            Eip8038Constants::STORAGE_CLEAR_REFUND.1
        );
    }

    fn ctx(is_create: bool, accounts: u64, slots: u64) -> TxContext {
        TxContext {
            baseline_intrinsic_gas: 21_000,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create,
            recipient_info: None,
            access_list_accounts: accounts,
            access_list_storage_slots: slots,
            authorization_count: 0,
        }
    }

    #[test]
    fn intrinsic_adds_access_list_deltas() {
        let schedule = Eip8038Schedule::new();
        // 2 addresses (+4800 each) + 3 keys (+3800 each) on top of baseline.
        let intrinsic = schedule.intrinsic_gas(&ctx(false, 2, 3)).unwrap();
        let addr_delta = (Eip8038Constants::ACCESS_LIST_ADDRESS_COST.1 -
            Eip8038Constants::ACCESS_LIST_ADDRESS_COST.0) *
            2;
        let key_delta = (Eip8038Constants::ACCESS_LIST_STORAGE_KEY_COST.1 -
            Eip8038Constants::ACCESS_LIST_STORAGE_KEY_COST.0) *
            3;
        assert_eq!(intrinsic, 21_000 + addr_delta + key_delta);
    }

    #[test]
    fn intrinsic_adds_create_delta() {
        let schedule = Eip8038Schedule::new();
        let with_create = schedule.intrinsic_gas(&ctx(true, 0, 0)).unwrap();
        let without = schedule.intrinsic_gas(&ctx(false, 0, 0)).unwrap();
        // Create txs pay an extra 2× the native create-transaction cost.
        let native_create = GasParams::new_spec(REF_SPEC).get(GasId::tx_create_cost());
        assert_eq!(with_create - without, native_create * (Eip8038Constants::MULTIPLIER - 1));
        assert_eq!(without, 21_000);
    }

    #[test]
    fn config_fingerprint_contains_constants() {
        let fp = Eip8038Schedule::new().config_fingerprint();
        assert!(fp.contains("multiplier=3"));
        assert!(fp.contains("cold_account=(2600, 7800)"));
        assert!(fp.contains("baseline=native-3x"));
    }
}
