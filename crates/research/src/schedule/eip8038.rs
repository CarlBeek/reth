//! EIP-8038: state access / write gas repricing schedule (misilva73 parameters).
//!
//! EIP-8038 reprices state-access and state-write gas from client runtime
//! benchmarks. The proposed values are **non-uniform** (one — `WARM_ACCESS` — is a
//! decrease), and 8038 changes *how* charging is done for SSTORE and account
//! writes — not just magnitudes. This schedule
//! applies it on the block's native OSAKA spec, as an **independent** experiment
//! (its own `config_fingerprint` → its own dataset; never benchmarked against
//! EIP-8037, and it does NOT pull in 8037's state-gas reservoir or 7976 floor).
//!
//! Proposed parameters (current → proposed):
//! ```text
//!   WARM_ACCESS                 100 →     62   (decrease)
//!   COLD_STORAGE_ACCESS        2100 →   2735
//!   COLD_ACCOUNT_NOCODE_ACCESS 2600 →   3140
//!   COLD_ACCOUNT_CODE_ACCESS   2600 →   9131
//!   STORAGE_WRITE              2800 →  15391
//!   ACCOUNT_WRITE              6700 →  22866
//!   REFUND_STORAGE_CLEAR       4800 →  17401
//!   TX_ACCESS_LIST_STORAGE_KEY 1900 →   2735
//!   TX_ACCESS_LIST_ADDRESS     2400 →   9131
//!   CREATE  = ACCOUNT_WRITE + COLD_STORAGE_ACCESS = 25601
//! ```
//!
//! # Charging is applied in three layers
//!
//! 1. [`configure_evm_env`](Eip8038Schedule::configure_evm_env) overrides the native gas-param
//!    slots to back-solved **absolute** values: the cold add-ons and write surcharges revm reads
//!    from `cfg.gas_params` and adds on top of the warm base, back-solved against `WARM_ACCESS`
//!    (62). revm's own cold/warm/transition branch logic is unchanged — only magnitudes move.
//!    `cold_account_additional_cost` holds the cold *add-on* (`NO_CODE − warm`), so `warm + add-on`
//!    is the `NO_CODE` total. **Caveat:** revm sources the *warm static* leg of SLOAD/BALANCE/
//!    EXTCODE*/CALL from a hardcoded const gas table (100), not `gas_params`, so the override only
//!    reaches SSTORE's warm base + the EIP-7702 second read — the warm reduction for the other
//!    opcodes is applied in Layer 3.
//! 2. [`intrinsic_gas`](Eip8038Schedule::intrinsic_gas) applies the per-item access-list /
//!    create-tx deltas (revm ignores `cfg.gas_params` for intrinsic gas).
//! 3. [`opcode_gas_delta`](Eip8038Schedule::opcode_gas_delta) (inspector-applied) adds the
//!    state-/opcode-dependent charges that no gas-param slot covers: the **warm-base correction**
//!    (62 − 100 = −38 for the native-warm-100 opcodes, since their warm static leg is hardcoded —
//!    see [`has_native_warm_static`]), the **cold-account CODE surcharge** (lift a cold access
//!    whose target has code from `NO_CODE` 3140 to CODE 9131), and the EXTCODESIZE/EXTCODECOPY
//!    **second-DB-read** flat `+WARM_ACCESS`. The inspector classifies the target (cold? has code?)
//!    read-only from the journal and supplies it on [`OpcodeContext`].
//!
//! # Cold-account CODE / `NO_CODE` rule (misilva73-confirmed)
//!
//! A cold account access costs CODE (9131) iff the target's `code_hash !=
//! KECCAK_EMPTY` — i.e. contracts **and** EIP-7702 delegated accounts. Pure EOAs,
//! empty-but-existent, and non-existent accounts are `NO_CODE` (3140). Precompiles
//! are pre-warmed, so the split never applies to them.

use super::{
    context::{OpcodeContext, TxContext},
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

/// EIP-8038 gas constants (misilva73 proposed parameters). Stored as
/// `(current, proposed)` pairs for the `config_fingerprint`, golden tests, and
/// documentation; the schedule applies the proposed column as absolute slot
/// overrides + inspector deltas (see the module docs).
#[derive(Debug, Clone, Copy)]
pub struct Eip8038Constants;

impl Eip8038Constants {
    /// Warm storage / account access (a *decrease* under 8038).
    pub const WARM_ACCESS: (u64, u64) = (100, 62);
    /// Cold storage slot access (SLOAD / cold SSTORE first touch).
    pub const COLD_STORAGE_ACCESS: (u64, u64) = (2_100, 2_735);
    /// Cold account access to a target with no code (pure EOA / empty /
    /// non-existent).
    pub const COLD_ACCOUNT_NOCODE_ACCESS: (u64, u64) = (2_600, 3_140);
    /// Cold account access to a target with code (contract / EIP-7702 delegated).
    pub const COLD_ACCOUNT_CODE_ACCESS: (u64, u64) = (2_600, 9_131);
    /// Regular gas surcharge for a first-time storage write (SSTORE set/reset).
    pub const STORAGE_WRITE: (u64, u64) = (2_800, 15_391);
    /// Regular gas for account writes (CALL/CALLCODE/CREATE/CREATE2/SELFDESTRUCT
    /// value transfer / new account).
    pub const ACCOUNT_WRITE: (u64, u64) = (6_700, 22_866);
    /// Refund for clearing a storage slot (set → zero).
    pub const REFUND_STORAGE_CLEAR: (u64, u64) = (4_800, 17_401);
    /// Intrinsic cost per access-list storage key (EIP-2930).
    pub const TX_ACCESS_LIST_STORAGE_KEY: (u64, u64) = (1_900, 2_735);
    /// Intrinsic cost per access-list address (EIP-2930).
    pub const TX_ACCESS_LIST_ADDRESS: (u64, u64) = (2_400, 9_131);

    /// CREATE / CREATE2 regular gas = `ACCOUNT_WRITE + COLD_STORAGE_ACCESS`
    /// (misilva73-confirmed; there is no standalone CREATE parameter).
    pub const CREATE: u64 = Self::ACCOUNT_WRITE.1 + Self::COLD_STORAGE_ACCESS.1; // 25_601
    /// Per-access surcharge lifting a cold account access from the `NO_CODE`
    /// baseline up to the CODE cost. Applied by the inspector on cold targets
    /// whose `code_hash != KECCAK_EMPTY`.
    pub const COLD_ACCOUNT_CODE_SURCHARGE: u64 =
        Self::COLD_ACCOUNT_CODE_ACCESS.1 - Self::COLD_ACCOUNT_NOCODE_ACCESS.1; // 5_991
}

/// Account-access opcodes the cold-account CODE/`NO_CODE` split applies to.
const fn is_account_access_opcode(opcode: u8) -> bool {
    matches!(opcode, 0x31 | 0x3B | 0x3C | 0x3F | 0xF1 | 0xF2 | 0xF4 | 0xFA | 0xFF)
}

/// revm's hardcoded warm static gas (`WARM_STORAGE_READ_COST`) for the
/// state-access opcodes. **Coupled to a revm invariant:** revm charges this warm
/// leg from a const per-spec gas table (`gas_table_spec`), NOT from
/// `cfg.gas_params` — so overriding the `warm_storage_read_cost` slot is *inert*
/// for these opcodes (only SSTORE's warm base, sourced from `sstore_static`, is
/// overridable). 8038's warm reduction (→ [`Eip8038Constants::WARM_ACCESS`]) is
/// therefore applied here in Layer 3, not Layer 1. A revm bump must re-validate
/// this constant; the `warm_sload`/`cold_sload` execution tests are the guard.
const NATIVE_WARM_STORAGE_READ_GAS: u64 = 100;

/// Opcodes whose warm static gas is the hardcoded [`NATIVE_WARM_STORAGE_READ_GAS`]
/// (so 8038's warm reduction must ride `opcode_gas_delta`, not the inert slot):
/// SLOAD plus the eight account-access reads. **Excludes** SELFDESTRUCT (0xFF),
/// whose cost is an account-write, not a warm read.
const fn has_native_warm_static(opcode: u8) -> bool {
    matches!(opcode, 0x31 | 0x3B | 0x3C | 0x3F | 0x54 | 0xF1 | 0xF2 | 0xF4 | 0xFA)
}

/// Apply EIP-8038's absolute gas-param slot overrides to `gas_params`. Shared by
/// [`Eip8038Schedule::configure_evm_env`] and the execution tests so they never
/// drift. revm adds the cold add-ons / write surcharges from these slots on top of
/// a warm base, so each is back-solved against `WARM_ACCESS` (62).
///
/// Note: the `warm_storage_read_cost` slot only governs SSTORE's warm base (via
/// `sstore_static`) and the EIP-7702 second read — revm sources the warm static
/// leg of SLOAD/BALANCE/EXTCODE*/CALL from a hardcoded table
/// ([`NATIVE_WARM_STORAGE_READ_GAS`]), so 8038's warm reduction for those reaches
/// them via [`Eip8038Schedule::opcode_gas_delta`], not this override.
/// `cold_account_additional_cost` is the cold *add-on* (`warm + add-on = NO_CODE`);
/// the CODE surcharge is applied per-access by `opcode_gas_delta`.
pub fn apply_gas_overrides(gas_params: &mut GasParams) {
    let warm = Eip8038Constants::WARM_ACCESS.1;
    let cold_storage = Eip8038Constants::COLD_STORAGE_ACCESS.1;
    let cold_account_nocode = Eip8038Constants::COLD_ACCOUNT_NOCODE_ACCESS.1;
    let storage_write = Eip8038Constants::STORAGE_WRITE.1;
    let account_write = Eip8038Constants::ACCOUNT_WRITE.1;
    let clear_refund = Eip8038Constants::REFUND_STORAGE_CLEAR.1;

    let overrides: [(GasId, u64); 14] = [
        (GasId::warm_storage_read_cost(), warm),
        (GasId::sstore_static(), warm),
        // Both cold-storage slots are the cold *add-on* over the warm base:
        // SLOAD charges `warm + cold_storage_additional`, SSTORE charges
        // `sstore_static(warm) + cold_storage_cost`, so each = warm + add-on =
        // COLD_STORAGE_ACCESS. (Verified by the SSTORE execution test.)
        (GasId::cold_storage_cost(), cold_storage - warm),
        (GasId::cold_storage_additional_cost(), cold_storage - warm),
        (GasId::cold_account_additional_cost(), cold_account_nocode - warm),
        (GasId::sstore_set_without_load_cost(), storage_write),
        (GasId::sstore_reset_without_cold_load_cost(), storage_write),
        (GasId::sstore_set_refund(), storage_write),
        (GasId::sstore_reset_refund(), storage_write),
        (GasId::sstore_clearing_slot_refund(), clear_refund),
        (GasId::new_account_cost(), account_write),
        (GasId::new_account_cost_for_selfdestruct(), account_write),
        (GasId::transfer_value_cost(), account_write),
        (GasId::create(), Eip8038Constants::CREATE),
    ];
    gas_params.override_gas(overrides);
}

/// EIP-8038 (misilva73) state access/write repricing on the block's native spec.
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
        "EIP-8038 (misilva73 proposed gas): non-uniform state access/write reprice + cold-account code/no-code split, native OSAKA, independent of EIP-8037"
    }

    fn config_fingerprint(&self) -> String {
        // Every proposed value + the algorithmic-model tag, so any renumber or
        // model change invalidates only the eip-8038 rows. `model` should carry
        // misilva73's pinned commit/run id before a long backfill.
        format!(
            "name=eip-8038|model=misilva73-absolute-v2-warm-static-L3|warm={:?}|cold_storage={:?}|\
             cold_account_nocode={:?}|cold_account_code={:?}|storage_write={:?}|account_write={:?}|\
             refund_storage_clear={:?}|access_list_key={:?}|access_list_address={:?}|create={}|\
             ext_second_read={}|native_warm_static={}|baseline=native-osaka-absolute|ref_spec={:?}",
            Eip8038Constants::WARM_ACCESS,
            Eip8038Constants::COLD_STORAGE_ACCESS,
            Eip8038Constants::COLD_ACCOUNT_NOCODE_ACCESS,
            Eip8038Constants::COLD_ACCOUNT_CODE_ACCESS,
            Eip8038Constants::STORAGE_WRITE,
            Eip8038Constants::ACCOUNT_WRITE,
            Eip8038Constants::REFUND_STORAGE_CLEAR,
            Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY,
            Eip8038Constants::TX_ACCESS_LIST_ADDRESS,
            Eip8038Constants::CREATE,
            Eip8038Constants::WARM_ACCESS.1,
            NATIVE_WARM_STORAGE_READ_GAS,
            REF_SPEC,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn replay_bump_multiplier(&self) -> Option<u64> {
        Some(4)
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        // Per-item ABSOLUTE deltas (proposed − native), non-uniform. revm's
        // initial-tx-gas helper ignores `cfg.gas_params`, so we model the
        // intrinsic change as a delta on the canonical intrinsic; block-spec
        // base/calldata costs cancel and the native per-item costs are
        // protocol-stable.
        let base = GasParams::new_spec(REF_SPEC);
        let item_delta = |proposed: u64, id: GasId| proposed as i64 - base.get(id) as i64;

        let addr_delta = item_delta(
            Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1,
            GasId::tx_access_list_address_cost(),
        );
        let key_delta = item_delta(
            Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1,
            GasId::tx_access_list_storage_key_cost(),
        );

        let mut extra: i64 = (ctx.access_list_accounts as i64).saturating_mul(addr_delta);
        extra =
            extra.saturating_add((ctx.access_list_storage_slots as i64).saturating_mul(key_delta));
        if ctx.is_create {
            // Create-transaction intrinsic repriced to the CREATE regular gas.
            // (Open item: whether the create-tx intrinsic is repriced alongside
            // the CREATE/CREATE2 opcode — validate with misilva73.)
            extra =
                extra.saturating_add(item_delta(Eip8038Constants::CREATE, GasId::tx_create_cost()));
        }

        let total = (ctx.baseline_intrinsic_gas as i64).saturating_add(extra).max(0) as u64;
        Some(total)
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        let mut delta: i64 = 0;

        // Warm-base correction (the misilva73 WARM_ACCESS 100 → 62 decrease).
        // revm charges these opcodes' warm static leg from a hardcoded const gas
        // table (NATIVE_WARM_STORAGE_READ_GAS), ignoring the `warm_storage_read_cost`
        // override, so the reduction can only be applied here as an additive delta
        // (= 62 - 100 = -38). The cold add-ons in `configure_evm_env` are
        // back-solved against the 62 warm base, so warm and cold totals both land
        // on the proposed values once this delta is applied.
        if has_native_warm_static(opcode) {
            delta += Eip8038Constants::WARM_ACCESS.1 as i64 - NATIVE_WARM_STORAGE_READ_GAS as i64;
        }

        // EXTCODESIZE / EXTCODECOPY model a second DB read: a flat extra warm
        // access on top of the normal account-access charge.
        if matches!(opcode, 0x3B | 0x3C) {
            delta += Eip8038Constants::WARM_ACCESS.1 as i64;
        }

        // Cold-account CODE surcharge: revm charged the `NO_CODE` baseline (set in
        // `configure_evm_env`); lift it to the CODE cost when the cold target has
        // code. The inspector populates the classification on `OpcodeContext`;
        // for non-account opcodes / pure EOAs the flags are false.
        if ctx.target_is_cold && ctx.target_is_code && is_account_access_opcode(opcode) {
            delta += Eip8038Constants::COLD_ACCOUNT_CODE_SURCHARGE as i64;
        }

        delta
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        let mut cfg = env.cfg_env.clone();

        apply_gas_overrides(&mut cfg.gas_params);
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

    #[test]
    fn constants_match_misilva73_proposed() {
        assert_eq!(Eip8038Constants::WARM_ACCESS.1, 62);
        assert_eq!(Eip8038Constants::COLD_STORAGE_ACCESS.1, 2_735);
        assert_eq!(Eip8038Constants::COLD_ACCOUNT_NOCODE_ACCESS.1, 3_140);
        assert_eq!(Eip8038Constants::COLD_ACCOUNT_CODE_ACCESS.1, 9_131);
        assert_eq!(Eip8038Constants::STORAGE_WRITE.1, 15_391);
        assert_eq!(Eip8038Constants::ACCOUNT_WRITE.1, 22_866);
        assert_eq!(Eip8038Constants::REFUND_STORAGE_CLEAR.1, 17_401);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1, 2_735);
        assert_eq!(Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1, 9_131);
        assert_eq!(Eip8038Constants::CREATE, 25_601);
        assert_eq!(Eip8038Constants::COLD_ACCOUNT_CODE_SURCHARGE, 5_991);
    }

    #[test]
    fn schedule_metadata() {
        let s = Eip8038Schedule::new();
        assert_eq!(s.name(), "eip-8038");
        assert_eq!(s.kind(), ScheduleKind::Both);
        assert!(s.modifies_intrinsic());
        assert!(s.modifies_execution());
        assert!(!s.uses_native_intrinsic_gas());
        assert!(s.affected_opcodes().contains(&0x55)); // SSTORE
        assert_eq!(s.replay_bump_multiplier(), Some(4)); // single [1,4] conditional bump
    }

    #[test]
    fn configure_evm_env_sets_proposed_absolute_values() {
        let schedule = Eip8038Schedule::new();
        let mut env = EvmEnv::default();
        let original_spec = env.cfg_env.spec;

        assert!(schedule.configure_evm_env(&mut env));
        // EIP-8038 stays on the native fork.
        assert_eq!(env.cfg_env.spec, original_spec);

        let g = |id| env.cfg_env.gas_params.get(id);
        // Warm base.
        assert_eq!(g(GasId::warm_storage_read_cost()), 62);
        assert_eq!(g(GasId::sstore_static()), 62);
        // Both cold-storage slots are the add-on over the warm base (warm +
        // add-on = COLD_STORAGE_ACCESS 2735); validated end-to-end by the SSTORE
        // execution test.
        assert_eq!(g(GasId::cold_storage_cost()), 2_735 - 62);
        assert_eq!(g(GasId::cold_storage_additional_cost()), 2_735 - 62);
        // Cold-account `NO_CODE` total = warm + additional.
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_account_additional_cost()),
            Eip8038Constants::COLD_ACCOUNT_NOCODE_ACCESS.1
        );
        // The previously-missing SSTORE reset/refund slots are now repriced.
        assert_eq!(g(GasId::sstore_set_without_load_cost()), 15_391);
        assert_eq!(g(GasId::sstore_reset_without_cold_load_cost()), 15_391);
        assert_eq!(g(GasId::sstore_set_refund()), 15_391);
        assert_eq!(g(GasId::sstore_reset_refund()), 15_391);
        assert_eq!(g(GasId::sstore_clearing_slot_refund()), 17_401);
        // Account write slots.
        assert_eq!(g(GasId::new_account_cost()), 22_866);
        assert_eq!(g(GasId::new_account_cost_for_selfdestruct()), 22_866);
        assert_eq!(g(GasId::transfer_value_cost()), 22_866);
        assert_eq!(g(GasId::create()), 25_601);
    }

    /// The Layer-3 delta of every native-warm-100 opcode carries the warm-base
    /// correction (62 − 100 = −38); EXTCODESIZE/EXTCODECOPY add the +62 second read
    /// on top. (Slot-level arithmetic; the execution tests in
    /// `tests/eip8038_execution.rs` validate the totals revm actually charges.)
    #[test]
    fn opcode_gas_delta_warm_base_and_second_read() {
        let s = Eip8038Schedule::new();
        let ctx = OpcodeContext::default();
        let warm_fix = Eip8038Constants::WARM_ACCESS.1 as i64 - NATIVE_WARM_STORAGE_READ_GAS as i64;
        assert_eq!(warm_fix, -38);
        assert_eq!(s.opcode_gas_delta(0x3B, &ctx), warm_fix + 62); // EXTCODESIZE + 2nd read
        assert_eq!(s.opcode_gas_delta(0x3C, &ctx), warm_fix + 62); // EXTCODECOPY + 2nd read
        assert_eq!(s.opcode_gas_delta(0x3F, &ctx), warm_fix); // EXTCODEHASH: no 2nd read
        assert_eq!(s.opcode_gas_delta(0x31, &ctx), warm_fix); // BALANCE
        assert_eq!(s.opcode_gas_delta(0x54, &ctx), warm_fix); // SLOAD
        assert_eq!(s.opcode_gas_delta(0x00, &ctx), 0); // STOP: not a state access
    }

    #[test]
    fn opcode_gas_delta_cold_account_code_surcharge() {
        let s = Eip8038Schedule::new();
        let warm_fix = Eip8038Constants::WARM_ACCESS.1 as i64 - NATIVE_WARM_STORAGE_READ_GAS as i64;
        let cold_code = OpcodeContext::default().with_target_classification(true, true);
        let cold_nocode = OpcodeContext::default().with_target_classification(true, false);
        let warm_code = OpcodeContext::default().with_target_classification(false, true);

        // Cold CALL to a contract: warm-base correction + 5991 code surcharge.
        assert_eq!(s.opcode_gas_delta(0xF1, &cold_code), warm_fix + 5_991);
        // Cold CALL to an EOA: warm-base correction only (no code surcharge).
        assert_eq!(s.opcode_gas_delta(0xF1, &cold_nocode), warm_fix);
        // Warm CALL: warm-base correction only.
        assert_eq!(s.opcode_gas_delta(0xF1, &warm_code), warm_fix);
        // Cold EXTCODECOPY to a contract: warm-base + second-read (+62) + code surcharge.
        assert_eq!(s.opcode_gas_delta(0x3C, &cold_code), warm_fix + 62 + 5_991);
        // SLOAD is storage, not an account access: warm-base correction, no surcharge.
        assert_eq!(s.opcode_gas_delta(0x54, &cold_code), warm_fix);
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
        let base = GasParams::new_spec(REF_SPEC);
        let addr_delta = Eip8038Constants::TX_ACCESS_LIST_ADDRESS.1 -
            base.get(GasId::tx_access_list_address_cost());
        let key_delta = Eip8038Constants::TX_ACCESS_LIST_STORAGE_KEY.1 -
            base.get(GasId::tx_access_list_storage_key_cost());
        let intrinsic = schedule.intrinsic_gas(&ctx(false, 2, 3)).unwrap();
        assert_eq!(intrinsic, 21_000 + addr_delta * 2 + key_delta * 3);
    }

    #[test]
    fn intrinsic_create_reprices_to_create_regular() {
        let schedule = Eip8038Schedule::new();
        let base = GasParams::new_spec(REF_SPEC);
        let with_create = schedule.intrinsic_gas(&ctx(true, 0, 0)).unwrap() as i64;
        let without = schedule.intrinsic_gas(&ctx(false, 0, 0)).unwrap() as i64;
        let create_delta =
            Eip8038Constants::CREATE as i64 - base.get(GasId::tx_create_cost()) as i64;
        assert_eq!(with_create - without, create_delta);
        assert_eq!(without, 21_000);
    }

    #[test]
    fn config_fingerprint_reflects_new_model() {
        let fp = Eip8038Schedule::new().config_fingerprint();
        // v2 carries the warm-base-correction model (the −38 Layer-3 fix), so
        // pre-fix +38 rows get a distinct config_hash.
        assert!(fp.contains("model=misilva73-absolute-v2-warm-static-L3"));
        assert!(fp.contains("native_warm_static=100"));
        assert!(fp.contains("cold_account_code=(2600, 9131)"));
        assert!(fp.contains("create=25601"));
        assert!(!fp.contains("multiplier"));
    }
}
