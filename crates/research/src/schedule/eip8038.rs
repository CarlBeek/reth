//! EIP-8038: state access / write gas repricing schedule (PR ethereum/EIPs#11802).
//!
//! EIP-8038 reprices state-access and state-write gas from client runtime
//! benchmarks. PR 11802 made the schedule **uniform**: it dropped the earlier
//! code/no-code account distinction and left `WARM_ACCESS` unchanged at 100.
//! This schedule applies it on the block's native spec as an **independent**
//! experiment (its own `config_fingerprint` → its own dataset; never benchmarked
//! against EIP-8037, and it does NOT pull in 8037's state-gas reservoir or 7976
//! floor). 8038-standalone therefore isolates the *regular-gas* reprice: the
//! state-gas portion that EIP-8037 would add (e.g. `GAS_NEW_ACCOUNT`,
//! `GAS_STORAGE_SET`) is **not** charged here.
//!
//! Proposed parameters (current → PR-11802):
//! ```text
//!   WARM_ACCESS                 100 →    100   (unchanged)
//!   COLD_ACCOUNT_ACCESS        2600 →   3000
//!   COLD_STORAGE_ACCESS        2100 →   3000
//!   STORAGE_WRITE              2800 →  10000   (surcharge, on top of the access)
//!   ACCOUNT_WRITE              6700 →   8000
//!   STORAGE_CLEAR_REFUND       4800 →  12480   (= (10000 + 3000) · 0.96)
//!   CREATE_ACCESS              7000 →  11000   (= ACCOUNT_WRITE + COLD_STORAGE_ACCESS)
//!   ACCESS_LIST_ADDRESS_COST   2400 →   3000
//!   ACCESS_LIST_STORAGE_KEY    1900 →   3000
//! ```
//!
//! # Charging — native `gas_params`, plus one inspector delta
//!
//! Because `WARM_ACCESS` is unchanged and there is no code/no-code split, **every**
//! 8038 cost is a stock revm `gas_params` slot that revm already reads at charge
//! time — so charging is native (no vendored revm changes for 8038):
//!
//! 1. [`apply_gas_overrides`] writes the back-solved **absolute** slot values onto the block's
//!    native gas-param table. revm adds the cold add-ons / write surcharges on top of the warm
//!    base, so each add-on is `value − WARM_ACCESS`. The intrinsic access-list slots are overridden
//!    here too, so the EVM charges the repriced intrinsic itself (the vendored handler's
//!    `validate_initial_tx_gas` reads `cfg.gas_params`); hence
//!    [`uses_native_intrinsic_gas`](Eip8038Schedule::uses_native_intrinsic_gas) is `true` and the
//!    runner applies no intrinsic compensation.
//! 2. [`opcode_gas_delta`](Eip8038Schedule::opcode_gas_delta) carries the **only** charge with no
//!    gas-param slot: `EXTCODESIZE`/`EXTCODECOPY` are charged an additional `WARM_ACCESS` for their
//!    second database read (EIP-8038 §"EXT* family update"). Stock revm charges a single access, so
//!    the `+100` rides the inspector. Everything else has `opcode_gas_delta == 0`.
//!
//! # Create transactions (deliberately not repriced)
//!
//! `CREATE_ACCESS` is scoped to the `CREATE`/`CREATE2` **opcodes** (PR 11802
//! §"`CREATE`/`CREATE2`"); the EIP gives no formula for the create-**transaction**
//! intrinsic. We therefore leave `tx_create_cost` at revm's native value rather
//! than guess — recorded in the fingerprint as `tx_create_repriced=false`. Revisit
//! if the EIP later specifies the create-tx intrinsic.

use super::{
    common::initial_and_floor_gas_for,
    context::{OpcodeContext, TxContext},
    traits::{GasSchedule, GasTaxBreakdown, ScheduleKind},
};
use crate::opcode;
use reth_evm::EvmEnv;
use revm::{
    context_interface::cfg::{
        gas::InitialAndFloorGas,
        gas_params::{GasId, GasParams},
    },
    primitives::hardfork::SpecId,
};

/// Reference spec for the native intrinsic calculation. EIP-8038 stays on the
/// block's native spec for execution; the intrinsic helper needs a concrete spec,
/// and the only repriced intrinsic items (access-list address/key) are
/// protocol-stable since Berlin, so the spec base cancels in the runner's delta.
/// `OSAKA` is the current mainnet default.
const REF_SPEC: SpecId = SpecId::OSAKA;

/// EIP-8038 gas constants (PR 11802 uniform parameters). Stored as
/// `(current, proposed)` pairs for the `config_fingerprint`, golden tests, and
/// documentation; the schedule applies the proposed column as absolute slot
/// overrides (see [`apply_gas_overrides`]).
#[derive(Debug, Clone, Copy)]
pub struct Eip8038Constants;

impl Eip8038Constants {
    /// Warm storage / account access. Unchanged by EIP-8038.
    pub const WARM_ACCESS: (u64, u64) = (100, 100);
    /// Cold account access (`*CALL` / `BALANCE` / `EXT*` / `SELFDESTRUCT`). Single
    /// value — PR 11802 dropped the code/no-code distinction.
    pub const COLD_ACCOUNT_ACCESS: (u64, u64) = (2_600, 3_000);
    /// Cold storage slot access (SLOAD / cold SSTORE first touch).
    pub const COLD_STORAGE_ACCESS: (u64, u64) = (2_100, 3_000);
    /// SSTORE write surcharge, charged on top of the storage access on a
    /// first-time value change (set or reset).
    pub const STORAGE_WRITE: (u64, u64) = (2_800, 10_000);
    /// Account write surcharge (new account via CALL / SELFDESTRUCT to empty /
    /// value transfer).
    pub const ACCOUNT_WRITE: (u64, u64) = (6_700, 8_000);
    /// Refund for clearing a storage slot (non-zero → zero).
    pub const REFUND_STORAGE_CLEAR: (u64, u64) = (4_800, 12_480);
    /// Intrinsic cost per access-list address (EIP-2930).
    pub const TX_ACCESS_LIST_ADDRESS: (u64, u64) = (2_400, 3_000);
    /// Intrinsic cost per access-list storage key (EIP-2930).
    pub const TX_ACCESS_LIST_STORAGE_KEY: (u64, u64) = (1_900, 3_000);

    /// `CALL_STIPEND` (2300) — unchanged by EIP-8038; used to derive the
    /// value-transfer charge.
    pub const CALL_STIPEND: u64 = 2_300;

    /// `CREATE_ACCESS` = `ACCOUNT_WRITE` + `COLD_STORAGE_ACCESS` (PR 11802); the
    /// regular gas for the `CREATE`/`CREATE2` opcodes.
    pub const CREATE: u64 = Self::ACCOUNT_WRITE.1 + Self::COLD_STORAGE_ACCESS.1; // 11_000

    /// `CALL_VALUE` = `ACCOUNT_WRITE` + `CALL_STIPEND` (PR 11802 §"`CALL`/
    /// `CALLCODE`") — the caller's value-transfer charge (revm's
    /// `transfer_value_cost` slot, native `CALLVALUE` = 9000 = 6700 + 2300).
    pub const CALL_VALUE: u64 = Self::ACCOUNT_WRITE.1 + Self::CALL_STIPEND; // 10_300

    /// `EXTCODESIZE`/`EXTCODECOPY` second-database-read surcharge = `WARM_ACCESS`.
    pub const EXT_SECOND_READ: u64 = Self::WARM_ACCESS.1; // 100
}

/// EIP-8038's absolute `gas_params` slot overrides (PR 11802), shared by
/// [`apply_gas_overrides`] (runtime charging) and the native intrinsic path
/// ([`initial_and_floor_gas_for`]) so they never drift.
///
/// revm adds the cold add-ons / write surcharges on top of a warm base, so each
/// cold add-on is `total − WARM_ACCESS`. `WARM_ACCESS` is set explicitly (though
/// unchanged at 100) so the back-solve is self-consistent. `tx_create_cost` is
/// intentionally absent — see the module docs.
const fn eip8038_overrides() -> [(GasId, u64); 16] {
    let warm = Eip8038Constants::WARM_ACCESS.1;
    let cold_account_addon = Eip8038Constants::COLD_ACCOUNT_ACCESS.1 - warm; // 2900
    let cold_storage_addon = Eip8038Constants::COLD_STORAGE_ACCESS.1 - warm; // 2900
    let storage_write = Eip8038Constants::STORAGE_WRITE.1; // 10000
    let account_write = Eip8038Constants::ACCOUNT_WRITE.1; // 8000

    [
        // Warm access (unchanged at 100; explicit for a self-consistent back-solve).
        (GasId::warm_storage_read_cost(), warm),
        (GasId::sstore_static(), warm),
        // Cold add-ons over the warm base. SLOAD = warm + cold_storage_additional;
        // cold SSTORE = sstore_static(warm) + cold_storage_cost — both reach
        // COLD_STORAGE_ACCESS.
        (GasId::cold_storage_cost(), cold_storage_addon),
        (GasId::cold_storage_additional_cost(), cold_storage_addon),
        // Account cold add-on: warm + add-on = COLD_ACCOUNT_ACCESS.
        (GasId::cold_account_additional_cost(), cold_account_addon),
        // SSTORE write surcharge (set & reset), on top of the storage access.
        (GasId::sstore_set_without_load_cost(), storage_write),
        (GasId::sstore_reset_without_cold_load_cost(), storage_write),
        // Refunds: STORAGE_WRITE refunded on reset-to-original; STORAGE_CLEAR on clear.
        (GasId::sstore_set_refund(), storage_write),
        (GasId::sstore_reset_refund(), storage_write),
        (GasId::sstore_clearing_slot_refund(), Eip8038Constants::REFUND_STORAGE_CLEAR.1),
        // Account writes: new account via CALL / SELFDESTRUCT-to-empty.
        (GasId::new_account_cost(), account_write),
        (GasId::new_account_cost_for_selfdestruct(), account_write),
        // Value-transfer CALL charge: CALL_VALUE = ACCOUNT_WRITE + CALL_STIPEND.
        (GasId::transfer_value_cost(), Eip8038Constants::CALL_VALUE),
        // CREATE/CREATE2 opcode regular gas (CREATE_ACCESS).
        (GasId::create(), Eip8038Constants::CREATE),
        // Intrinsic per-item access-list costs (EIP-2930 access list).
        (GasId::tx_access_list_address_cost(), Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1),
        (GasId::tx_access_list_storage_key_cost(), Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1),
    ]
}

/// Apply EIP-8038's absolute `gas_params` slot overrides to `gas_params`.
pub fn apply_gas_overrides(gas_params: &mut GasParams) {
    gas_params.override_gas(eip8038_overrides());
}

/// EIP-8038 (PR 11802) uniform state access/write repricing on the block's native
/// spec.
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
        "EIP-8038 (PR 11802 uniform reprice): state access/write repricing, no code/no-code split, native spec, independent of EIP-8037"
    }

    fn config_fingerprint(&self) -> String {
        // Every proposed value + the model tag, so any renumber invalidates only
        // the eip-8038 rows. `tx_create_repriced=false` records that the
        // create-transaction intrinsic is left at revm's native value.
        format!(
            "name=eip-8038|model=pr-11802-unified|warm={}|cold_account={}|cold_storage={}|\
             storage_write={}|account_write={}|refund_storage_clear={}|create={}|call_value={}|\
             access_list_address={}|access_list_key={}|ext_second_read={}|tx_create_repriced=false|\
             baseline=native-osaka-absolute|ref_spec={REF_SPEC:?}",
            Eip8038Constants::WARM_ACCESS.1,
            Eip8038Constants::COLD_ACCOUNT_ACCESS.1,
            Eip8038Constants::COLD_STORAGE_ACCESS.1,
            Eip8038Constants::STORAGE_WRITE.1,
            Eip8038Constants::ACCOUNT_WRITE.1,
            Eip8038Constants::REFUND_STORAGE_CLEAR.1,
            Eip8038Constants::CREATE,
            Eip8038Constants::CALL_VALUE,
            Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1,
            Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1,
            Eip8038Constants::EXT_SECOND_READ,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn replay_bump_multiplier(&self) -> Option<u64> {
        Some(4)
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        // Native intrinsic through the overridden gas-param table. The overrides
        // touch only the access-list slots, so this differs from the native
        // intrinsic exactly by the access-list reprice (the create-tx intrinsic is
        // left native — see the module docs).
        Some(initial_and_floor_gas_for(ctx, REF_SPEC, &eip8038_overrides()).initial_total_gas())
    }

    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        Some(initial_and_floor_gas_for(ctx, REF_SPEC, &eip8038_overrides()))
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        self.opcode_gas_tax_breakdown(opcode, ctx).total()
    }

    fn opcode_gas_tax_breakdown(&self, opcode: u8, _ctx: &OpcodeContext) -> GasTaxBreakdown {
        let mut b = GasTaxBreakdown::default();

        // EIP-8038 §"EXT* family update": EXTCODESIZE / EXTCODECOPY make a second
        // database read (to fetch the code) and are charged an additional
        // WARM_ACCESS on top of the normal account access. Stock revm charges a
        // single access, so this surcharge rides the inspector. Unconditional
        // (independent of cold/warm). Every other 8038 cost is a native
        // gas_params slot, so its opcode delta is 0.
        if matches!(opcode, crate::opcode::EXTCODESIZE | crate::opcode::EXTCODECOPY) {
            b.second_db_read += Eip8038Constants::EXT_SECOND_READ as i64;
        }

        b
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        // Stay on the block's native spec; only overlay the repriced slots.
        apply_gas_overrides(&mut env.cfg_env.gas_params);
        true
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        // The intrinsic access-list slots are overridden in `configure_evm_env`,
        // so the EVM (vendored handler reading `cfg.gas_params`) charges the
        // repriced intrinsic itself — the runner must not compensate.
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
            opcode::BALANCE,
            opcode::EXTCODESIZE,
            opcode::EXTCODECOPY,
            opcode::EXTCODEHASH,
            opcode::SLOAD,
            opcode::SSTORE,
            opcode::CREATE,
            opcode::CALL,
            opcode::CALLCODE,
            opcode::DELEGATECALL,
            opcode::CREATE2,
            opcode::STATICCALL,
            opcode::SELFDESTRUCT,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::TxContext;
    use alloy_primitives::{Address, Bytes, U256};

    #[test]
    fn constants_match_pr11802() {
        assert_eq!(Eip8038Constants::WARM_ACCESS.1, 100);
        assert_eq!(Eip8038Constants::COLD_ACCOUNT_ACCESS.1, 3_000);
        assert_eq!(Eip8038Constants::COLD_STORAGE_ACCESS.1, 3_000);
        assert_eq!(Eip8038Constants::STORAGE_WRITE.1, 10_000);
        assert_eq!(Eip8038Constants::ACCOUNT_WRITE.1, 8_000);
        assert_eq!(Eip8038Constants::REFUND_STORAGE_CLEAR.1, 12_480);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1, 3_000);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1, 3_000);
        assert_eq!(Eip8038Constants::CREATE, 11_000);
        assert_eq!(Eip8038Constants::CALL_VALUE, 10_300);
        assert_eq!(Eip8038Constants::EXT_SECOND_READ, 100);
        // STORAGE_CLEAR_REFUND = (STORAGE_WRITE + COLD_STORAGE_ACCESS) · 0.96.
        assert_eq!(
            Eip8038Constants::REFUND_STORAGE_CLEAR.1,
            (Eip8038Constants::STORAGE_WRITE.1 + Eip8038Constants::COLD_STORAGE_ACCESS.1) * 96 /
                100
        );
    }

    #[test]
    fn schedule_metadata() {
        let s = Eip8038Schedule::new();
        assert_eq!(s.name(), "eip-8038");
        assert_eq!(s.kind(), ScheduleKind::Both);
        assert!(s.modifies_intrinsic());
        assert!(s.modifies_execution());
        // PR 11802: intrinsic is charged natively via overridden gas_params.
        assert!(s.uses_native_intrinsic_gas());
        assert!(s.affected_opcodes().contains(&0x55)); // SSTORE
        assert_eq!(s.replay_bump_multiplier(), Some(4)); // single [1,4] conditional bump
    }

    #[test]
    fn configure_evm_env_sets_unified_absolute_values() {
        let schedule = Eip8038Schedule::new();
        let mut env = EvmEnv::default();
        let original_spec = env.cfg_env.spec;

        assert!(schedule.configure_evm_env(&mut env));
        // EIP-8038 stays on the native fork.
        assert_eq!(env.cfg_env.spec, original_spec);

        let g = |id| env.cfg_env.gas_params.get(id);
        // Warm base unchanged.
        assert_eq!(g(GasId::warm_storage_read_cost()), 100);
        assert_eq!(g(GasId::sstore_static()), 100);
        // Cold add-ons: warm + add-on = COLD_*_ACCESS (3000).
        assert_eq!(g(GasId::warm_storage_read_cost()) + g(GasId::cold_storage_cost()), 3_000);
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_storage_additional_cost()),
            3_000
        );
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_account_additional_cost()),
            3_000
        );
        // SSTORE write surcharge + refunds.
        assert_eq!(g(GasId::sstore_set_without_load_cost()), 10_000);
        assert_eq!(g(GasId::sstore_reset_without_cold_load_cost()), 10_000);
        assert_eq!(g(GasId::sstore_set_refund()), 10_000);
        assert_eq!(g(GasId::sstore_reset_refund()), 10_000);
        assert_eq!(g(GasId::sstore_clearing_slot_refund()), 12_480);
        // Account writes + value transfer + create opcode.
        assert_eq!(g(GasId::new_account_cost()), 8_000);
        assert_eq!(g(GasId::new_account_cost_for_selfdestruct()), 8_000);
        assert_eq!(g(GasId::transfer_value_cost()), 10_300);
        assert_eq!(g(GasId::create()), 11_000);
        // Intrinsic access-list slots.
        assert_eq!(g(GasId::tx_access_list_address_cost()), 3_000);
        assert_eq!(g(GasId::tx_access_list_storage_key_cost()), 3_000);
        // The create-TRANSACTION intrinsic is left at revm's native value.
        assert_eq!(
            g(GasId::tx_create_cost()),
            GasParams::new_spec(REF_SPEC).get(GasId::tx_create_cost())
        );
    }

    #[test]
    fn opcode_gas_delta_only_extcode_second_read() {
        let s = Eip8038Schedule::new();
        let ctx = OpcodeContext::default();
        // EXTCODESIZE / EXTCODECOPY: +WARM_ACCESS for the second DB read.
        assert_eq!(s.opcode_gas_delta(0x3B, &ctx), 100); // EXTCODESIZE
        assert_eq!(s.opcode_gas_delta(0x3C, &ctx), 100); // EXTCODECOPY
                                                         // Every other affected opcode is repriced
                                                         // via native gas_params → delta 0.
        assert_eq!(s.opcode_gas_delta(0x3F, &ctx), 0); // EXTCODEHASH
        assert_eq!(s.opcode_gas_delta(0x31, &ctx), 0); // BALANCE
        assert_eq!(s.opcode_gas_delta(0x54, &ctx), 0); // SLOAD
        assert_eq!(s.opcode_gas_delta(0x55, &ctx), 0); // SSTORE
        assert_eq!(s.opcode_gas_delta(0xF1, &ctx), 0); // CALL
        assert_eq!(s.opcode_gas_delta(0x00, &ctx), 0); // STOP
    }

    #[test]
    fn opcode_gas_tax_breakdown_reconciles() {
        let s = Eip8038Schedule::new();
        let ctx = OpcodeContext::default();
        // EXTCODECOPY's only delta category is the second DB read.
        let b = s.opcode_gas_tax_breakdown(0x3C, &ctx);
        assert_eq!(b.second_db_read, 100);
        assert_eq!(b.other, 0);
        // Invariant: total() == opcode_gas_delta for every opcode.
        for opcode in [0x00u8, 0x31, 0x3B, 0x3C, 0x3F, 0x54, 0x55, 0xF1, 0xFA, 0xFF] {
            assert_eq!(
                s.opcode_gas_tax_breakdown(opcode, &ctx).total(),
                s.opcode_gas_delta(opcode, &ctx),
                "breakdown total must equal delta for opcode {opcode:#x}"
            );
        }
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
    fn intrinsic_reprices_access_list_only() {
        let schedule = Eip8038Schedule::new();
        // Delta vs the native intrinsic = the access-list reprice only.
        let native =
            initial_and_floor_gas_for(&ctx(false, 2, 3), REF_SPEC, &[]).initial_total_gas();
        let sched = schedule.intrinsic_gas(&ctx(false, 2, 3)).unwrap();
        let base = GasParams::new_spec(REF_SPEC);
        let addr_delta = Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1 -
            base.get(GasId::tx_access_list_address_cost());
        let key_delta = Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1 -
            base.get(GasId::tx_access_list_storage_key_cost());
        assert_eq!(sched - native, addr_delta * 2 + key_delta * 3);
    }

    #[test]
    fn intrinsic_does_not_reprice_create_transactions() {
        let schedule = Eip8038Schedule::new();
        let with_create = schedule.intrinsic_gas(&ctx(true, 0, 0)).unwrap();
        let without = schedule.intrinsic_gas(&ctx(false, 0, 0)).unwrap();
        // CREATE_ACCESS is opcode-only; the create-tx intrinsic stays at revm's
        // native tx_create_cost (no 8038 override).
        assert_eq!(
            with_create - without,
            GasParams::new_spec(REF_SPEC).get(GasId::tx_create_cost())
        );
    }

    #[test]
    fn config_fingerprint_reflects_unified_model() {
        let fp = Eip8038Schedule::new().config_fingerprint();
        assert!(fp.contains("model=pr-11802-unified"));
        assert!(fp.contains("warm=100"));
        assert!(fp.contains("cold_account=3000"));
        assert!(fp.contains("storage_write=10000"));
        assert!(fp.contains("create=11000"));
        assert!(fp.contains("tx_create_repriced=false"));
        assert!(!fp.contains("nocode"));
        assert!(!fp.contains("multiplier"));
    }
}
