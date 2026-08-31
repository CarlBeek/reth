//! Glamsterdam: the full repricing stack applied as one composite schedule.
//!
//! Unlike the single-EIP schedules ([`Eip2780Schedule`](super::Eip2780Schedule),
//! [`Eip8037Schedule`](super::Eip8037Schedule),
//! [`Eip8038Schedule`](super::Eip8038Schedule)), which each isolate one
//! proposal against the block's native rules, this schedule applies the
//! **entire** Glamsterdam execution-gas repricing together in a single replay
//! lane. That is the only way to capture interaction effects: a call that only
//! runs out of gas because of the new intrinsic cost changes which cold/warm
//! accesses even happen downstream, and no combination of independent
//! single-EIP deltas can show that.
//!
//! # Source of truth
//!
//! Every constant and formula here is ported from **`ethereum/execution-specs`,
//! branch `forks/amsterdam`** (the execution-layer fork is named `amsterdam`
//! in the spec; "Glamsterdam" is the combined CL+EL name):
//!
//! - `src/ethereum/forks/amsterdam/vm/gas.py` — `GasCosts` (execution gas), `StateGasCosts`
//!   (EIP-8037 state bytes).
//! - `src/ethereum/forks/amsterdam/transactions.py` — `calculate_intrinsic_cost`, the single
//!   combined EIP-2780 + EIP-7976 + EIP-7981 + EIP-8038 intrinsic formula.
//! - `src/ethereum/forks/amsterdam/vm/eoa_delegation.py` — EIP-7702 runtime costs.
//!
//! The numbers are locked by [`tests::constants_match_amsterdam_spec`]; a devnet
//! renumber must break that test rather than silently changing results. The
//! `config_fingerprint` embeds every constant, so a renumber also re-keys the
//! dataset instead of mixing old and new rows.
//!
//! # Why this is a port, not a composition
//!
//! The current spec no longer classifies transactions into `eip2780.rs`'s eight
//! named categories — it is one additive formula — and new-account cost is now
//! entirely state gas rather than a flat execution charge. `eip2780.rs` is
//! therefore superseded, not reusable. The constants also genuinely disagree
//! with [`Eip8038Constants`](super::eip8038::Eip8038Constants), which tracks
//! EIP-8038 PR 11802 rather than the merged amsterdam fork:
//!
//! | | amsterdam fork (here) | PR 11802 (`eip8038.rs`) |
//! |---|---|---|
//! | `COLD_STORAGE_ACCESS` | 2,100 | 3,000 |
//! | `ACCOUNT_WRITE` | 9,000 | 8,000 |
//! | `REFUND_STORAGE_CLEAR` | 11,616 | 12,480 |
//! | `CREATE_ACCESS` | 12,000 (`+COLD_ACCOUNT_ACCESS`) | 11,000 (`+COLD_STORAGE_ACCESS`) |
//! | `CALL_VALUE` | 11,300 | 10,300 |
//! | `TX_ACCESS_LIST_ADDRESS` | 2,900 | 3,000 |
//! | `TX_ACCESS_LIST_STORAGE_KEY` | 2,000 | 3,000 |
//!
//! Seven of the twelve shared constants differ. Only `WARM_ACCESS`,
//! `COLD_ACCOUNT_ACCESS`, `STORAGE_WRITE`, `CALL_STIPEND` and the EXT\*
//! second-read surcharge agree. So this module carries its own constant table
//! and must never reuse `Eip8038Constants`.
//!
//! # Charging
//!
//! Everything is native: `configure_evm_env` switches to [`SpecId::AMSTERDAM`]
//! (for EIP-8037's state-gas reservoir machinery) and overlays the spec-correct
//! values on revm's gas-param table, whose baked-in AMSTERDAM numbers are
//! several devnet iterations stale. The one exception is the
//! `EXTCODESIZE`/`EXTCODECOPY` second-database-read surcharge, which has no
//! gas-param slot and rides the inspector — same as `eip8038.rs`.
//!
//! Because the intrinsic is charged by the EVM itself,
//! [`uses_native_intrinsic_gas`](GlamsterdamSchedule::uses_native_intrinsic_gas)
//! is `true` and the runner applies no post-hoc compensation. That is what makes
//! the replay's execution path faithful rather than merely its reported total.
//!
//! # Known deviation from the spec: EIP-7702 charge timing
//!
//! The spec charges an authorization's state-dependent costs at the **top
//! frame** in `set_delegation`, and carries *no* intrinsic state gas at all —
//! `IntrinsicGasCost` has only `execution` and `calldata_floor` fields, and
//! `allocate_evm_gas` budgets from `intrinsic.execution` alone.
//!
//! revm instead reserves `n × (NEW_ACCOUNT + AUTH_BASE)` state gas in
//! `initial_tx_gas` and credits back, in `apply_eip7702_auth_list`, whatever
//! turned out to be unnecessary. The net charge is arithmetically identical to
//! the spec's:
//!
//! ```text
//! n×(NEW_ACCOUNT + AUTH_BASE) − (existing×NEW_ACCOUNT + deployed×AUTH_BASE)
//!   = (n−existing)×NEW_ACCOUNT + (n−deployed)×AUTH_BASE
//! ```
//!
//! and the credit lands in `pre_execution`, before the frame runs, so mid-tx
//! headroom is right too. We follow revm's structure — as `eip8037.rs` already
//! does — because the alternative is structural: revm applies the authorization
//! list in `pre_execution`, before a frame or gas meter exists, where the only
//! error channel is a transaction *rejection* rather than an OOG halt. And
//! `initial_gas_and_reservoir` subtracts `initial_state_gas` with raw
//! arithmetic, relying on validation having already proved it affordable, so a
//! post-validation increase would underflow rather than halt.
//!
//! **The one materially observable consequence** is that validation runs before
//! the credit: `initial_total_gas()` uses the full pessimistic reservation, so a
//! 7702 transaction whose gas limit covers the spec's requirement but not
//! ~218,790 gas per authorization on top is *rejected* where the spec would run
//! it. Rejections carry no execution trace, so they read differently downstream
//! from a genuine OOG. Recorded in the fingerprint as `auth_state_gas=intrinsic`.

use super::{
    common::initial_and_floor_gas_for,
    context::{OpcodeContext, TxContext},
    traits::{GasSchedule, GasTaxBreakdown, ScheduleKind},
};
use crate::opcode;
use reth_evm::EvmEnv;
use revm::{
    context_interface::{
        cfg::{gas::InitialAndFloorGas, gas_params::GasId},
        Cfg,
    },
    primitives::hardfork::SpecId,
};

/// Glamsterdam execution- and state-gas constants, ported from
/// `ethereum/execution-specs` branch `forks/amsterdam`.
///
/// Values that the spec *derives* are derived here too, so a change to a base
/// constant propagates exactly as it does in the spec.
#[derive(Debug, Clone, Copy)]
pub struct GlamsterdamConstants;

impl GlamsterdamConstants {
    // ── Access (`GasCosts`) ─────────────────────────────────────────────

    /// Warm account / storage access. Unchanged from Berlin.
    pub const WARM_ACCESS: u64 = 100;
    /// Cold account access. Raised from Berlin's 2,600.
    pub const COLD_ACCOUNT_ACCESS: u64 = 3_000;
    /// Cold storage-slot access. Unchanged from Berlin.
    pub const COLD_STORAGE_ACCESS: u64 = 2_100;

    // ── Storage / account writes ────────────────────────────────────────

    /// Flat SSTORE write charge, decoupled from the cold/warm SLOAD derivation
    /// that produced the old 2,900.
    pub const STORAGE_WRITE: u64 = 10_000;
    /// Account-write charge (SELFDESTRUCT to a new account, EIP-7702
    /// delegation write).
    pub const ACCOUNT_WRITE: u64 = 9_000;
    /// `CALL_STIPEND`, unchanged.
    pub const CALL_STIPEND: u64 = 2_300;
    /// Value-bearing CALL charge. The spec states this as a literal 11,300 and
    /// documents it as `ACCOUNT_WRITE + CALL_STIPEND`; asserted both ways.
    pub const CALL_VALUE: u64 = 11_300;

    // ── Contract creation ───────────────────────────────────────────────

    /// **Derived**: `ACCOUNT_WRITE + COLD_ACCOUNT_ACCESS`. Note the second term
    /// is the *account* access, not the storage access that EIP-8038 PR 11802
    /// used.
    pub const CREATE_ACCESS: u64 = Self::ACCOUNT_WRITE + Self::COLD_ACCOUNT_ACCESS;

    // ── Refunds ─────────────────────────────────────────────────────────

    /// **Derived**: `(STORAGE_WRITE + COLD_STORAGE_ACCESS) × 4800 / 5000`,
    /// matching the spec's integer floor division.
    pub const REFUND_STORAGE_CLEAR: u64 =
        (Self::STORAGE_WRITE + Self::COLD_STORAGE_ACCESS) * 4_800 / 5_000;

    // ── Transaction intrinsic ───────────────────────────────────────────

    /// Sender-side base cost, replacing the flat 21,000.
    pub const TX_BASE: u64 = 12_000;
    /// Charged on the recipient when `value > 0` and the tx is not a
    /// self-transfer.
    pub const TX_VALUE_COST: u64 = 6_000;
    /// Standard price per calldata token.
    pub const TX_DATA_TOKEN_STANDARD: u64 = 4;
    /// EIP-7976 floor price per token, raised from EIP-7623's 10.
    pub const TX_DATA_TOKEN_FLOOR: u64 = 16;
    /// **Derived**: `COLD_ACCOUNT_ACCESS − WARM_ACCESS`.
    pub const TX_ACCESS_LIST_ADDRESS: u64 = Self::COLD_ACCOUNT_ACCESS - Self::WARM_ACCESS;
    /// **Derived**: `COLD_STORAGE_ACCESS − WARM_ACCESS`.
    pub const TX_ACCESS_LIST_STORAGE_KEY: u64 = Self::COLD_STORAGE_ACCESS - Self::WARM_ACCESS;

    // ── EIP-7981 access-list floor tokens ───────────────────────────────

    /// Floor tokens contributed per access-list address (20 bytes × 4).
    pub const ACCESS_LIST_ADDRESS_FLOOR_TOKENS: u64 = 80;
    /// Floor tokens contributed per access-list storage key (32 bytes × 4).
    pub const ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS: u64 = 128;
    /// Floor tokens per byte of access-list data — the multiplier revm applies
    /// to the 20-/32-byte item widths to reach the two token counts above.
    pub const ACCESS_LIST_FLOOR_BYTE_MULTIPLIER: u64 = 4;

    // ── Authorization (EIP-7702) ────────────────────────────────────────

    /// Serialized bytes of one authorization tuple.
    pub const AUTH_TUPLE_BYTES: u64 = 101;
    /// `ecrecover` precompile cost, an input to the auth base cost.
    pub const PRECOMPILE_ECRECOVER: u64 = 3_000;
    /// **Derived**: `AUTH_TUPLE_BYTES × TX_DATA_TOKEN_FLOOR + ecrecover +
    /// COLD_ACCOUNT_ACCESS + 2 × WARM_ACCESS` = 7,816.
    ///
    /// This is only the *state-independent* part, which the spec pays in the
    /// intrinsic. `set_delegation` charges [`Self::ACCOUNT_WRITE`] on top at the
    /// top frame — see [`AUTH_REGULAR_PER_AUTH`](Self::AUTH_REGULAR_PER_AUTH).
    pub const EXECUTION_PER_AUTH_BASE_COST: u64 = Self::AUTH_TUPLE_BYTES *
        Self::TX_DATA_TOKEN_FLOOR +
        Self::PRECOMPILE_ECRECOVER +
        Self::COLD_ACCOUNT_ACCESS +
        2 * Self::WARM_ACCESS;

    /// Total **execution** gas charged per authorization:
    /// `EXECUTION_PER_AUTH_BASE_COST + ACCOUNT_WRITE` = 16,816.
    ///
    /// `eoa_delegation.py::set_delegation` charges `GasCosts.ACCOUNT_WRITE`
    /// (execution gas) for the delegation write, on top of the intrinsic
    /// `EXECUTION_PER_AUTH_BASE_COST`. revm charges no equivalent anywhere in
    /// its EIP-7702 path — `apply_auth_list` only counts refunds and writes the
    /// designator — so without this the schedule undercharges every
    /// authorization by 9,000 execution gas.
    ///
    /// We fold it into the intrinsic slot rather than charging it at the top
    /// frame, because revm applies the authorization list in `pre_execution`,
    /// before a frame or its gas meter exists, and the only error channel there
    /// is a transaction *rejection* rather than an OOG halt. Pre-validating the
    /// charge keeps the downstream gas arithmetic underflow-free.
    ///
    /// The spec exempts three cases this cannot see, so it over-charges 9,000
    /// in each: an authorization on the sender's own account (already covered by
    /// `TX_BASE`), on the recipient of a value-bearing transaction (covered by
    /// `TX_VALUE_COST`), and the second and later authorizations on one
    /// authority (the spec charges once per authority, not per tuple). All three
    /// are rare next to the dominant shape — a sponsor delegating an account
    /// that is neither the sender nor the recipient, once — where this is exact.
    pub const AUTH_REGULAR_PER_AUTH: u64 = Self::EXECUTION_PER_AUTH_BASE_COST + Self::ACCOUNT_WRITE;

    // ── EIP-8037 state gas (`StateGasCosts`) ────────────────────────────

    /// Cost per state byte.
    pub const CPSB: u64 = 1_530;
    /// State bytes per new account.
    pub const STATE_BYTES_PER_NEW_ACCOUNT: u64 = 120;
    /// State bytes per new storage slot.
    pub const STATE_BYTES_PER_STORAGE_SET: u64 = 64;
    /// State bytes per EIP-7702 authorization base (the delegation designator).
    pub const STATE_BYTES_PER_AUTH_BASE: u64 = 23;

    /// **Derived** state gas for creating one account.
    pub const NEW_ACCOUNT_STATE_GAS: u64 = Self::STATE_BYTES_PER_NEW_ACCOUNT * Self::CPSB;
    /// **Derived** state gas for one new storage slot.
    pub const STORAGE_SET_STATE_GAS: u64 = Self::STATE_BYTES_PER_STORAGE_SET * Self::CPSB;
    /// **Derived** combined state gas for one authorization (account + designator).
    pub const AUTH_STATE_GAS: u64 =
        (Self::STATE_BYTES_PER_NEW_ACCOUNT + Self::STATE_BYTES_PER_AUTH_BASE) * Self::CPSB;

    // ── EXT* family ─────────────────────────────────────────────────────

    /// `EXTCODESIZE`/`EXTCODECOPY` second-database-read surcharge
    /// (`WARM_ACCESS`), charged on top of the normal account access —
    /// `vm/instructions/environment.py`'s `# Code reading cost (EIP-8038)`.
    /// `EXTCODEHASH` is deliberately excluded: it reads only the hash.
    pub const EXT_SECOND_READ: u64 = Self::WARM_ACCESS;
}

/// The composite Glamsterdam repricing schedule.
#[derive(Debug, Clone, Copy, Default)]
pub struct GlamsterdamSchedule;

impl GlamsterdamSchedule {
    /// Create a new Glamsterdam schedule.
    pub const fn new() -> Self {
        Self
    }
}

impl GasSchedule for GlamsterdamSchedule {
    fn name(&self) -> &str {
        "glamsterdam-v1"
    }

    fn description(&self) -> &str {
        "Glamsterdam composite repricing (execution-specs forks/amsterdam): combined EIP-2780 + 7976 + 7981 + 8037 + 8038 in one replay lane"
    }

    fn config_fingerprint(&self) -> String {
        // Every ported constant, so a devnet renumber re-keys the dataset
        // instead of mixing rows priced under different rules. `spec=` names
        // the source branch; `auth_state_gas=intrinsic` records the documented
        // deviation from `set_delegation`'s top-frame charge.
        format!(
            "name=glamsterdam-v1|spec=execution-specs/forks/amsterdam|\
             warm={}|cold_account={}|cold_storage={}|storage_write={}|account_write={}|\
             call_value={}|create_access={}|refund_storage_clear={}|\
             tx_base={}|tx_value={}|token_standard={}|token_floor={}|\
             al_address={}|al_key={}|al_addr_tokens={}|al_key_tokens={}|\
             auth_base={}|auth_regular={}|cpsb={}|account_bytes={}|storage_bytes={}|auth_bytes={}|\
             ext_second_read={}|auth_state_gas=intrinsic|native_revm=true",
            GlamsterdamConstants::WARM_ACCESS,
            GlamsterdamConstants::COLD_ACCOUNT_ACCESS,
            GlamsterdamConstants::COLD_STORAGE_ACCESS,
            GlamsterdamConstants::STORAGE_WRITE,
            GlamsterdamConstants::ACCOUNT_WRITE,
            GlamsterdamConstants::CALL_VALUE,
            GlamsterdamConstants::CREATE_ACCESS,
            GlamsterdamConstants::REFUND_STORAGE_CLEAR,
            GlamsterdamConstants::TX_BASE,
            GlamsterdamConstants::TX_VALUE_COST,
            GlamsterdamConstants::TX_DATA_TOKEN_STANDARD,
            GlamsterdamConstants::TX_DATA_TOKEN_FLOOR,
            GlamsterdamConstants::TX_ACCESS_LIST_ADDRESS,
            GlamsterdamConstants::TX_ACCESS_LIST_STORAGE_KEY,
            GlamsterdamConstants::ACCESS_LIST_ADDRESS_FLOOR_TOKENS,
            GlamsterdamConstants::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS,
            GlamsterdamConstants::EXECUTION_PER_AUTH_BASE_COST,
            GlamsterdamConstants::AUTH_REGULAR_PER_AUTH,
            GlamsterdamConstants::CPSB,
            GlamsterdamConstants::STATE_BYTES_PER_NEW_ACCOUNT,
            GlamsterdamConstants::STATE_BYTES_PER_STORAGE_SET,
            GlamsterdamConstants::STATE_BYTES_PER_AUTH_BASE,
            GlamsterdamConstants::EXT_SECOND_READ,
        )
    }

    fn kind(&self) -> ScheduleKind {
        ScheduleKind::Both
    }

    fn replay_bump_multiplier(&self) -> Option<u64> {
        // Matches EIP-8037's tier: the state-gas reservoir dominates the
        // headroom question, and 10x is what that lane already uses, so the two
        // datasets stay comparable.
        Some(10)
    }

    fn intrinsic_gas(&self, ctx: &TxContext) -> Option<u64> {
        Some(self.initial_and_floor_gas(ctx)?.initial_total_gas())
    }

    fn initial_and_floor_gas(&self, ctx: &TxContext) -> Option<InitialAndFloorGas> {
        Some(initial_and_floor_gas_for(ctx, SpecId::AMSTERDAM, &glamsterdam_overrides()))
    }

    fn opcode_gas_delta(&self, opcode: u8, ctx: &OpcodeContext) -> i64 {
        self.opcode_gas_tax_breakdown(opcode, ctx).total()
    }

    fn opcode_gas_tax_breakdown(&self, opcode: u8, _ctx: &OpcodeContext) -> GasTaxBreakdown {
        let mut b = GasTaxBreakdown::default();
        // The only charge with no gas-param slot. Unconditional — the spec adds
        // it regardless of whether the account access was cold or warm.
        if matches!(opcode, opcode::EXTCODESIZE | opcode::EXTCODECOPY) {
            b.second_db_read += GlamsterdamConstants::EXT_SECOND_READ as i64;
        }
        b
    }

    fn configure_evm_env(&self, env: &mut EvmEnv<SpecId>) -> bool {
        // Preserve the block's code-size limits across the spec switch — the
        // Amsterdam defaults would otherwise silently replace them.
        let baseline_max_code_size = env.cfg_env.max_code_size();
        let baseline_max_initcode_size = env.cfg_env.max_initcode_size();

        let mut cfg = env.cfg_env.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
        cfg.limit_contract_code_size = Some(baseline_max_code_size);
        cfg.limit_contract_initcode_size = Some(baseline_max_initcode_size);
        // EIP-7708 (ETH-transfer logs) is a separate proposal; keeping it off
        // isolates this lane to the gas repricing, and matches `eip8037.rs` so
        // the two datasets differ only in the gas table.
        cfg.amsterdam_eip7708_disabled = true;
        cfg.amsterdam_eip7708_delayed_burn_disabled = true;

        cfg.gas_params.override_gas(glamsterdam_overrides());

        env.cfg_env = cfg;
        true
    }

    fn uses_native_intrinsic_gas(&self) -> bool {
        // The overridden table drives the vendored handler's
        // `validate_initial_tx_gas`, so the EVM charges the Glamsterdam
        // intrinsic itself. The runner must not compensate — and because the
        // EVM executes on the correct budget, the OOG points and cold/warm
        // access pattern are faithful, which is the whole point of running the
        // stack as one composite lane.
        true
    }

    fn tx_category(&self, ctx: &TxContext) -> Option<String> {
        // Mirrors the spec's own intrinsic branching so the category explains
        // which recipient-cost arm a tx took.
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

/// The spec-correct absolute `gas_params` slot values, overlaid on revm's
/// `SpecId::AMSTERDAM` table.
///
/// Shared by `configure_evm_env` (runtime charging) and the intrinsic path
/// ([`initial_and_floor_gas_for`]) so the two can never drift.
///
/// # Why revm's own AMSTERDAM table isn't enough
///
/// It is several devnet iterations stale: `create()` and `tx_create_cost()` sit
/// at 9,000 (the `ACCOUNT_WRITE` half of `CREATE_ACCESS`),
/// `sstore_set_without_load_cost()` at 2,800 (the old cold/warm-SLOAD
/// derivation rather than the flat `STORAGE_WRITE`), cold account access was
/// never bumped off Berlin's 2,600, and `tx_base_stipend()` is still the flat
/// 21,000.
///
/// # Add-on vs absolute slots
///
/// revm reaches a cold cost by adding an add-on to the warm base, so the cold
/// slots carry `total − WARM_ACCESS`, not the total. Writing the total would
/// overcharge by `WARM_ACCESS` on every cold access. The write-surcharge and
/// per-item intrinsic slots are absolute.
const fn glamsterdam_overrides() -> [(GasId, u64); 25] {
    use GlamsterdamConstants as C;

    let cold_account_addon = C::COLD_ACCOUNT_ACCESS - C::WARM_ACCESS; // 2_900
    let cold_storage_addon = C::COLD_STORAGE_ACCESS - C::WARM_ACCESS; // 2_000

    // The spec charges `TX_ACCESS_LIST_*` for the item AND `floor_tokens ×
    // TX_DATA_TOKEN_FLOOR` for its bytes, both inside `access_list_cost`, which
    // feeds the *execution* intrinsic; the same tokens also feed the floor.
    // revm's per-item slot covers only the execution side, so the data charge
    // is baked in here and the floor picks the tokens up separately via
    // `tx_access_list_floor_byte_multiplier`.
    let al_address =
        C::TX_ACCESS_LIST_ADDRESS + C::ACCESS_LIST_ADDRESS_FLOOR_TOKENS * C::TX_DATA_TOKEN_FLOOR; // 4_180
    let al_storage_key = C::TX_ACCESS_LIST_STORAGE_KEY +
        C::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS * C::TX_DATA_TOKEN_FLOOR; // 4_048

    [
        // ── Transaction intrinsic ──────────────────────────────────────
        (GasId::tx_base_stipend(), C::TX_BASE),
        // The two slots added for this port: the spec's decomposed
        // recipient-side terms, which have no pre-Glamsterdam analogue.
        (GasId::tx_recipient_access_cost(), C::COLD_ACCOUNT_ACCESS),
        (GasId::tx_value_cost(), C::TX_VALUE_COST),
        (GasId::tx_create_cost(), C::CREATE_ACCESS),
        (GasId::tx_token_cost(), C::TX_DATA_TOKEN_STANDARD),
        (GasId::tx_access_list_address_cost(), al_address),
        (GasId::tx_access_list_storage_key_cost(), al_storage_key),
        // ── Calldata floor (EIP-7976 / EIP-7981) ───────────────────────
        // Anchored on TX_BASE; `initial_tx_gas` adds recipient_execution_gas
        // on top, per the spec's `base_execution_gas` floor anchor.
        (GasId::tx_floor_cost_base_gas(), C::TX_BASE),
        (GasId::tx_floor_cost_per_token(), C::TX_DATA_TOKEN_FLOOR),
        // Uniform floor tokens across zero and non-zero calldata bytes.
        (GasId::tx_floor_token_zero_byte_multiplier(), C::TX_DATA_TOKEN_STANDARD),
        (GasId::tx_access_list_floor_byte_multiplier(), C::ACCESS_LIST_FLOOR_BYTE_MULTIPLIER),
        // ── EIP-7702 authorizations ────────────────────────────────────
        // The slot holds the REGULAR portion only. `GasParams`'
        // `tx_eip7702_per_empty_account_cost()` accessor composes the state-gas
        // portion on top (from `new_account_state_gas` +
        // `tx_eip7702_state_gas_bytecode`), and `initial_tx_gas` then splits the
        // two back apart. Writing the combined figure here would double-count
        // the state gas. See the module docs on the intrinsic-vs-top-frame
        // deviation.
        //
        // The regular portion is `EXECUTION_PER_AUTH_BASE_COST + ACCOUNT_WRITE`,
        // not the base cost alone: revm charges no `ACCOUNT_WRITE` anywhere in
        // its EIP-7702 path, so the base cost by itself undercharges every
        // authorization by 9,000 execution gas. See
        // [`GlamsterdamConstants::AUTH_REGULAR_PER_AUTH`].
        (GasId::tx_eip7702_per_empty_account_cost(), C::AUTH_REGULAR_PER_AUTH),
        // ── Access (runtime) ───────────────────────────────────────────
        (GasId::warm_storage_read_cost(), C::WARM_ACCESS),
        (GasId::sstore_static(), C::WARM_ACCESS),
        (GasId::cold_account_additional_cost(), cold_account_addon),
        (GasId::cold_storage_cost(), cold_storage_addon),
        (GasId::cold_storage_additional_cost(), cold_storage_addon),
        // ── Storage writes and refunds ─────────────────────────────────
        (GasId::sstore_set_without_load_cost(), C::STORAGE_WRITE),
        (GasId::sstore_reset_without_cold_load_cost(), C::STORAGE_WRITE),
        (GasId::sstore_set_refund(), C::STORAGE_WRITE),
        (GasId::sstore_reset_refund(), C::STORAGE_WRITE),
        (GasId::sstore_clearing_slot_refund(), C::REFUND_STORAGE_CLEAR),
        // ── Calls, creation, selfdestruct ──────────────────────────────
        // `CALL` with value: CALL_VALUE = ACCOUNT_WRITE + CALL_STIPEND.
        (GasId::transfer_value_cost(), C::CALL_VALUE),
        // CREATE/CREATE2 opcode regular gas.
        (GasId::create(), C::CREATE_ACCESS),
        // SELFDESTRUCT to a non-existent beneficiary charges ACCOUNT_WRITE as
        // execution gas (`vm/instructions/system.py`). A plain CALL creating an
        // account does NOT — that cost is entirely state gas — so
        // `new_account_cost` is left at revm's Amsterdam zero.
        (GasId::new_account_cost_for_selfdestruct(), C::ACCOUNT_WRITE),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::common::gas_params_for;
    use alloy_primitives::{Address, Bytes, U256};
    use revm::context_interface::cfg::gas_params::GasParams;

    /// Locks every ported number against `ethereum/execution-specs`
    /// `forks/amsterdam` (`vm/gas.py`, `transactions.py`). A devnet renumber
    /// must break this test rather than silently changing replay results.
    #[test]
    fn constants_match_amsterdam_spec() {
        use GlamsterdamConstants as C;

        // GasCosts — access
        assert_eq!(C::WARM_ACCESS, 100);
        assert_eq!(C::COLD_ACCOUNT_ACCESS, 3_000);
        assert_eq!(C::COLD_STORAGE_ACCESS, 2_100);
        // GasCosts — storage / call / creation
        assert_eq!(C::STORAGE_WRITE, 10_000);
        assert_eq!(C::ACCOUNT_WRITE, 9_000);
        assert_eq!(C::CALL_STIPEND, 2_300);
        assert_eq!(C::CALL_VALUE, 11_300);
        // The spec states CALL_VALUE literally and documents the derivation;
        // assert the derivation holds so a change to either is caught.
        assert_eq!(C::CALL_VALUE, C::ACCOUNT_WRITE + C::CALL_STIPEND);
        assert_eq!(C::CREATE_ACCESS, 12_000);
        assert_eq!(C::CREATE_ACCESS, C::ACCOUNT_WRITE + C::COLD_ACCOUNT_ACCESS);
        // GasCosts — refunds
        assert_eq!(C::REFUND_STORAGE_CLEAR, 11_616);
        // GasCosts — transaction intrinsic
        assert_eq!(C::TX_BASE, 12_000);
        assert_eq!(C::TX_VALUE_COST, 6_000);
        assert_eq!(C::TX_DATA_TOKEN_STANDARD, 4);
        assert_eq!(C::TX_DATA_TOKEN_FLOOR, 16);
        assert_eq!(C::TX_ACCESS_LIST_ADDRESS, 2_900);
        assert_eq!(C::TX_ACCESS_LIST_STORAGE_KEY, 2_000);
        // transactions.py — EIP-7981 floor tokens
        assert_eq!(C::ACCESS_LIST_ADDRESS_FLOOR_TOKENS, 80);
        assert_eq!(C::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS, 128);
        // The 80/128 token counts are the 20-/32-byte item widths times the
        // per-byte multiplier revm uses to reach them.
        assert_eq!(C::ACCESS_LIST_ADDRESS_FLOOR_TOKENS, 20 * C::ACCESS_LIST_FLOOR_BYTE_MULTIPLIER);
        assert_eq!(
            C::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS,
            32 * C::ACCESS_LIST_FLOOR_BYTE_MULTIPLIER
        );
        // GasCosts — authorization
        assert_eq!(C::AUTH_TUPLE_BYTES, 101);
        assert_eq!(C::EXECUTION_PER_AUTH_BASE_COST, 7_816);
        // StateGasCosts
        assert_eq!(C::CPSB, 1_530);
        assert_eq!(C::STATE_BYTES_PER_NEW_ACCOUNT, 120);
        assert_eq!(C::STATE_BYTES_PER_STORAGE_SET, 64);
        assert_eq!(C::STATE_BYTES_PER_AUTH_BASE, 23);
        assert_eq!(C::NEW_ACCOUNT_STATE_GAS, 120 * 1_530);
        assert_eq!(C::STORAGE_SET_STATE_GAS, 64 * 1_530);
        assert_eq!(C::AUTH_STATE_GAS, 143 * 1_530);
        // EXT* second read
        assert_eq!(C::EXT_SECOND_READ, 100);
    }

    /// The amsterdam-fork numbers genuinely disagree with EIP-8038 PR 11802,
    /// which `eip8038.rs` tracks. If these ever converge, the two schedules can
    /// share a constant table — until then, reusing `Eip8038Constants` here
    /// would be a silent mispricing.
    #[test]
    fn constants_deliberately_differ_from_eip8038_pr11802() {
        use crate::schedule::eip8038::Eip8038Constants as P;
        use GlamsterdamConstants as C;

        // All seven divergences, so a devnet renumber that converges ANY of
        // them trips this test and prompts a re-check rather than leaving a
        // stale duplicate table in place.
        assert_ne!(C::COLD_STORAGE_ACCESS, P::COLD_STORAGE_ACCESS.1); // 2100 vs 3000
        assert_ne!(C::ACCOUNT_WRITE, P::ACCOUNT_WRITE.1); // 9000 vs 8000
        assert_ne!(C::REFUND_STORAGE_CLEAR, P::REFUND_STORAGE_CLEAR.1); // 11616 vs 12480
        assert_ne!(C::CREATE_ACCESS, P::CREATE); // 12000 vs 11000
        assert_ne!(C::CALL_VALUE, P::CALL_VALUE); // 11300 vs 10300
        assert_ne!(C::TX_ACCESS_LIST_ADDRESS, P::TX_ACCESS_LIST_ADDRESS.1); // 2900 vs 3000
        assert_ne!(C::TX_ACCESS_LIST_STORAGE_KEY, P::TX_ACCESS_LIST_STORAGE_KEY.1); // 2000 vs 3000

        // The two also DERIVE their composites differently: the spec builds
        // CREATE_ACCESS from the account access, PR 11802 from the storage
        // access. Same shape, different second term.
        assert_eq!(C::CREATE_ACCESS, C::ACCOUNT_WRITE + C::COLD_ACCOUNT_ACCESS);
        assert_eq!(P::CREATE, P::ACCOUNT_WRITE.1 + P::COLD_STORAGE_ACCESS.1);

        // Where they agree, they agree — so the divergence is specific, not
        // wholesale, and the five agreements are load-bearing context for
        // anyone comparing the two datasets.
        assert_eq!(C::WARM_ACCESS, P::WARM_ACCESS.1);
        assert_eq!(C::COLD_ACCOUNT_ACCESS, P::COLD_ACCOUNT_ACCESS.1);
        assert_eq!(C::STORAGE_WRITE, P::STORAGE_WRITE.1);
        assert_eq!(C::CALL_STIPEND, P::CALL_STIPEND);
        assert_eq!(C::EXT_SECOND_READ, P::EXT_SECOND_READ);
    }

    fn ctx() -> TxContext {
        TxContext {
            baseline_intrinsic_gas: 21_000,
            sender: Address::repeat_byte(0x01),
            recipient: Some(Address::repeat_byte(0x02)),
            value: U256::ZERO,
            input: Bytes::new(),
            gas_limit: 1_000_000,
            is_create: false,
            recipient_info: None,
            access_list_accounts: 0,
            access_list_storage_slots: 0,
            authorization_count: 0,
        }
    }

    /// Hand-computes `calculate_intrinsic_cost` from the spec formula for each
    /// recipient arm and checks the schedule reproduces it exactly.
    #[test]
    fn intrinsic_matches_spec_formula_per_recipient_arm() {
        use GlamsterdamConstants as C;
        let s = GlamsterdamSchedule::new();

        // Plain call, no value, no calldata:
        //   TX_BASE + COLD_ACCOUNT_ACCESS
        assert_eq!(s.intrinsic_gas(&ctx()).unwrap(), C::TX_BASE + C::COLD_ACCOUNT_ACCESS);

        // Value-bearing call: + TX_VALUE_COST.
        let mut c = ctx();
        c.value = U256::from(1);
        assert_eq!(
            s.intrinsic_gas(&c).unwrap(),
            C::TX_BASE + C::COLD_ACCOUNT_ACCESS + C::TX_VALUE_COST
        );

        // Self-transfer skips BOTH the recipient access and the value cost,
        // even with value > 0 — the spec's `is_self_transfer` branch.
        let mut c = ctx();
        c.value = U256::from(1);
        c.recipient = Some(c.sender);
        assert!(c.is_self_transfer());
        assert_eq!(s.intrinsic_gas(&c).unwrap(), C::TX_BASE);

        // Contract creation pays CREATE_ACCESS instead of the recipient access,
        // and no value cost on top. State gas is the new-account charge.
        let mut c = ctx();
        c.is_create = true;
        c.recipient = None;
        c.value = U256::from(1);
        let split = s.initial_and_floor_gas(&c).unwrap();
        assert_eq!(split.initial_regular_gas(), C::TX_BASE + C::CREATE_ACCESS);
        assert_eq!(split.initial_state_gas, C::NEW_ACCOUNT_STATE_GAS);
    }

    #[test]
    fn intrinsic_charges_calldata_tokens_at_standard_price() {
        use GlamsterdamConstants as C;
        let s = GlamsterdamSchedule::new();
        let mut c = ctx();
        c.input = Bytes::from(vec![0, 0, 0, 1, 2, 3]);
        // `count_tokens_in_data`: a zero byte is 1 token, a non-zero byte is 4.
        let (zero_bytes, nonzero_bytes) = (3u64, 3u64);
        let tokens = zero_bytes + nonzero_bytes * 4;
        assert_eq!(tokens, 15);
        assert_eq!(
            s.intrinsic_gas(&c).unwrap(),
            C::TX_BASE + C::COLD_ACCOUNT_ACCESS + tokens * C::TX_DATA_TOKEN_STANDARD
        );
    }

    /// EIP-7976: the floor counts every calldata byte uniformly at
    /// `TX_DATA_TOKEN_STANDARD` tokens, priced at `TX_DATA_TOKEN_FLOOR`, and
    /// anchors on `TX_BASE + recipient_execution_gas`.
    #[test]
    fn floor_matches_spec_formula() {
        use GlamsterdamConstants as C;
        let s = GlamsterdamSchedule::new();
        let mut c = ctx();
        c.input = Bytes::from(vec![0, 0, 0, 1, 2, 3]);

        let floor_tokens = 6 * C::TX_DATA_TOKEN_STANDARD;
        let expected = floor_tokens * C::TX_DATA_TOKEN_FLOOR + C::TX_BASE + C::COLD_ACCOUNT_ACCESS;
        assert_eq!(s.initial_and_floor_gas(&c).unwrap().floor_gas(), expected);

        // The anchor tracks the recipient arm: a self-transfer's floor drops by
        // exactly COLD_ACCOUNT_ACCESS.
        c.recipient = Some(c.sender);
        assert_eq!(
            s.initial_and_floor_gas(&c).unwrap().floor_gas(),
            expected - C::COLD_ACCOUNT_ACCESS
        );
    }

    /// EIP-7981: an access-list item is charged its per-item cost *plus* its
    /// data charge in the execution intrinsic, and contributes floor tokens.
    #[test]
    fn access_list_charged_in_both_execution_and_floor() {
        use GlamsterdamConstants as C;
        let s = GlamsterdamSchedule::new();
        let mut c = ctx();
        c.access_list_accounts = 2;
        c.access_list_storage_slots = 3;

        let per_address = C::TX_ACCESS_LIST_ADDRESS +
            C::ACCESS_LIST_ADDRESS_FLOOR_TOKENS * C::TX_DATA_TOKEN_FLOOR;
        let per_key = C::TX_ACCESS_LIST_STORAGE_KEY +
            C::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS * C::TX_DATA_TOKEN_FLOOR;
        assert_eq!(per_address, 4_180);
        assert_eq!(per_key, 4_048);

        let split = s.initial_and_floor_gas(&c).unwrap();
        assert_eq!(
            split.initial_regular_gas(),
            C::TX_BASE + C::COLD_ACCOUNT_ACCESS + 2 * per_address + 3 * per_key
        );

        // Floor: the same items' tokens, priced at the floor rate, on top of
        // the anchor.
        let al_tokens =
            2 * C::ACCESS_LIST_ADDRESS_FLOOR_TOKENS + 3 * C::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS;
        assert_eq!(
            split.floor_gas(),
            al_tokens * C::TX_DATA_TOKEN_FLOOR + C::TX_BASE + C::COLD_ACCOUNT_ACCESS
        );
    }

    #[test]
    fn authorization_intrinsic_splits_regular_and_state_gas() {
        use GlamsterdamConstants as C;
        let s = GlamsterdamSchedule::new();
        let mut c = ctx();
        c.authorization_count = 2;

        let split = s.initial_and_floor_gas(&c).unwrap();
        // Regular: the spec's EXECUTION_PER_AUTH_BASE_COST plus the
        // ACCOUNT_WRITE that `set_delegation` charges and revm omits.
        assert_eq!(
            split.initial_regular_gas(),
            C::TX_BASE + C::COLD_ACCOUNT_ACCESS + 2 * C::AUTH_REGULAR_PER_AUTH
        );
        // State: the documented deviation — reserved intrinsically rather than
        // at the top frame, following revm's EIP-8037 model.
        assert_eq!(split.initial_state_gas, 2 * C::AUTH_STATE_GAS);
    }

    /// `tx_eip7702_per_empty_account_cost` is a *composing* accessor: it adds
    /// the state-gas portion to whatever the slot holds. The slot must
    /// therefore carry only the regular cost — writing the combined figure
    /// silently double-counts ~218k state gas per authorization, which is a
    /// mispricing large enough to change replay outcomes.
    #[test]
    fn auth_slot_holds_regular_cost_only_not_the_composed_total() {
        use GlamsterdamConstants as C;
        let params = gas_params_for(SpecId::AMSTERDAM, &glamsterdam_overrides());

        // The raw slot: regular only.
        assert_eq!(
            params.get(GasId::tx_eip7702_per_empty_account_cost()),
            C::AUTH_REGULAR_PER_AUTH
        );
        // The accessor: regular + state.
        assert_eq!(
            params.tx_eip7702_per_empty_account_cost(),
            C::AUTH_REGULAR_PER_AUTH + C::AUTH_STATE_GAS
        );
        assert_eq!(params.tx_eip7702_state_gas(), C::AUTH_STATE_GAS);
    }

    /// `set_delegation` charges `ACCOUNT_WRITE` execution gas per authorization
    /// on top of the intrinsic base cost, and revm charges no equivalent — its
    /// `apply_auth_list` only counts refunds and writes the designator. Without
    /// folding it in, every authorization is undercharged by 9,000 execution gas.
    #[test]
    fn auth_regular_cost_includes_the_account_write_revm_omits() {
        use GlamsterdamConstants as C;
        assert_eq!(C::EXECUTION_PER_AUTH_BASE_COST, 7_816);
        assert_eq!(C::AUTH_REGULAR_PER_AUTH, 7_816 + 9_000);
        assert_eq!(C::AUTH_REGULAR_PER_AUTH, 16_816);
        // The gap is exactly one ACCOUNT_WRITE — the charge revm never makes.
        assert_eq!(C::AUTH_REGULAR_PER_AUTH - C::EXECUTION_PER_AUTH_BASE_COST, C::ACCOUNT_WRITE);

        // And it reaches the intrinsic: one authorization costs a full
        // ACCOUNT_WRITE more than the base cost alone would give.
        let s = GlamsterdamSchedule::new();
        let mut c = ctx();
        c.authorization_count = 1;
        let with_auth = s.initial_and_floor_gas(&c).unwrap().initial_regular_gas();
        let without = s.initial_and_floor_gas(&ctx()).unwrap().initial_regular_gas();
        assert_eq!(with_auth - without, C::AUTH_REGULAR_PER_AUTH);
    }

    /// Every slot the schedule claims to set must actually land, at the
    /// spec-correct value, on the table the EVM will read. Guards against a
    /// silently-wrong add-on/absolute mix-up.
    #[test]
    fn overrides_produce_spec_correct_table() {
        use GlamsterdamConstants as C;
        let params = gas_params_for(SpecId::AMSTERDAM, &glamsterdam_overrides());
        let g = |id| params.get(id);

        // Cold costs are warm + add-on; assert the TOTAL, which is what the
        // spec specifies and where an add-on/absolute confusion would show.
        assert_eq!(g(GasId::warm_storage_read_cost()), C::WARM_ACCESS);
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_account_additional_cost()),
            C::COLD_ACCOUNT_ACCESS
        );
        assert_eq!(
            g(GasId::warm_storage_read_cost()) + g(GasId::cold_storage_additional_cost()),
            C::COLD_STORAGE_ACCESS
        );
        assert_eq!(
            g(GasId::sstore_static()) + g(GasId::cold_storage_cost()),
            C::COLD_STORAGE_ACCESS
        );

        // Absolute slots.
        assert_eq!(g(GasId::sstore_set_without_load_cost()), C::STORAGE_WRITE);
        assert_eq!(g(GasId::sstore_reset_without_cold_load_cost()), C::STORAGE_WRITE);
        assert_eq!(g(GasId::sstore_clearing_slot_refund()), C::REFUND_STORAGE_CLEAR);
        assert_eq!(g(GasId::create()), C::CREATE_ACCESS);
        assert_eq!(g(GasId::tx_create_cost()), C::CREATE_ACCESS);
        assert_eq!(g(GasId::transfer_value_cost()), C::CALL_VALUE);
        assert_eq!(g(GasId::new_account_cost_for_selfdestruct()), C::ACCOUNT_WRITE);
        assert_eq!(g(GasId::tx_base_stipend()), C::TX_BASE);
        assert_eq!(g(GasId::tx_recipient_access_cost()), C::COLD_ACCOUNT_ACCESS);
        assert_eq!(g(GasId::tx_value_cost()), C::TX_VALUE_COST);
        assert_eq!(g(GasId::tx_floor_cost_per_token()), C::TX_DATA_TOKEN_FLOOR);

        // A plain CALL that creates an account pays NO execution-gas
        // account-write charge — that cost is entirely state gas.
        assert_eq!(g(GasId::new_account_cost()), 0);

        // EIP-8037 state gas comes from revm's native Amsterdam table and
        // already matches the spec, so it is not overridden — assert it anyway,
        // so a revm bump that changes CPSB is caught here.
        assert_eq!(g(GasId::new_account_state_gas()), C::NEW_ACCOUNT_STATE_GAS);
        assert_eq!(g(GasId::sstore_set_state_gas()), C::STORAGE_SET_STATE_GAS);
        assert_eq!(g(GasId::create_state_gas()), C::NEW_ACCOUNT_STATE_GAS);
        assert_eq!(g(GasId::code_deposit_state_gas()), C::CPSB);
    }

    /// revm's baked-in AMSTERDAM table is stale in specific, known ways. If a
    /// revm bump fixes any of them the override becomes redundant — this test
    /// documents which ones are load-bearing today.
    #[test]
    fn overrides_are_load_bearing_against_revm_defaults() {
        use GlamsterdamConstants as C;
        let native = GasParams::new_spec(SpecId::AMSTERDAM);

        assert_ne!(native.get(GasId::tx_base_stipend()), C::TX_BASE);
        assert_ne!(native.get(GasId::create()), C::CREATE_ACCESS);
        assert_ne!(native.get(GasId::tx_create_cost()), C::CREATE_ACCESS);
        assert_ne!(native.get(GasId::sstore_set_without_load_cost()), C::STORAGE_WRITE);
        assert_ne!(
            native.get(GasId::warm_storage_read_cost()) +
                native.get(GasId::cold_account_additional_cost()),
            C::COLD_ACCOUNT_ACCESS
        );
        // The recipient slots do not exist pre-Glamsterdam at all.
        assert_eq!(native.get(GasId::tx_recipient_access_cost()), 0);
        assert_eq!(native.get(GasId::tx_value_cost()), 0);
    }

    #[test]
    fn extcode_second_read_is_the_only_inspector_delta() {
        let s = GlamsterdamSchedule::new();
        let c = OpcodeContext::default();
        assert_eq!(s.opcode_gas_delta(opcode::EXTCODESIZE, &c), 100);
        assert_eq!(s.opcode_gas_delta(opcode::EXTCODECOPY, &c), 100);
        // EXTCODEHASH reads only the hash — no second read.
        assert_eq!(s.opcode_gas_delta(opcode::EXTCODEHASH, &c), 0);
        // Everything else is priced natively via gas_params.
        for op in [opcode::BALANCE, opcode::SLOAD, opcode::SSTORE, opcode::CALL, opcode::CREATE] {
            assert_eq!(s.opcode_gas_delta(op, &c), 0, "opcode {op:#x}");
        }
        // The breakdown must reconcile with the delta for every opcode.
        for op in 0u8..=255 {
            assert_eq!(
                s.opcode_gas_tax_breakdown(op, &c).total(),
                s.opcode_gas_delta(op, &c),
                "opcode {op:#x}"
            );
        }
    }

    #[test]
    fn configure_evm_env_switches_spec_and_preserves_code_limits() {
        let s = GlamsterdamSchedule::new();
        let mut env = EvmEnv::default();
        env.cfg_env.limit_contract_code_size = Some(24_576);
        env.cfg_env.limit_contract_initcode_size = Some(49_152);

        assert!(s.configure_evm_env(&mut env));
        assert_eq!(env.cfg_env.spec, SpecId::AMSTERDAM);
        assert!(env.cfg_env.is_amsterdam_eip8037_enabled());
        // The block's limits survive the spec switch.
        assert_eq!(env.cfg_env.max_code_size(), 24_576);
        assert_eq!(env.cfg_env.max_initcode_size(), 49_152);
        // And the overrides landed on the env the EVM will actually use.
        assert_eq!(
            env.cfg_env.gas_params.get(GasId::tx_base_stipend()),
            GlamsterdamConstants::TX_BASE
        );
    }

    #[test]
    fn schedule_metadata() {
        let s = GlamsterdamSchedule::new();
        assert_eq!(s.name(), "glamsterdam-v1");
        assert_eq!(s.kind(), ScheduleKind::Both);
        assert!(s.modifies_intrinsic());
        assert!(s.modifies_execution());
        // The EVM charges the intrinsic, so the runner must not compensate.
        assert!(s.uses_native_intrinsic_gas());
        assert_eq!(s.replay_bump_multiplier(), Some(10));
    }

    #[test]
    fn tx_category_covers_every_recipient_arm() {
        let s = GlamsterdamSchedule::new();

        let mut c = ctx();
        assert_eq!(s.tx_category(&c).unwrap(), "plain_call");
        c.value = U256::from(1);
        assert_eq!(s.tx_category(&c).unwrap(), "value_call");

        let mut c = ctx();
        c.recipient = Some(c.sender);
        assert_eq!(s.tx_category(&c).unwrap(), "self_transfer");

        let mut c = ctx();
        c.is_create = true;
        c.recipient = None;
        assert_eq!(s.tx_category(&c).unwrap(), "contract_creation");

        let mut c = ctx();
        c.authorization_count = 1;
        assert_eq!(s.tx_category(&c).unwrap(), "authorization");
    }

    #[test]
    fn config_fingerprint_embeds_every_constant() {
        let fp = GlamsterdamSchedule::new().config_fingerprint();
        assert!(fp.contains("spec=execution-specs/forks/amsterdam"));
        assert!(fp.contains("tx_base=12000"));
        assert!(fp.contains("cold_account=3000"));
        assert!(fp.contains("cold_storage=2100"));
        assert!(fp.contains("account_write=9000"));
        assert!(fp.contains("create_access=12000"));
        assert!(fp.contains("refund_storage_clear=11616"));
        assert!(fp.contains("auth_base=7816"));
        assert!(fp.contains("auth_regular=16816"));
        assert!(fp.contains("cpsb=1530"));
        assert!(fp.contains("auth_state_gas=intrinsic"));
        // Distinct from the single-EIP lanes, so datasets never collide.
        assert_ne!(fp, crate::schedule::Eip8037Schedule::new().config_fingerprint());
        assert_ne!(fp, crate::schedule::Eip8038Schedule::new().config_fingerprint());
    }
}
