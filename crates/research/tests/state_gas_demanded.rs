//! Guards the EIP-8037 `state_gas_demanded` accounting added to the vendored
//! revm (frame-local counter that survives OOG and reverts).
//!
//! The dashboard relies on `result.gas().state_gas_demanded()` being nonzero
//! when a state-creating op (e.g. an SSTORE to a new slot) ran out of gas
//! before its charge landed — otherwise the tx misleadingly reads "0 state
//! gas needed". These tests pin the chain:
//!
//!   1. `Gas::record_state_cost` — the function every state charge funnels
//!      through (SSTORE / account-create via the `state_gas!` macro; code
//!      deposit directly). It must count the attempt even when it OOGs.
//!   2. `build_result_gas` — the sole builder of the final tx `ResultGas`,
//!      must copy demanded onto it.
//!   3. End-to-end: a real SSTORE-to-a-new-slot, executed in a *sub-call*
//!      under the Amsterdam (EIP-8037) spec, must surface demanded on the
//!      top-level transaction result — which also exercises the
//!      child→parent propagation in `handle_reservoir_remaining_gas`.

use revm::context_interface::cfg::gas::InitialAndFloorGas;
use revm::handler::post_execution::build_result_gas;
use revm::interpreter::Gas;

/// 1. A charge that can't be afforded still counts toward demanded, while
///    state_gas_spent only moves when the charge actually lands.
#[test]
fn record_state_cost_counts_demand_even_on_oog() {
    // regular gas = 0, reservoir = 1000: state charges draw from the reservoir.
    let mut gas = Gas::new_with_regular_gas_and_reservoir(0, 1000);

    assert!(gas.record_state_cost(200));
    assert_eq!(gas.state_gas_spent(), 200);
    assert_eq!(gas.state_gas_demanded(), 200);

    // Unaffordable (reservoir 800, no regular gas to spill into) → OOG.
    // spent must NOT move; demanded must still record the attempt.
    assert!(!gas.record_state_cost(5000));
    assert_eq!(gas.state_gas_spent(), 200, "spent unchanged on OOG");
    assert_eq!(gas.state_gas_demanded(), 5200, "attempt counted despite OOG");
}

/// 2. The final ResultGas the tx exposes carries demanded.
#[test]
fn build_result_gas_surfaces_demanded() {
    let mut gas = Gas::new_with_regular_gas_and_reservoir(1_000_000, 0);
    assert!(gas.record_state_cost(500));

    let rg = build_result_gas(&gas, InitialAndFloorGas::new(0, 0));
    assert_eq!(rg.state_gas_demanded(), 500);
}

// ── 3. End-to-end: SSTORE-to-new-slot in a sub-call, Amsterdam spec ──

mod evm {
    use revm::bytecode::Bytecode;
    use revm::context::{Context, TxEnv};
    use revm::database::CacheDB;
    use revm::database_interface::EmptyDB;
    use revm::primitives::{hardfork::SpecId, Address, Bytes, TxKind, U256};
    use revm::state::AccountInfo;
    use revm::{ExecuteEvm, MainBuilder, MainContext};

    /// PUSH1 0x01, PUSH1 0x00, SSTORE, STOP — writes a fresh slot (0 → 1).
    const SSTORE_NEW_SLOT: [u8; 6] = [0x60, 0x01, 0x60, 0x00, 0x55, 0x00];

    /// Bytecode for a proxy that CALLs `callee` forwarding all gas, then STOP.
    /// Puts the SSTORE in a child frame so the result depends on child→parent
    /// propagation of the demanded counter.
    fn proxy_calling(callee: Address) -> Vec<u8> {
        let mut code = Vec::new();
        // CALL pops gas, addr, value, argsOffset, argsSize, retOffset, retSize
        // → push them in reverse so `gas` ends up on top.
        for _ in 0..5 {
            code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0 (retSize,retOffset,argsSize,argsOffset,value)
        }
        code.push(0x73); // PUSH20 callee
        code.extend_from_slice(callee.as_slice());
        code.push(0x5a); // GAS
        code.push(0xf1); // CALL
        code.push(0x00); // STOP
        code
    }

    fn acct(code: &[u8], balance: U256) -> AccountInfo {
        AccountInfo {
            balance,
            nonce: 0,
            code_hash: revm::primitives::keccak256(code),
            code: Some(Bytecode::new_raw(Bytes::copy_from_slice(code))),
            ..Default::default()
        }
    }

    #[test]
    fn sstore_new_slot_in_subcall_reports_state_gas_demanded() {
        let sstore_contract = Address::from([0x11; 20]);
        let proxy = Address::from([0x33; 20]);
        let caller = Address::from([0x22; 20]);

        let mut db = CacheDB::<EmptyDB>::default();
        db.insert_account_info(sstore_contract, acct(&SSTORE_NEW_SLOT, U256::ZERO));
        let proxy_code = proxy_calling(sstore_contract);
        db.insert_account_info(proxy, acct(&proxy_code, U256::ZERO));
        db.insert_account_info(
            caller,
            AccountInfo { balance: U256::from(1u64) << 100, ..Default::default() },
        );

        // Amsterdam *spec* alone leaves cfg.gas_params on the pre-Amsterdam
        // table, so SSTORE bills as regular gas and no state gas is charged.
        // `with_spec_and_mainnet_gas_params` sets both — the same call the
        // research EIP-8037 schedule uses to turn on native state-gas
        // accounting.
        let mut evm = Context::mainnet()
            .with_db(db)
            .modify_cfg_chained(|cfg| {
                *cfg = cfg.clone().with_spec_and_mainnet_gas_params(SpecId::AMSTERDAM);
            })
            .build_mainnet();

        let tx = TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(proxy))
            .gas_limit(2_000_000)
            .gas_price(0)
            .build()
            .expect("valid tx env");

        let out = evm.transact(tx).expect("execution completes");
        let gas = out.result.gas();
        assert!(
            gas.state_gas_demanded() > 0,
            "a new-slot SSTORE in a sub-call must report state gas demanded on the \
             tx result; got spent={} demanded={}",
            gas.state_gas_spent(),
            gas.state_gas_demanded(),
        );
    }
}
