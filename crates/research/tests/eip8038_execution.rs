//! End-to-end gas validation for the EIP-8038 (misilva73) schedule.
//!
//! Runs real transactions in an in-memory `CacheDB` (no reth datadir is touched)
//! under the schedule's `gas_params` override and asserts the exact gas charged.
//! This validates the error-prone Layer-1 back-solve (warm base + cold add-on +
//! write surcharge), which the schedule's unit tests only confirm at the slot
//! level — here revm actually charges it.

use reth_research::schedule::apply_eip8038_gas_overrides;
use revm::{
    bytecode::Bytecode,
    context::{Context, TxEnv},
    database::CacheDB,
    database_interface::EmptyDB,
    primitives::{hardfork::SpecId, keccak256, Address, Bytes, TxKind, U256},
    state::AccountInfo,
    ExecuteEvm, MainBuilder, MainContext,
};

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
