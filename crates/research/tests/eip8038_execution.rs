//! End-to-end gas validation for the EIP-8038 (misilva73) schedule.
//!
//! Runs real transactions in an in-memory `CacheDB` (no reth datadir is touched)
//! under the schedule's `gas_params` override and asserts the exact gas charged.
//! This validates the error-prone Layer-1 back-solve (warm base + cold add-on +
//! write surcharge), which the schedule's unit tests only confirm at the slot
//! level — here revm actually charges it.

use reth_research::{
    schedule::{apply_eip8038_gas_overrides, Eip8038Schedule},
    ScheduleInspector,
};
use revm::{
    bytecode::Bytecode,
    context::{Context, TxEnv},
    database::CacheDB,
    database_interface::EmptyDB,
    inspector::InspectEvm,
    primitives::{hardfork::SpecId, keccak256, Address, Bytes, TxKind, U256},
    state::AccountInfo,
    ExecuteEvm, MainBuilder, MainContext,
};
use std::sync::Arc;

fn code_account(code: &[u8]) -> AccountInfo {
    AccountInfo {
        code_hash: keccak256(code),
        code: Some(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        ..Default::default()
    }
}

/// Execute `code` as a CALL target under native OSAKA gas params + EIP-8038's
/// overrides, returning the transaction's total `gas_used`.
fn run_eip8038(code: &[u8]) -> u64 {
    let target = Address::from([0x11u8; 20]);
    let caller = Address::from([0x22u8; 20]);

    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(target, code_account(code));
    db.insert_account_info(
        caller,
        AccountInfo { balance: U256::from(1u64) << 100, ..Default::default() },
    );

    let mut evm = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            // Native OSAKA table, then EIP-8038's absolute slot overrides on top
            // (the exact override `configure_evm_env` applies).
            *cfg = cfg.clone().with_spec_and_mainnet_gas_params(SpecId::OSAKA);
            apply_eip8038_gas_overrides(&mut cfg.gas_params);
        })
        .build_mainnet();

    let tx = TxEnv::builder()
        .caller(caller)
        .kind(TxKind::Call(target))
        .gas_limit(2_000_000)
        .gas_price(0)
        .build()
        .expect("valid tx env");

    evm.transact(tx).expect("execution completes").result.tx_gas_used()
}

const EXECUTOR: Address = Address::new([0x11u8; 20]);
const CALLER: Address = Address::new([0x22u8; 20]);

/// A pure EOA (balance, no code) — `code_hash == KECCAK_EMPTY` → `NO_CODE`.
fn eoa_account() -> AccountInfo {
    AccountInfo { balance: U256::from(1u64), ..Default::default() }
}

/// Run `exec_code` as the called contract under the FULL EIP-8038 schedule:
/// Layer-1 `gas_params` overrides AND the Layer-3 inspector deltas (the warm-base
/// correction, cold-account CODE surcharge, EXT* second-read). `probes` are extra
/// accounts the bytecode accesses. Returns `(gas_used, inspector)` so tests can
/// assert both the charged gas and the cold-account classification counters.
///
/// Unlike [`run_eip8038`] (Layer-1 only), this attaches a [`ScheduleInspector`]
/// via `inspect_tx`, which is the only path that applies `opcode_gas_delta` — so
/// it's required to observe the warm-base / surcharge magnitudes for every opcode
/// except SSTORE.
fn run_eip8038_inspected(
    exec_code: &[u8],
    probes: &[(Address, AccountInfo)],
) -> (u64, ScheduleInspector) {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(EXECUTOR, code_account(exec_code));
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(1u64) << 100, ..Default::default() },
    );
    for (addr, info) in probes {
        db.insert_account_info(*addr, info.clone());
    }

    let inspector = ScheduleInspector::new(Arc::new(Eip8038Schedule::new()));
    let mut evm = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            *cfg = cfg.clone().with_spec_and_mainnet_gas_params(SpecId::OSAKA);
            apply_eip8038_gas_overrides(&mut cfg.gas_params);
        })
        .build_mainnet_with_inspector(inspector);

    let tx = TxEnv::builder()
        .caller(CALLER)
        .kind(TxKind::Call(EXECUTOR))
        .gas_limit(2_000_000)
        .gas_price(0)
        .build()
        .expect("valid tx env");

    let gas = evm.inspect_tx(tx).expect("execution completes").result.tx_gas_used();
    (gas, evm.inspector)
}

/// `PUSH20 <addr>; <op>; STOP` — a single account-access opcode against `addr`.
fn one_access(addr: Address, op: u8) -> Vec<u8> {
    let mut c = Vec::with_capacity(23);
    c.push(0x73); // PUSH20
    c.extend_from_slice(addr.as_slice());
    c.push(op);
    c.push(0x00); // STOP
    c
}

const PROBE: Address = Address::new([0x33u8; 20]);
// Intrinsic (21000) + PUSH20 (3) for a `one_access` bytecode; the opcode cost is
// then `gas_used - ONE_ACCESS_OVERHEAD`.
const ONE_ACCESS_OVERHEAD: u64 = 21_000 + 3;

// --- C1 regression guards: SLOAD warm/cold flow through the inspector ----------

#[test]
fn cold_sload_charges_8038_cost() {
    // PUSH1 0; SLOAD; STOP.
    let (gas, _) = run_eip8038_inspected(&[0x60, 0x00, 0x54, 0x00], &[]);
    // intrinsic(21000) + PUSH1(3) + cold SLOAD.
    assert_eq!(gas - 21_003, 2_735, "cold SLOAD = COLD_STORAGE_ACCESS 2735; got {gas}");
}

#[test]
fn warm_sload_charges_8038_cost() {
    // PUSH1 0; SLOAD(cold); POP; PUSH1 0; SLOAD(warm); STOP.
    let (gas, _) = run_eip8038_inspected(&[0x60, 0x00, 0x54, 0x50, 0x60, 0x00, 0x54, 0x00], &[]);
    // intrinsic + PUSH1(3) + cold(2735) + POP(2) + PUSH1(3) + warm.
    let warm = gas - 21_003 - 2_735 - 2 - 3;
    assert_eq!(warm, 62, "warm SLOAD = WARM_ACCESS 62; got gas_used={gas} (warm={warm})");
}

// --- Cold-account code/no-code split via real execution ------------------------

#[test]
fn cold_balance_to_contract_charges_8038_code_cost() {
    let (gas, _) =
        run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, code_account(&[0x00]))]);
    assert_eq!(gas - ONE_ACCESS_OVERHEAD, 9_131, "cold BALANCE to contract = CODE 9131; got {gas}");
}

#[test]
fn cold_balance_to_eoa_charges_8038_nocode_cost() {
    let (gas, _) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, eoa_account())]);
    assert_eq!(gas - ONE_ACCESS_OVERHEAD, 3_140, "cold BALANCE to EOA = NO_CODE 3140; got {gas}");
}

#[test]
fn cold_balance_to_empty_account_charges_nocode() {
    // Empty-but-existent account (Maria: NO_CODE).
    let (gas, _) =
        run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, AccountInfo::default())]);
    assert_eq!(
        gas - ONE_ACCESS_OVERHEAD,
        3_140,
        "cold BALANCE to empty acct = NO_CODE 3140; got {gas}"
    );
}

#[test]
fn cold_balance_to_nonexistent_charges_nocode() {
    // PROBE not inserted → db.basic() == None → NO_CODE.
    let (gas, _) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[]);
    assert_eq!(
        gas - ONE_ACCESS_OVERHEAD,
        3_140,
        "cold BALANCE to non-existent = NO_CODE 3140; got {gas}"
    );
}

#[test]
fn cold_balance_to_7702_delegated_charges_code_cost() {
    // EIP-7702 designator code → code_hash != KECCAK_EMPTY → ALWAYS CODE (Maria).
    let delegated = AccountInfo {
        code: Some(Bytecode::new_eip7702(Address::from([0xAAu8; 20]))),
        ..Default::default()
    };
    let (gas, _) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, delegated)]);
    assert_eq!(gas - ONE_ACCESS_OVERHEAD, 9_131, "cold BALANCE to 7702 = CODE 9131; got {gas}");
}

#[test]
fn warm_balance_charges_8038_cost() {
    // PUSH20; BALANCE(cold); POP; PUSH20; BALANCE(warm); STOP — to an EOA.
    let mut code = one_access(PROBE, 0x31);
    code.pop(); // drop the STOP
    code.push(0x50); // POP the first BALANCE result
    code.extend_from_slice(&one_access(PROBE, 0x31));
    let (gas, _) = run_eip8038_inspected(&code, &[(PROBE, eoa_account())]);
    // intrinsic + PUSH20(3) + cold(3140) + POP(2) + PUSH20(3) + warm.
    let warm = gas - 21_003 - 3_140 - 2 - 3;
    assert_eq!(warm, 62, "warm BALANCE = WARM_ACCESS 62; got gas_used={gas} (warm={warm})");
}

#[test]
fn cold_extcodesize_to_contract_adds_second_read() {
    // EXTCODESIZE (0x3B): cold CODE access (9131) + the modelled second DB read (+62).
    let (gas, _) =
        run_eip8038_inspected(&one_access(PROBE, 0x3B), &[(PROBE, code_account(&[0x00]))]);
    assert_eq!(
        gas - ONE_ACCESS_OVERHEAD,
        9_131 + 62,
        "cold EXTCODESIZE to contract = CODE 9131 + second-read 62 = 9193; got {gas}"
    );
}

// --- Classification counters populate from a real execution --------------------

#[test]
fn classification_counters_match_real_accesses() {
    // Cold contract → code counter.
    let (_, insp) =
        run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, code_account(&[0x00]))]);
    assert_eq!(insp.operation_counts().cold_account_code_count, 1);
    assert_eq!(insp.operation_counts().cold_account_nocode_count, 0);

    // Cold EOA → nocode counter.
    let (_, insp) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, eoa_account())]);
    assert_eq!(insp.operation_counts().cold_account_code_count, 0);
    assert_eq!(insp.operation_counts().cold_account_nocode_count, 1);

    // 7702-delegated → code counter.
    let delegated = AccountInfo {
        code: Some(Bytecode::new_eip7702(Address::from([0xAAu8; 20]))),
        ..Default::default()
    };
    let (_, insp) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, delegated)]);
    assert_eq!(insp.operation_counts().cold_account_code_count, 1);
}

/// A cold 0→nonzero SSTORE ("new slot") must charge
/// `COLD_STORAGE_ACCESS (2735) + STORAGE_WRITE (15391) = 18126`, proving the
/// warm-base / cold-add-on / set-surcharge decomposition is correct (revm
/// composes it as `sstore_static(62) + cold_storage_additional(2673) +
/// sstore_set_without_load(15391)`).
#[test]
fn cold_new_slot_sstore_charges_8038_cost() {
    // PUSH1 1, PUSH1 0, SSTORE, STOP.
    let gas_used = run_eip8038(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x00]);
    // gas_used = intrinsic(21000) + 2×PUSH1(3) + SSTORE + STOP(0).
    assert_eq!(
        gas_used - 21_006,
        2_735 + 15_391,
        "cold new-slot SSTORE should charge COLD_STORAGE_ACCESS + STORAGE_WRITE = 18126; \
         got gas_used={gas_used}"
    );
}

/// F8: the storage-reprice driver counters populate from a real execution
/// covering a cold fresh-set, a same-tx dirty re-write, and a cold + a warm
/// SLOAD.
#[test]
fn storage_reprice_drivers_counted() {
    // PUSH1 1, PUSH1 0, SSTORE   → slot0 = 1 (cold, set)
    // PUSH1 2, PUSH1 0, SSTORE   → slot0 = 2 (warm, dirty: original 0, current 1, new 2)
    // PUSH1 0, SLOAD, POP        → SLOAD slot0 (warm — already touched by the SSTOREs)
    // PUSH1 1, SLOAD, POP        → SLOAD slot1 (cold — never touched)
    // STOP
    let code = [
        0x60, 0x01, 0x60, 0x00, 0x55, // SSTORE slot0 = 1
        0x60, 0x02, 0x60, 0x00, 0x55, // SSTORE slot0 = 2
        0x60, 0x00, 0x54, 0x50, // SLOAD slot0, POP
        0x60, 0x01, 0x54, 0x50, // SLOAD slot1, POP
        0x00, // STOP
    ];
    let (_, insp) = run_eip8038_inspected(&code, &[]);
    let oc = insp.operation_counts();

    assert_eq!(oc.sstore_set_count, 1, "0→1 on a clean slot is a set");
    assert_eq!(oc.sstore_dirty_count, 1, "1→2 on an already-written slot is dirty");
    assert_eq!(oc.sstore_reset_count, 0);
    assert_eq!(oc.sstore_clear_count, 0);
    assert_eq!(oc.sstore_noop_count, 0);
    assert_eq!(oc.sstore_cold_count, 1, "only the first SSTORE touched slot0 cold");
    assert_eq!(oc.sload_cold_count, 1, "slot1 SLOAD is cold");
    assert_eq!(oc.sload_warm_count, 1, "slot0 SLOAD is warm (touched by the SSTOREs)");
}
