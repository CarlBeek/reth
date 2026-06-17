//! End-to-end gas validation for the EIP-8038 (PR 11802 uniform) schedule.
//!
//! Runs real transactions in an in-memory `CacheDB` (no reth datadir is touched)
//! under the schedule's `gas_params` overrides and asserts the exact gas charged.
//! This validates the error-prone back-solve — each cold add-on is stored as
//! `value − WARM_ACCESS` and revm adds it back on top of the warm base — which the
//! schedule's unit tests only confirm at the slot level; here revm actually
//! charges it. The single non-`gas_params` charge (the EXTCODESIZE/EXTCODECOPY
//! second-read `+WARM_ACCESS`) rides the inspector, so it's exercised via the
//! inspected path.

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

const EXECUTOR: Address = Address::new([0x11u8; 20]);
const CALLER: Address = Address::new([0x22u8; 20]);
const PROBE: Address = Address::new([0x33u8; 20]);

// Intrinsic (21000) + PUSH20 (3) for a `one_access` bytecode; the opcode cost is
// then `gas_used - ONE_ACCESS_OVERHEAD`.
const ONE_ACCESS_OVERHEAD: u64 = 21_000 + 3;

fn code_account(code: &[u8]) -> AccountInfo {
    AccountInfo {
        code_hash: keccak256(code),
        code: Some(Bytecode::new_raw(Bytes::copy_from_slice(code))),
        ..Default::default()
    }
}

/// A pure EOA (balance, no code).
fn eoa_account() -> AccountInfo {
    AccountInfo { balance: U256::from(1u64), ..Default::default() }
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

/// `CALL(gas=0xFFFF, addr, value, 0,0,0,0); POP; STOP` — one CALL to `addr`.
fn call_with_value(addr: Address, value: u8) -> Vec<u8> {
    let mut c = Vec::new();
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0   retLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0   retOffset
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0   argsLen
    c.extend_from_slice(&[0x60, 0x00]); // PUSH1 0   argsOffset
    c.extend_from_slice(&[0x60, value]); // PUSH1 value
    c.push(0x73); // PUSH20 addr
    c.extend_from_slice(addr.as_slice());
    c.extend_from_slice(&[0x61, 0xFF, 0xFF]); // PUSH2 0xFFFF  gas
    c.push(0xF1); // CALL
    c.push(0x50); // POP success
    c.push(0x00); // STOP
    c
}

/// Build the EVM for `code` as the called contract, optionally applying EIP-8038's
/// overrides, optionally with extra `probes` accounts inserted. Returns `gas_used`.
fn run(code: &[u8], apply_8038: bool, probes: &[(Address, AccountInfo)]) -> u64 {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(EXECUTOR, code_account(code));
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(1u64) << 100, ..Default::default() },
    );
    for (addr, info) in probes {
        db.insert_account_info(*addr, info.clone());
    }

    let mut evm = Context::mainnet()
        .with_db(db)
        .modify_cfg_chained(|cfg| {
            *cfg = cfg.clone().with_spec_and_mainnet_gas_params(SpecId::OSAKA);
            if apply_8038 {
                apply_eip8038_gas_overrides(&mut cfg.gas_params);
            }
        })
        .build_mainnet();

    let tx = TxEnv::builder()
        .caller(CALLER)
        .kind(TxKind::Call(EXECUTOR))
        .gas_limit(2_000_000)
        .gas_price(0)
        .build()
        .expect("valid tx env");

    evm.transact(tx).expect("execution completes").result.tx_gas_used()
}

/// Native OSAKA baseline (no 8038 overrides).
fn run_native(code: &[u8], probes: &[(Address, AccountInfo)]) -> u64 {
    run(code, false, probes)
}

/// EIP-8038 `gas_params` overrides, no inspector (validates the slot back-solve).
fn run_eip8038(code: &[u8], probes: &[(Address, AccountInfo)]) -> u64 {
    run(code, true, probes)
}

/// Full schedule path: EIP-8038 overrides AND the [`ScheduleInspector`] (the only
/// path that applies `opcode_gas_delta` — required for the EXTCODE* second read
/// and to read the classification counters). Returns `(gas_used, inspector)`.
fn run_eip8038_inspected(
    code: &[u8],
    probes: &[(Address, AccountInfo)],
) -> (u64, ScheduleInspector) {
    let mut db = CacheDB::<EmptyDB>::default();
    db.insert_account_info(EXECUTOR, code_account(code));
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

// --- Storage access: SLOAD / SSTORE ---------------------------------------------

#[test]
fn cold_sload_charges_cold_storage_access() {
    // PUSH1 0; SLOAD; STOP.
    let gas = run_eip8038(&[0x60, 0x00, 0x54, 0x00], &[]);
    // intrinsic(21000) + PUSH1(3) + cold SLOAD.
    assert_eq!(gas - 21_003, 3_000, "cold SLOAD = COLD_STORAGE_ACCESS 3000; got {gas}");
}

#[test]
fn warm_sload_charges_warm_access() {
    // PUSH1 0; SLOAD(cold); POP; PUSH1 0; SLOAD(warm); STOP.
    let gas = run_eip8038(&[0x60, 0x00, 0x54, 0x50, 0x60, 0x00, 0x54, 0x00], &[]);
    // intrinsic + PUSH1(3) + cold(3000) + POP(2) + PUSH1(3) + warm.
    let warm = gas - 21_003 - 3_000 - 2 - 3;
    assert_eq!(warm, 100, "warm SLOAD = WARM_ACCESS 100 (unchanged); got gas_used={gas}");
}

/// A cold 0→nonzero SSTORE ("new slot") charges
/// `COLD_STORAGE_ACCESS (3000) + STORAGE_WRITE (10000) = 13000` — proving the
/// warm-base / cold-add-on / set-surcharge decomposition (revm composes it as
/// `sstore_static(100) + cold_storage_cost(2900) + sstore_set_without_load(10000)`).
#[test]
fn cold_new_slot_sstore_charges_access_plus_write() {
    // PUSH1 1, PUSH1 0, SSTORE, STOP.
    let gas = run_eip8038(&[0x60, 0x01, 0x60, 0x00, 0x55, 0x00], &[]);
    // gas = intrinsic(21000) + 2×PUSH1(6) + SSTORE + STOP(0).
    assert_eq!(
        gas - 21_006,
        3_000 + 10_000,
        "cold new-slot SSTORE = COLD_STORAGE_ACCESS + STORAGE_WRITE = 13000; got {gas}"
    );
}

// --- Account access: BALANCE / EXTCODE* (uniform — no code/no-code split) --------

/// PR 11802 dropped the code/no-code split: a cold account access costs
/// `COLD_ACCOUNT_ACCESS (3000)` regardless of whether the target has code.
#[test]
fn cold_account_access_is_uniform_3000() {
    let to_contract = run_eip8038(&one_access(PROBE, 0x31), &[(PROBE, code_account(&[0x00]))]);
    let to_eoa = run_eip8038(&one_access(PROBE, 0x31), &[(PROBE, eoa_account())]);
    let to_absent = run_eip8038(&one_access(PROBE, 0x31), &[]);
    assert_eq!(to_contract - ONE_ACCESS_OVERHEAD, 3_000, "cold BALANCE to contract = 3000");
    assert_eq!(to_eoa - ONE_ACCESS_OVERHEAD, 3_000, "cold BALANCE to EOA = 3000");
    assert_eq!(to_absent - ONE_ACCESS_OVERHEAD, 3_000, "cold BALANCE to absent = 3000");
    // The whole point of PR 11802: all three are identical.
    assert_eq!(to_contract, to_eoa);
    assert_eq!(to_eoa, to_absent);
}

#[test]
fn warm_account_access_charges_warm_access() {
    // PUSH20; BALANCE(cold); POP; PUSH20; BALANCE(warm); STOP — to an EOA.
    let mut code = one_access(PROBE, 0x31);
    code.pop(); // drop the STOP
    code.push(0x50); // POP the first BALANCE result
    code.extend_from_slice(&one_access(PROBE, 0x31));
    let gas = run_eip8038(&code, &[(PROBE, eoa_account())]);
    // intrinsic + PUSH20(3) + cold(3000) + POP(2) + PUSH20(3) + warm.
    let warm = gas - 21_003 - 3_000 - 2 - 3;
    assert_eq!(warm, 100, "warm BALANCE = WARM_ACCESS 100; got gas_used={gas}");
}

/// EIP-8038 §"EXT* family update": EXTCODESIZE/EXTCODECOPY are charged an extra
/// `WARM_ACCESS` for the second database read. revm charges a single access, so
/// the `+100` rides the inspector — present on the inspected path, absent without.
#[test]
fn cold_extcodesize_adds_second_read_via_inspector() {
    let (inspected, _) = run_eip8038_inspected(&one_access(PROBE, 0x3B), &[]);
    let bare = run_eip8038(&one_access(PROBE, 0x3B), &[]);
    assert_eq!(
        bare - ONE_ACCESS_OVERHEAD,
        3_000,
        "EXTCODESIZE gas_params charge = COLD_ACCOUNT_ACCESS 3000 (no second read); got {bare}"
    );
    assert_eq!(
        inspected - ONE_ACCESS_OVERHEAD,
        3_000 + 100,
        "EXTCODESIZE inspected = 3000 + second-read 100 = 3100; got {inspected}"
    );
}

// --- Value transfer / CREATE: baseline-vs-8038 delta isolates the slot ----------

/// The net cost a value-transfer CALL adds, when the callee (an EOA) leaves the
/// 2300 stipend unused, is `ACCOUNT_WRITE = 8000`: revm charges the caller
/// `CALL_VALUE = transfer_value_cost (10300) = ACCOUNT_WRITE + CALL_STIPEND`, and
/// the unused stipend returns to the caller (10300 − 2300 = 8000). The slot value
/// itself (10300) is asserted by the schedule's `configure_evm_env` unit test;
/// here we confirm the meaningful net charge composes to `ACCOUNT_WRITE`.
/// Isolated as the delta between a value=1 and value=0 CALL to the same EOA.
#[test]
fn value_transfer_call_net_cost_is_account_write() {
    let with_value = run_eip8038(&call_with_value(PROBE, 1), &[(PROBE, eoa_account())]);
    let no_value = run_eip8038(&call_with_value(PROBE, 0), &[(PROBE, eoa_account())]);
    assert_eq!(
        with_value - no_value,
        8_000,
        "net value-transfer cost = ACCOUNT_WRITE 8000 (CALL_VALUE 10300 − stipend 2300); \
         with={with_value} without={no_value}"
    );
}

/// The CREATE opcode is repriced from native `GAS_CREATE` (32000) to
/// `CREATE_ACCESS` (11000). Isolated as the baseline-minus-8038 delta over an
/// empty-initcode CREATE (the inner frame and everything else cancel).
#[test]
fn create_opcode_reprices_to_create_access() {
    // PUSH1 0 (size), PUSH1 0 (offset), PUSH1 0 (value), CREATE, POP, STOP.
    let code = [0x60, 0x00, 0x60, 0x00, 0x60, 0x00, 0xF0, 0x50, 0x00];
    let native = run_native(&code, &[]);
    let eip8038 = run_eip8038(&code, &[]);
    assert_eq!(
        native - eip8038,
        32_000 - 11_000,
        "CREATE 8038 cheaper by GAS_CREATE − CREATE_ACCESS = 21000; native={native} 8038={eip8038}"
    );
}

// --- Classification counter (single, uniform) -----------------------------------

#[test]
fn cold_account_access_count_is_single_and_uniform() {
    // One cold access, regardless of whether the target has code.
    let (_, to_contract) =
        run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, code_account(&[0x00]))]);
    assert_eq!(to_contract.operation_counts().cold_account_access_count, 1);

    let (_, to_eoa) = run_eip8038_inspected(&one_access(PROBE, 0x31), &[(PROBE, eoa_account())]);
    assert_eq!(to_eoa.operation_counts().cold_account_access_count, 1);

    // A warm re-access does not increment the cold counter.
    let mut code = one_access(PROBE, 0x31);
    code.pop();
    code.push(0x50);
    code.extend_from_slice(&one_access(PROBE, 0x31));
    let (_, warm_reaccess) = run_eip8038_inspected(&code, &[(PROBE, eoa_account())]);
    assert_eq!(
        warm_reaccess.operation_counts().cold_account_access_count,
        1,
        "only the first (cold) access counts"
    );
}

/// F8: the storage-reprice driver counters populate from a real execution
/// covering a cold fresh-set, a same-tx dirty re-write, and a cold + a warm SLOAD.
#[test]
fn storage_reprice_drivers_counted() {
    let code = [
        0x60, 0x01, 0x60, 0x00, 0x55, // SSTORE slot0 = 1 (cold, set)
        0x60, 0x02, 0x60, 0x00, 0x55, // SSTORE slot0 = 2 (warm, dirty)
        0x60, 0x00, 0x54, 0x50, // SLOAD slot0 (warm), POP
        0x60, 0x01, 0x54, 0x50, // SLOAD slot1 (cold), POP
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
