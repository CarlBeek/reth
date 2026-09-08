//! Pins revm's native `SpecId::AMSTERDAM` gas rules against
//! `ethereum/execution-specs` branch `forks/amsterdam` (glamsterdam devnet-8).
//!
//! The `amsterdam` research lane overrides *nothing* — it replays on revm's own
//! Amsterdam table. That is only sound while revm's table matches the spec, so
//! this file is the check that makes the deletion of the crate's former
//! hand-ported constant table safe. A revm bump that renumbers Amsterdam breaks
//! these tests, which is the intended signal: re-read the spec, then re-key the
//! dataset (the schedule's `config_fingerprint` reads the same constants, so it
//! moves automatically).
//!
//! Spec references, all under `forks/amsterdam`:
//!   - `vm/gas.py` — `GasCosts`, `StateGasCosts`
//!   - `transactions.py` — `calculate_intrinsic_cost`
//!   - `vm/instructions/{storage,system,environment}.py`
//!
//! # Two layers
//!
//! 1. **Constants** — the spec's named values. revm keeps its own copies in
//!    `revm::primitives::{eip2780, eip8037, eip8038}`.
//! 2. **Composition** — the built `GasParams` slots. This is the layer that can be wrong even when
//!    every constant is right, because revm reaches a cold cost by *adding* a premium to the warm
//!    base while the spec quotes the total. Getting that decomposition wrong is a silent ±100 gas
//!    on every cold access, so each slot below is asserted against the spec-side identity it has to
//!    reproduce, not against a transcribed number.

use revm::{
    context_interface::cfg::gas_params::{Eip2780TxInfo, GasId, GasParams},
    primitives::{eip2780, eip8037, eip8038, hardfork::SpecId, U256},
};

/// Spec constants, transcribed from `forks/amsterdam` `vm/gas.py::GasCosts`.
mod spec {
    pub(crate) const WARM_ACCESS: u64 = 100;
    pub(crate) const COLD_ACCOUNT_ACCESS: u64 = 3_000;
    pub(crate) const COLD_STORAGE_ACCESS: u64 = 2_100;
    pub(crate) const ACCOUNT_WRITE: u64 = 9_000;
    pub(crate) const STORAGE_WRITE: u64 = 10_000;
    pub(crate) const CALL_STIPEND: u64 = 2_300;
    pub(crate) const CALL_VALUE: u64 = 11_300;
    pub(crate) const CREATE_ACCESS: u64 = 12_000;
    pub(crate) const REFUND_STORAGE_CLEAR: u64 = 11_616;
    pub(crate) const TX_ACCESS_LIST_ADDRESS: u64 = 2_900;
    pub(crate) const TX_ACCESS_LIST_STORAGE_KEY: u64 = 2_000;
    pub(crate) const EXECUTION_PER_AUTH_BASE_COST: u64 = 7_816;

    // `transactions.py::calculate_intrinsic_cost`
    pub(crate) const TX_BASE: u64 = 12_000;
    pub(crate) const TX_VALUE_COST: u64 = 6_000;
    pub(crate) const TX_DATA_TOKEN_STANDARD: u64 = 4;
    pub(crate) const TX_DATA_TOKEN_FLOOR: u64 = 16;

    // EIP-7981 access-list floor tokens
    pub(crate) const ACCESS_LIST_ADDRESS_FLOOR_TOKENS: u64 = 80;
    pub(crate) const ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS: u64 = 128;

    // `vm/gas.py::StateGasCosts`
    pub(crate) const COST_PER_STATE_BYTE: u64 = 1_530;
    pub(crate) const STATE_BYTES_PER_NEW_ACCOUNT: u64 = 120;
    pub(crate) const STATE_BYTES_PER_STORAGE_SET: u64 = 64;
    pub(crate) const STATE_BYTES_PER_AUTH_BASE: u64 = 23;
    pub(crate) const STATE_BYTES_PER_CODE_BYTE: u64 = 1;
}

fn amsterdam() -> GasParams {
    GasParams::new_spec(SpecId::AMSTERDAM)
}

// ─────────────────────── Layer 1: constants ───────────────────────

#[test]
fn revm_constants_match_execution_specs() {
    assert_eq!(eip8038::WARM_ACCESS, spec::WARM_ACCESS);
    assert_eq!(eip8038::COLD_ACCOUNT_ACCESS, spec::COLD_ACCOUNT_ACCESS);
    assert_eq!(eip8038::COLD_STORAGE_ACCESS, spec::COLD_STORAGE_ACCESS);
    assert_eq!(eip8038::ACCOUNT_WRITE, spec::ACCOUNT_WRITE);
    assert_eq!(eip8038::STORAGE_WRITE, spec::STORAGE_WRITE);
    assert_eq!(eip8038::CALL_VALUE, spec::CALL_VALUE);
    assert_eq!(eip8038::CREATE_ACCESS, spec::CREATE_ACCESS);
    assert_eq!(eip8038::STORAGE_CLEAR_REFUND, spec::REFUND_STORAGE_CLEAR);
    assert_eq!(eip8038::ACCESS_LIST_ADDRESS_COST, spec::TX_ACCESS_LIST_ADDRESS);
    assert_eq!(eip8038::ACCESS_LIST_STORAGE_KEY_COST, spec::TX_ACCESS_LIST_STORAGE_KEY);
    assert_eq!(eip8038::EIP7702_PER_AUTH_BASE_REGULAR, spec::EXECUTION_PER_AUTH_BASE_COST);
    assert_eq!(eip8038::TX_DATA_TOKEN_FLOOR, spec::TX_DATA_TOKEN_FLOOR);

    assert_eq!(eip2780::TX_BASE_COST, spec::TX_BASE);
    assert_eq!(eip2780::TX_VALUE_COST, spec::TX_VALUE_COST);

    assert_eq!(eip8037::CPSB_GLAMSTERDAM, spec::COST_PER_STATE_BYTE);
    assert_eq!(eip8037::NEW_ACCOUNT_BYTES, spec::STATE_BYTES_PER_NEW_ACCOUNT);
    assert_eq!(eip8037::SSTORE_SET_BYTES, spec::STATE_BYTES_PER_STORAGE_SET);
    assert_eq!(eip8037::AUTH_BASE_BYTES, spec::STATE_BYTES_PER_AUTH_BASE);
    assert_eq!(eip8037::CODE_DEPOSIT_PER_BYTE, spec::STATE_BYTES_PER_CODE_BYTE);
}

/// The spec *derives* several constants. Asserting the derivations (not just the
/// values) means a renumber of one base propagates the way the spec says it
/// should, instead of leaving an inconsistent pair.
#[test]
fn revm_constants_preserve_spec_derivations() {
    assert_eq!(eip8038::CREATE_ACCESS, eip8038::ACCOUNT_WRITE + eip8038::COLD_ACCOUNT_ACCESS);
    assert_eq!(eip8038::CALL_VALUE, eip8038::ACCOUNT_WRITE + spec::CALL_STIPEND);
    assert_eq!(
        eip8038::ACCESS_LIST_ADDRESS_COST,
        eip8038::COLD_ACCOUNT_ACCESS - eip8038::WARM_ACCESS
    );
    assert_eq!(
        eip8038::ACCESS_LIST_STORAGE_KEY_COST,
        eip8038::COLD_STORAGE_ACCESS - eip8038::WARM_ACCESS
    );
    // `(STORAGE_WRITE + COLD_STORAGE_ACCESS) * 4800 / 5000`
    assert_eq!(
        eip8038::STORAGE_CLEAR_REFUND,
        (eip8038::STORAGE_WRITE + eip8038::COLD_STORAGE_ACCESS) * 4_800 / 5_000
    );
}

// ─────────────────── Layer 2: composed table slots ───────────────────

/// Access costs. revm stores cold as a *premium over warm*; the spec quotes the
/// total. This is the ±100 trap.
#[test]
fn access_slots_compose_to_spec_totals() {
    let p = amsterdam();
    let warm = p.get(GasId::warm_storage_read_cost());

    assert_eq!(warm, spec::WARM_ACCESS);
    assert_eq!(warm + p.get(GasId::cold_account_additional_cost()), spec::COLD_ACCOUNT_ACCESS);
    assert_eq!(warm + p.get(GasId::cold_storage_additional_cost()), spec::COLD_STORAGE_ACCESS);
    // SSTORE charges `sstore_static` unconditionally and adds `cold_storage_cost`
    // when cold, so the two must also sum to the spec's total.
    assert_eq!(
        p.get(GasId::sstore_static()) + p.get(GasId::cold_storage_cost()),
        spec::COLD_STORAGE_ACCESS
    );
    assert_eq!(p.get(GasId::sstore_static()), spec::WARM_ACCESS);
}

/// Storage writes and refunds are absolute, not add-ons.
#[test]
fn storage_write_slots_match_spec() {
    let p = amsterdam();

    assert_eq!(p.get(GasId::sstore_set_without_load_cost()), spec::STORAGE_WRITE);
    assert_eq!(p.get(GasId::sstore_reset_without_cold_load_cost()), spec::STORAGE_WRITE);
    assert_eq!(p.get(GasId::sstore_set_refund()), spec::STORAGE_WRITE);
    assert_eq!(p.get(GasId::sstore_reset_refund()), spec::STORAGE_WRITE);
    assert_eq!(p.get(GasId::sstore_clearing_slot_refund()), spec::REFUND_STORAGE_CLEAR);

    // A cold SSTORE to a fresh slot pays the spec's access + write.
    assert_eq!(
        p.get(GasId::sstore_static()) +
            p.get(GasId::cold_storage_cost()) +
            p.get(GasId::sstore_set_without_load_cost()),
        spec::COLD_STORAGE_ACCESS + spec::STORAGE_WRITE
    );
}

/// Calls, creation and selfdestruct.
#[test]
fn call_and_create_slots_match_spec() {
    let p = amsterdam();

    assert_eq!(p.get(GasId::transfer_value_cost()), spec::CALL_VALUE);
    assert_eq!(p.get(GasId::create()), spec::CREATE_ACCESS);
    assert_eq!(p.get(GasId::tx_create_cost()), spec::CREATE_ACCESS);

    // A plain CALL that creates an account pays no execution-gas account write —
    // that cost is entirely state gas. SELFDESTRUCT to a non-existent
    // beneficiary still pays ACCOUNT_WRITE (`vm/instructions/system.py`).
    assert_eq!(p.get(GasId::new_account_cost()), 0);
    assert_eq!(p.get(GasId::new_account_cost_for_selfdestruct()), spec::ACCOUNT_WRITE);

    // Code deposit is state gas at CPSB/byte, so the execution-gas slot is zero.
    assert_eq!(p.get(GasId::code_deposit_cost()), 0);
}

/// EIP-8037 state gas: bytes × CPSB, baked into the table.
#[test]
fn state_gas_slots_match_spec() {
    let p = amsterdam();
    let cpsb = spec::COST_PER_STATE_BYTE;

    assert_eq!(p.get(GasId::sstore_set_state_gas()), spec::STATE_BYTES_PER_STORAGE_SET * cpsb);
    assert_eq!(p.get(GasId::new_account_state_gas()), spec::STATE_BYTES_PER_NEW_ACCOUNT * cpsb);
    assert_eq!(p.get(GasId::create_state_gas()), spec::STATE_BYTES_PER_NEW_ACCOUNT * cpsb);
    assert_eq!(p.get(GasId::code_deposit_state_gas()), spec::STATE_BYTES_PER_CODE_BYTE * cpsb);
    assert_eq!(
        p.get(GasId::tx_eip7702_state_gas_bytecode()),
        spec::STATE_BYTES_PER_AUTH_BASE * cpsb
    );
}

/// Access-list items: the spec charges the item cost *and* its bytes at the
/// floor rate, both inside `access_list_cost`. revm bakes the data charge into
/// the per-item slot.
#[test]
fn access_list_slots_bake_in_the_eip7981_data_charge() {
    let p = amsterdam();

    assert_eq!(
        p.get(GasId::tx_access_list_address_cost()),
        spec::TX_ACCESS_LIST_ADDRESS +
            spec::ACCESS_LIST_ADDRESS_FLOOR_TOKENS * spec::TX_DATA_TOKEN_FLOOR
    );
    assert_eq!(
        p.get(GasId::tx_access_list_storage_key_cost()),
        spec::TX_ACCESS_LIST_STORAGE_KEY +
            spec::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS * spec::TX_DATA_TOKEN_FLOOR
    );
    // 80 tokens over 20 address bytes / 128 over 32 key bytes = 4 tokens a byte.
    assert_eq!(p.get(GasId::tx_access_list_floor_byte_multiplier()), 4);
    assert_eq!(
        spec::ACCESS_LIST_ADDRESS_FLOOR_TOKENS,
        20 * p.get(GasId::tx_access_list_floor_byte_multiplier())
    );
    assert_eq!(
        spec::ACCESS_LIST_STORAGE_KEY_FLOOR_TOKENS,
        32 * p.get(GasId::tx_access_list_floor_byte_multiplier())
    );
}

/// EIP-2780 / EIP-7976 intrinsic and floor anchors.
///
/// # `TX_BASE` is not a table slot
///
/// `tx_base_stipend` deliberately stays at the legacy 21,000 even on Amsterdam:
/// it still serves the pre-EIP-2780 branch of `initial_tx_gas`. The decomposed
/// path reads `eip2780::TX_BASE_COST` as a **compile-time constant**, so
/// `TX_BASE` (and `TX_VALUE_COST`, and the recipient's `COLD_ACCOUNT_ACCESS`)
/// cannot be reached through `GasParams` at all. A schedule that wants to sweep
/// those three has to patch revm — overriding the table silently does nothing.
/// `tx_floor_cost_base_gas` *is* re-anchored on the slot, hence the asymmetry.
#[test]
fn intrinsic_slots_match_spec() {
    let p = amsterdam();

    assert_eq!(p.get(GasId::tx_base_stipend()), 21_000, "legacy branch keeps the flat base");
    assert_eq!(p.get(GasId::tx_floor_cost_base_gas()), spec::TX_BASE);
    assert_eq!(p.get(GasId::tx_token_cost()), spec::TX_DATA_TOKEN_STANDARD);
    assert_eq!(p.get(GasId::tx_floor_cost_per_token()), spec::TX_DATA_TOKEN_FLOOR);
    assert_eq!(p.get(GasId::tx_account_write_cost()), spec::ACCOUNT_WRITE);
    assert_eq!(p.get(GasId::tx_create_access_cost()), spec::CREATE_ACCESS);

    // Per-auth intrinsic is the state-*independent* base only; the
    // state-dependent remainder is charged at the runtime phase.
    assert_eq!(p.get(GasId::tx_eip7702_regular_gas()), spec::EXECUTION_PER_AUTH_BASE_COST);
    assert_eq!(p.get(GasId::tx_eip7702_regular_refund()), 0);
}

// ──────────── Layer 2b: the intrinsic as actually computed ────────────

fn intrinsic(is_create: bool, value: u64, is_self_transfer: bool) -> u64 {
    amsterdam()
        .initial_tx_gas(
            &[],
            is_create,
            0,
            0,
            0,
            Some(Eip2780TxInfo { value: U256::from(value), is_self_transfer }),
        )
        .initial_regular_gas()
}

/// `calculate_intrinsic_cost`'s four arms, with empty calldata so only the
/// base + recipient terms show.
#[test]
fn intrinsic_gas_matches_spec_for_each_recipient_arm() {
    // Plain call to someone else, no value: base + cold recipient access.
    assert_eq!(intrinsic(false, 0, false), spec::TX_BASE + spec::COLD_ACCOUNT_ACCESS);

    // Value-bearing call to someone else: adds TX_VALUE_COST.
    assert_eq!(
        intrinsic(false, 1, false),
        spec::TX_BASE + spec::COLD_ACCOUNT_ACCESS + spec::TX_VALUE_COST
    );

    // Self-transfer pays neither the recipient nor the value charge.
    assert_eq!(intrinsic(false, 1, true), spec::TX_BASE);

    // Creation pays CREATE_ACCESS and — since devnet-8 — no value charge.
    assert_eq!(intrinsic(true, 0, false), spec::TX_BASE + spec::CREATE_ACCESS);
    assert_eq!(intrinsic(true, 1, false), spec::TX_BASE + spec::CREATE_ACCESS);
}

/// EIP-8037 moved the state-dependent charges out of the intrinsic phase and
/// into a runtime phase, so the intrinsic reserves no state gas. This pins that
/// model: `schedule_initial_state_gas` is expected to be 0 here, and the
/// reservoir columns derive from it.
#[test]
fn intrinsic_reserves_no_state_gas() {
    for (is_create, auths) in [(false, 0), (true, 0), (false, 2), (true, 2)] {
        let gas = amsterdam().initial_tx_gas(
            &[],
            is_create,
            0,
            0,
            auths,
            Some(Eip2780TxInfo { value: U256::ZERO, is_self_transfer: false }),
        );
        assert_eq!(
            gas.initial_state_gas, 0,
            "is_create={is_create} auths={auths}: state gas must be charged at the runtime phase"
        );
    }
}

// ────────── Layer 3: behavior, measured against a real EVM ──────────

/// EIP-8038 adds a `WARM_ACCESS` second-database-read surcharge to
/// `EXTCODESIZE` / `EXTCODECOPY` — `vm/instructions/environment.py`, the
/// `# Code reading cost (EIP-8038)` line — on top of the normal account access
/// and *regardless of whether that access was cold or warm*. `EXTCODEHASH` is
/// excluded: it reads only the hash, not the code.
///
/// There is no `GasId` slot for the surcharge, so it cannot be checked by
/// reading the table. This measures it instead, by differencing two programs
/// that are identical except for the one opcode.
///
/// This is the assertion that lets `AmsterdamSchedule` add nothing of its own.
/// If it ever fails, the schedule has to supply the difference again through
/// `GasSchedule::opcode_gas_tax_breakdown` — but do not add it while this
/// passes, or every EXT\* op is charged twice.
#[test]
fn ext_second_read_surcharge_is_charged_by_revm() {
    // BALANCE is the control: same account-access shape, no code read.
    assert_eq!(access_cost(BALANCE, Warmth::Cold), spec::COLD_ACCOUNT_ACCESS);
    assert_eq!(access_cost(BALANCE, Warmth::Warm), spec::WARM_ACCESS);

    // EXTCODESIZE reads code, so it pays the surcharge in both regimes.
    assert_eq!(
        access_cost(EXTCODESIZE, Warmth::Cold),
        spec::COLD_ACCOUNT_ACCESS + spec::WARM_ACCESS
    );
    assert_eq!(access_cost(EXTCODESIZE, Warmth::Warm), spec::WARM_ACCESS + spec::WARM_ACCESS);

    // EXTCODEHASH is excluded by the spec.
    assert_eq!(access_cost(EXTCODEHASH, Warmth::Cold), spec::COLD_ACCOUNT_ACCESS);
    assert_eq!(access_cost(EXTCODEHASH, Warmth::Warm), spec::WARM_ACCESS);
}

const BALANCE: u8 = 0x31;
const EXTCODESIZE: u8 = 0x3b;
const EXTCODEHASH: u8 = 0x3f;

enum Warmth {
    Cold,
    Warm,
}

/// Gas charged by one account-touching opcode against `TARGET`, isolated by
/// differencing two programs whose only difference is that opcode.
///
/// `Warm` measures the *second* touch: both programs touch the account once
/// first, so the cold premium cancels and what remains is the warm cost.
fn access_cost(opcode: u8, warmth: Warmth) -> u64 {
    // PUSH20 TARGET, <opcode>, POP  — and the same without <opcode>.
    let touch = |op: Option<u8>| {
        let mut v = vec![0x73];
        v.extend_from_slice(&TARGET);
        if let Some(op) = op {
            v.push(op);
        }
        v.push(0x50); // POP
        v
    };

    let (probe, control) = match warmth {
        Warmth::Cold => (touch(Some(opcode)), touch(None)),
        // Prefix both with an identical warming touch of the same opcode.
        Warmth::Warm => {
            let warm_up = touch(Some(opcode));
            let mut probe = warm_up.clone();
            probe.extend_from_slice(&touch(Some(opcode)));
            let mut control = warm_up;
            control.extend_from_slice(&touch(None));
            (probe, control)
        }
    };

    run_probe(&probe) - run_probe(&control)
}

const TARGET: [u8; 20] = [0x44; 20];

/// Total gas spent running `code` as a call target under Amsterdam.
fn run_probe(code: &[u8]) -> u64 {
    use revm::{
        bytecode::Bytecode,
        context::{Context, TxEnv},
        database::CacheDB,
        database_interface::EmptyDB,
        primitives::{keccak256, Address, Bytes, TxKind},
        state::AccountInfo,
        ExecuteEvm, MainBuilder, MainContext,
    };

    let acct = |code: &[u8]| AccountInfo {
        code_hash: keccak256(code),
        code: Some(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        ..Default::default()
    };

    let probe_addr = Address::from([0x11; 20]);
    let caller = Address::from([0x22; 20]);

    let mut program = code.to_vec();
    program.push(0x00); // STOP

    let mut db = CacheDB::<EmptyDB>::default();
    // TARGET carries code so the EXT* opcodes perform a real code read.
    db.insert_account_info(Address::from(TARGET), acct(&[0x00, 0x00, 0x00]));
    db.insert_account_info(probe_addr, acct(&program));
    db.insert_account_info(
        caller,
        AccountInfo { balance: U256::from(1u64) << 100, ..Default::default() },
    );

    let mut evm = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            *cfg = cfg.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
        })
        .build_mainnet();

    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(probe_addr))
        .gas_limit(2_000_000)
        .gas_price(0)
        .build()
        .expect("valid tx env");

    let out = evm.transact(tx).expect("execution completes");
    assert!(out.result.is_success(), "probe must run to completion: {:?}", out.result);
    out.result.gas().total_gas_spent()
}
