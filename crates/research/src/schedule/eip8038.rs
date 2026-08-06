//! EIP-8038: state access / write gas repricing schedule.
//!
//! EIP-8038 reprices state-access and state-write gas from client runtime
//! benchmarks. There is no code/no-code account distinction and `WARM_ACCESS` is
//! unchanged. This schedule applies it on the block's native spec as an
//! **independent** experiment (its own `config_fingerprint` → its own dataset;
//! never benchmarked against EIP-8037, and it does NOT pull in 8037's state-gas
//! reservoir or 7976 floor). 8038-standalone therefore isolates the *regular-gas*
//! reprice: the state-gas portion that EIP-8037 would add (e.g. `GAS_NEW_ACCOUNT`,
//! `GAS_STORAGE_SET`) is **not** charged here.
//!
//! Numbers track ethereum/EIPs commit `8331fb3eed` — the EIP is a live document
//! whose parameter table has been revised repeatedly, so the fingerprint records
//! the pinned commit rather than a revision name.
//!
//! Parameters (current → EIP-8038):
//! ```text
//!   WARM_ACCESS                 100 →    100   (unchanged)
//!   COLD_ACCOUNT_ACCESS        2600 →   3000
//!   COLD_STORAGE_ACCESS        2100 →   2100   (unchanged)
//!   STORAGE_WRITE              2800 →  10000   (surcharge, on top of the access)
//!   ACCOUNT_WRITE              6700 →   9000
//!   STORAGE_CLEAR_REFUND       4800 →  11616   (= (10000 + 2100) · 4800/5000)
//!   CREATE_ACCESS              7000 →  12000   (= ACCOUNT_WRITE + COLD_ACCOUNT_ACCESS)
//!   ACCESS_LIST_ADDRESS_COST   2400 →   2900   (= COLD_ACCOUNT_ACCESS − WARM_ACCESS)
//!   ACCESS_LIST_STORAGE_KEY    1900 →   2000   (= COLD_STORAGE_ACCESS − WARM_ACCESS)
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
//!    base, so each add-on is `value − WARM_ACCESS`. The intrinsic slots (per-item access-list
//!    costs and the create-transaction cost) are overridden here too, so the EVM charges the
//!    repriced intrinsic itself (the vendored handler's `validate_initial_tx_gas` reads
//!    `cfg.gas_params`); hence
//!    [`uses_native_intrinsic_gas`](Eip8038Schedule::uses_native_intrinsic_gas) is `true` and the
//!    runner applies no intrinsic compensation.
//! 2. [`opcode_gas_delta`](Eip8038Schedule::opcode_gas_delta) carries the **only** charge with no
//!    gas-param slot: `EXTCODESIZE`/`EXTCODECOPY` are charged an additional `WARM_ACCESS` for their
//!    second database read (EIP-8038 §"EXT* family update"). Stock revm charges a single access, so
//!    the surcharge rides the inspector. Everything else has `opcode_gas_delta == 0`.
//!
//! # Access lists are cost-neutral
//!
//! The per-item intrinsic costs are `COLD_* − WARM_ACCESS`, so an entry plus the warm access it
//! enables totals exactly the cold access it replaces. Mainnet gives a 100-gas discount for
//! pre-warming; under EIP-8038 the discount is gone but listing an entry is never a penalty.
//! An entry that is listed and then never accessed is still pure waste.
//!
//! # Create transactions
//!
//! `CREATE_ACCESS` replaces the flat `GAS_CREATE` at the `CREATE`/`CREATE2` opcodes and also at the
//! create-**transaction** intrinsic, recorded in the fingerprint as `tx_create_repriced=true`.
//! Worth knowing that this rests on the EIP's parameter table (whose "Operations affected" column
//! names contract-creation txs) and on unanimous client behaviour — execution-specs charges
//! `CREATE_ACCESS` for `is_create` in `calculate_intrinsic_cost`, and revm, geth and besu all
//! reprice `tx_create_cost` — rather than on the EIP's normative prose, which mentions only the
//! opcode constant. In those clients the transaction-level charge sits inside EIP-2780's decomposed
//! intrinsic (`TX_BASE` + `CREATE_ACCESS`); this schedule simulates 8038 alone, so it keeps the
//! native 21000 base and swaps only the 32000.

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

/// EIP-8038 gas constants. Stored as `(current, proposed)` pairs for the
/// `config_fingerprint`, golden tests, and documentation; the schedule applies
/// the proposed column as absolute slot overrides (see [`apply_gas_overrides`]).
///
/// The EIP defines four of these as formulas over the others, so they are
/// written as derivations rather than literals: a renumber of a base value then
/// propagates instead of silently leaving a stale dependent behind.
#[derive(Debug, Clone, Copy)]
pub struct Eip8038Constants;

impl Eip8038Constants {
    /// Warm storage / account access. Unchanged by EIP-8038.
    ///
    /// Load-bearing beyond its own slot: revm's *static* opcode gas table
    /// hardcodes the warm base, so the `COLD − WARM` back-solve in
    /// [`eip8038_overrides`] is only valid while this equals revm's
    /// `WARM_STORAGE_READ_COST`. Pinned by a `const _` assertion below.
    pub const WARM_ACCESS: (u64, u64) = (100, 100);
    /// Cold account access (`*CALL` / `BALANCE` / `EXT*` / `SELFDESTRUCT`).
    /// Single value — EIP-8038 has no code/no-code distinction.
    pub const COLD_ACCOUNT_ACCESS: (u64, u64) = (2_600, 3_000);
    /// Cold storage slot access (SLOAD / cold SSTORE first touch). Left
    /// unrepriced by EIP-8038, but still overridden explicitly so the back-solve
    /// stays self-consistent and any future renumber re-keys the fingerprint.
    pub const COLD_STORAGE_ACCESS: (u64, u64) = (2_100, 2_100);
    /// SSTORE write surcharge, charged on top of the storage access on a
    /// first-time value change (set or reset).
    pub const STORAGE_WRITE: (u64, u64) = (2_800, 10_000);
    /// Account write surcharge, charged per operation that writes an account's
    /// leaf values. No per-account tracking and no refund — a value-bearing
    /// `CALL` to the same recipient pays it on each call.
    pub const ACCOUNT_WRITE: (u64, u64) = (6_700, 9_000);

    /// `CALL_STIPEND` (2300) — unchanged by EIP-8038; used to derive the
    /// value-transfer charge.
    pub const CALL_STIPEND: u64 = 2_300;

    /// Refund for clearing a storage slot (non-zero → zero).
    ///
    /// EIP-8038: `STORAGE_CLEAR_REFUND = (STORAGE_WRITE + COLD_STORAGE_ACCESS) *
    /// (4800/5000)`. The multiply must precede the divide — `(10000 + 2100) *
    /// 4800 / 5000` is 11,616 exactly, whereas dividing first truncates
    /// `12100 / 5000` to 2 and yields 9,600. The spec states no rounding rule,
    /// so an exact result is a property of these values, not a guarantee.
    pub const REFUND_STORAGE_CLEAR: (u64, u64) =
        (4_800, (Self::STORAGE_WRITE.1 + Self::COLD_STORAGE_ACCESS.1) * 4_800 / 5_000);

    /// Intrinsic cost per access-list address (EIP-2930).
    ///
    /// EIP-8038: `ACCESS_LIST_ADDRESS_COST = COLD_ACCOUNT_ACCESS − WARM_ACCESS`.
    /// The subtraction makes pre-warming exactly cost-neutral: the entry plus the
    /// warm access it enables totals the cold access it replaces. Charging the
    /// full cold cost would make an access list strictly more expensive than not
    /// using one.
    pub const TX_ACCESS_LIST_ADDRESS: (u64, u64) =
        (2_400, Self::COLD_ACCOUNT_ACCESS.1 - Self::WARM_ACCESS.1);

    /// Intrinsic cost per access-list storage key (EIP-2930).
    /// `ACCESS_LIST_STORAGE_KEY_COST = COLD_STORAGE_ACCESS − WARM_ACCESS`.
    pub const TX_ACCESS_LIST_STORAGE_KEY: (u64, u64) =
        (1_900, Self::COLD_STORAGE_ACCESS.1 - Self::WARM_ACCESS.1);

    /// Cold-account premium revm layers on top of the warm base.
    /// Const-eval rejects an underflow, so a cold value below warm is a build
    /// failure rather than a wrapped charge at replay time.
    const COLD_ACCOUNT_ADDON: u64 = Self::COLD_ACCOUNT_ACCESS.1 - Self::WARM_ACCESS.1;

    /// Cold-storage premium revm layers on top of the warm base.
    const COLD_STORAGE_ADDON: u64 = Self::COLD_STORAGE_ACCESS.1 - Self::WARM_ACCESS.1;

    /// `CREATE_ACCESS` = `ACCOUNT_WRITE` + `COLD_ACCOUNT_ACCESS` (EIP-8038
    /// §`CREATE`/`CREATE2`); the regular gas replacing the flat `GAS_CREATE`.
    ///
    /// The base is the **account** cold cost, not the storage one. The two were
    /// equal under the EIP's earlier numbers, which made a wrong base invisible;
    /// `create_access_derives_from_cold_account_not_cold_storage` guards it.
    pub const CREATE: u64 = Self::ACCOUNT_WRITE.1 + Self::COLD_ACCOUNT_ACCESS.1;

    /// `CALL_VALUE` = `ACCOUNT_WRITE` + `CALL_STIPEND` (EIP-8038 §`CALL`/
    /// `CALLCODE`) — the caller's value-transfer charge (revm's
    /// `transfer_value_cost` slot, native `CALLVALUE` = 9000 = 6700 + 2300).
    pub const CALL_VALUE: u64 = Self::ACCOUNT_WRITE.1 + Self::CALL_STIPEND;

    /// `EXTCODESIZE`/`EXTCODECOPY` second-database-read surcharge = `WARM_ACCESS`.
    pub const EXT_SECOND_READ: u64 = Self::WARM_ACCESS.1;
}

// Invariants the constants alone cannot express, checked at build time.
const _: () = assert!(
    Eip8038Constants::WARM_ACCESS.1 == 100,
    "revm's static opcode gas table hardcodes the warm base at 100; the COLD − WARM back-solve \
     in eip8038_overrides() silently mis-charges if WARM_ACCESS moves off it"
);
const _: () = assert!(
    Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1 + Eip8038Constants::WARM_ACCESS.1 ==
        Eip8038Constants::COLD_ACCOUNT_ACCESS.1 &&
        Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1 + Eip8038Constants::WARM_ACCESS.1 ==
            Eip8038Constants::COLD_STORAGE_ACCESS.1,
    "EIP-8038: an access-list entry plus the warm access it enables must total exactly the cold \
     access it replaces"
);

/// EIP-8038's absolute `gas_params` slot overrides, shared by
/// [`apply_gas_overrides`] (runtime charging) and the native intrinsic path
/// ([`initial_and_floor_gas_for`]) so they never drift.
///
/// revm adds the cold add-ons / write surcharges on top of a warm base, so each
/// cold add-on is `total − WARM_ACCESS`. `WARM_ACCESS` is set explicitly (though
/// unchanged) so the back-solve is self-consistent.
const fn eip8038_overrides() -> [(GasId, u64); 17] {
    let warm = Eip8038Constants::WARM_ACCESS.1;
    let cold_account_addon = Eip8038Constants::COLD_ACCOUNT_ADDON;
    let cold_storage_addon = Eip8038Constants::COLD_STORAGE_ADDON;
    let storage_write = Eip8038Constants::STORAGE_WRITE.1;
    let account_write = Eip8038Constants::ACCOUNT_WRITE.1;

    [
        // Warm access (unchanged; explicit for a self-consistent back-solve).
        (GasId::warm_storage_read_cost(), warm),
        (GasId::sstore_static(), warm),
        // Cold add-ons over the warm base. SLOAD = warm + cold_storage_additional;
        // cold SSTORE = sstore_static(warm) + cold_storage_cost — both reach
        // COLD_STORAGE_ACCESS. Note `cold_storage_cost` takes the add-on, not the
        // full cold cost it holds natively: EIP-8038's SSTORE table charges
        // COLD_STORAGE_ACCESS + STORAGE_WRITE with no separate warm term, dropping
        // the redundant 100 that today's composite 5000 carries.
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
        // A value-bearing CALL already pays ACCOUNT_WRITE inside `transfer_value_cost`,
        // and EIP-8038 leaves GAS_NEW_ACCOUNT to EIP-8037's state-gas dimension — which
        // this standalone schedule does not charge. So reaching a dead account adds no
        // regular gas; a non-zero value here would bill the account write twice.
        (GasId::new_account_cost(), 0),
        // SELFDESTRUCT has no bundled value-transfer charge, so the EIP adds a standalone
        // ACCOUNT_WRITE when it sends a positive balance to a dead account.
        (GasId::new_account_cost_for_selfdestruct(), account_write),
        // Value-transfer CALL charge: CALL_VALUE = ACCOUNT_WRITE + CALL_STIPEND.
        (GasId::transfer_value_cost(), Eip8038Constants::CALL_VALUE),
        // CREATE_ACCESS replaces the flat GAS_CREATE at the opcodes and, per the EIP's
        // parameter table and every reference client, at the create-transaction
        // intrinsic too. Both slots must move together — see the module docs.
        (GasId::create(), Eip8038Constants::CREATE),
        (GasId::tx_create_cost(), Eip8038Constants::CREATE),
        // Intrinsic per-item access-list costs (EIP-2930 access list).
        (GasId::tx_access_list_address_cost(), Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1),
        (GasId::tx_access_list_storage_key_cost(), Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1),
    ]
}

/// Apply EIP-8038's absolute `gas_params` slot overrides to `gas_params`.
pub fn apply_gas_overrides(gas_params: &mut GasParams) {
    gas_params.override_gas(eip8038_overrides());
}

/// EIP-8038 state access/write repricing on the block's native spec.
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
        "EIP-8038 state access/write repricing (no code/no-code split; cold account 3000, cold storage unchanged at 2100, storage write 10000, account write 9000), applied on the block's native spec, independent of EIP-8037"
    }

    fn config_fingerprint(&self) -> String {
        // Every proposed value + the model tag, so any renumber invalidates only
        // the eip-8038 rows. The model tag carries the pinned ethereum/EIPs commit
        // rather than a revision name: the EIP is a live document whose parameter
        // table moved repeatedly, so "EIP-8038" alone does not identify a dataset.
        //
        // The trailing structural tags exist because the numbers alone cannot
        // distinguish a change of *basis* from no change at all — e.g. deriving
        // CREATE from the storage rather than the account cold cost produces
        // identical output whenever those two happen to coincide.
        format!(
            "name=eip-8038|model=eip8038-spec-8331fb3eed|warm={}|cold_account={}|cold_storage={}|\
             storage_write={}|account_write={}|refund_storage_clear={}|create={}|call_value={}|\
             access_list_address={}|access_list_key={}|ext_second_read={}|\
             create_base=cold_account|access_list_basis=cold_minus_warm|new_account_regular=0|\
             tx_create_repriced=true|\
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
        // reach two intrinsic slots — the per-item access-list costs and the
        // create-transaction cost — so this differs from the native intrinsic by
        // exactly those two reprices.
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
        // The intrinsic access-list and create-transaction slots are overridden in
        // `configure_evm_env`, so the EVM (vendored handler reading
        // `cfg.gas_params`) charges the repriced intrinsic itself — the runner
        // must not compensate.
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

    /// Golden values from the EIP-8038 parameters table. These literals — not the
    /// identity assertions below them, which merely restate the implementation's
    /// own derivations — are what pins this schedule to the spec.
    #[test]
    fn constants_match_eip8038_spec() {
        assert_eq!(Eip8038Constants::WARM_ACCESS.1, 100);
        assert_eq!(Eip8038Constants::COLD_ACCOUNT_ACCESS.1, 3_000);
        assert_eq!(Eip8038Constants::COLD_STORAGE_ACCESS.1, 2_100);
        assert_eq!(Eip8038Constants::STORAGE_WRITE.1, 10_000);
        assert_eq!(Eip8038Constants::ACCOUNT_WRITE.1, 9_000);
        assert_eq!(Eip8038Constants::REFUND_STORAGE_CLEAR.1, 11_616);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1, 2_900);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1, 2_000);
        assert_eq!(Eip8038Constants::CREATE, 12_000);
        assert_eq!(Eip8038Constants::CALL_VALUE, 11_300);
        assert_eq!(Eip8038Constants::EXT_SECOND_READ, 100);

        // Guards against anyone re-literalising a value the spec defines as a formula.
        assert_eq!(
            Eip8038Constants::REFUND_STORAGE_CLEAR.1,
            (Eip8038Constants::STORAGE_WRITE.1 + Eip8038Constants::COLD_STORAGE_ACCESS.1) * 4_800 /
                5_000
        );
        assert_eq!(Eip8038Constants::CALL_VALUE, 9_000 + Eip8038Constants::CALL_STIPEND);
    }

    /// The EIP derives `CREATE_ACCESS` from the **account** cold cost. While the two
    /// cold costs were both 3000 a wrong base was numerically invisible, so this test
    /// also asserts they still differ — otherwise it silently stops discriminating.
    #[test]
    fn create_access_derives_from_cold_account_not_cold_storage() {
        assert_eq!(Eip8038Constants::CREATE, 12_000);
        assert_eq!(
            Eip8038Constants::CREATE,
            Eip8038Constants::ACCOUNT_WRITE.1 + Eip8038Constants::COLD_ACCOUNT_ACCESS.1
        );
        assert_ne!(
            Eip8038Constants::COLD_ACCOUNT_ACCESS.1,
            Eip8038Constants::COLD_STORAGE_ACCESS.1,
            "the two cold costs have coincided again; this test no longer discriminates a wrong \
             CREATE base"
        );
        assert_ne!(
            Eip8038Constants::CREATE,
            Eip8038Constants::ACCOUNT_WRITE.1 + Eip8038Constants::COLD_STORAGE_ACCESS.1
        );
    }

    /// Pre-warming must be exactly cost-neutral: the intrinsic entry plus the warm
    /// access it enables totals the cold access it replaces.
    #[test]
    fn access_list_entry_plus_warm_access_equals_cold_access() {
        assert_eq!(
            Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1 + Eip8038Constants::WARM_ACCESS.1,
            Eip8038Constants::COLD_ACCOUNT_ACCESS.1
        );
        assert_eq!(
            Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1 + Eip8038Constants::WARM_ACCESS.1,
            Eip8038Constants::COLD_STORAGE_ACCESS.1
        );
    }

    #[test]
    fn schedule_metadata() {
        let s = Eip8038Schedule::new();
        assert_eq!(s.name(), "eip-8038");
        assert_eq!(s.kind(), ScheduleKind::Both);
        assert!(s.modifies_intrinsic());
        assert!(s.modifies_execution());
        // The intrinsic is charged natively via the overridden gas_params.
        assert!(s.uses_native_intrinsic_gas());
        assert!(s.affected_opcodes().contains(&0x55)); // SSTORE
        assert_eq!(s.replay_bump_multiplier(), Some(4)); // single [1,4] conditional bump
    }

    #[test]
    fn configure_evm_env_sets_absolute_slot_values() {
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
        // Cold add-ons compose back to the full cold access costs.
        assert_eq!(g(GasId::warm_storage_read_cost()) + g(GasId::cold_storage_cost()), 2_100);
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_storage_additional_cost()),
            2_100
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
        assert_eq!(g(GasId::sstore_clearing_slot_refund()), 11_616);
        // Reaching a dead account via a value-bearing CALL adds no regular gas: the
        // ACCOUNT_WRITE is already inside transfer_value_cost, and GAS_NEW_ACCOUNT is
        // EIP-8037 state gas, which this standalone schedule does not charge.
        assert_eq!(g(GasId::new_account_cost()), 0);
        // SELFDESTRUCT has no bundled charge, so it pays a standalone ACCOUNT_WRITE.
        assert_eq!(g(GasId::new_account_cost_for_selfdestruct()), 9_000);
        assert_eq!(g(GasId::transfer_value_cost()), 11_300);
        // CREATE_ACCESS lands on both the opcode and the create-transaction intrinsic.
        assert_eq!(g(GasId::create()), 12_000);
        assert_eq!(g(GasId::tx_create_cost()), 12_000);
        assert_ne!(
            g(GasId::tx_create_cost()),
            GasParams::new_spec(REF_SPEC).get(GasId::tx_create_cost()),
            "the create-transaction intrinsic must no longer sit at revm's native value"
        );
        // Intrinsic access-list slots.
        assert_eq!(g(GasId::tx_access_list_address_cost()), 2_900);
        assert_eq!(g(GasId::tx_access_list_storage_key_cost()), 2_000);
    }

    #[test]
    fn opcode_gas_delta_only_extcode_second_read() {
        let s = Eip8038Schedule::new();
        let ctx = OpcodeContext::default();
        // EXTCODESIZE / EXTCODECOPY: +WARM_ACCESS for the second DB read.
        assert_eq!(s.opcode_gas_delta(0x3B, &ctx), 100); // EXTCODESIZE
        assert_eq!(s.opcode_gas_delta(0x3C, &ctx), 100); // EXTCODECOPY
                                                         // Every other affected opcode is repriced via native gas_params → delta 0.
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
    fn intrinsic_reprices_access_list_items() {
        let schedule = Eip8038Schedule::new();
        // For a non-create tx the only intrinsic reprice is the access list.
        let native =
            initial_and_floor_gas_for(&ctx(false, 2, 3), REF_SPEC, &[]).initial_total_gas();
        let sched = schedule.intrinsic_gas(&ctx(false, 2, 3)).unwrap();
        let base = GasParams::new_spec(REF_SPEC);
        let addr_delta = Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1 -
            base.get(GasId::tx_access_list_address_cost());
        let key_delta = Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1 -
            base.get(GasId::tx_access_list_storage_key_cost());
        // Pinned literally: a self-derived delta would pass against any constants.
        assert_eq!(addr_delta, 500, "EIP-2930 address cost 2400 → 2900");
        assert_eq!(key_delta, 100, "EIP-2930 storage-key cost 1900 → 2000");
        assert_eq!(sched - native, addr_delta * 2 + key_delta * 3);
        assert_eq!(sched - native, 1_300);
    }

    #[test]
    fn intrinsic_reprices_create_transactions() {
        let schedule = Eip8038Schedule::new();
        let with_create = schedule.intrinsic_gas(&ctx(true, 0, 0)).unwrap();
        let without = schedule.intrinsic_gas(&ctx(false, 0, 0)).unwrap();
        // CREATE_ACCESS replaces the native flat create-transaction cost.
        assert_eq!(with_create - without, Eip8038Constants::CREATE);
        assert_ne!(
            with_create - without,
            GasParams::new_spec(REF_SPEC).get(GasId::tx_create_cost())
        );
    }

    /// The fingerprint is the dataset key, so every applied value must appear in it:
    /// a value that is charged but not fingerprinted would silently reuse the rows of
    /// a different parameter set.
    #[test]
    fn config_fingerprint_pins_every_proposed_value() {
        let fp = Eip8038Schedule::new().config_fingerprint();
        assert!(fp.contains("model=eip8038-spec-8331fb3eed"));
        assert!(fp.contains("warm=100"));
        assert!(fp.contains("cold_account=3000"));
        assert!(fp.contains("cold_storage=2100"));
        assert!(fp.contains("storage_write=10000"));
        assert!(fp.contains("account_write=9000"));
        assert!(fp.contains("refund_storage_clear=11616"));
        assert!(fp.contains("create=12000"));
        assert!(fp.contains("call_value=11300"));
        assert!(fp.contains("access_list_address=2900"));
        assert!(fp.contains("access_list_key=2000"));
        assert!(fp.contains("ext_second_read=100"));
        // Structural tags: the numbers alone cannot express a change of basis.
        assert!(fp.contains("create_base=cold_account"));
        assert!(fp.contains("access_list_basis=cold_minus_warm"));
        assert!(fp.contains("new_account_regular=0"));
        assert!(fp.contains("tx_create_repriced=true"));
        assert!(!fp.contains("nocode"));
        assert!(!fp.contains("multiplier"));
        // The superseded revision must not creep back into the tag.
        assert!(!fp.contains("11802"));
        assert!(!fp.contains("unified"));
    }
}
