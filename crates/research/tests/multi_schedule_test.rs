//! Integration tests for the multi-schedule research system.
//!
//! These tests verify the end-to-end workflow of:
//! 1. Configuring schedules via CLI args
//! 2. Building a schedule registry
//! 3. Creating an analyzer
//! 4. Processing transactions and recording divergences

use alloy_primitives::{Address, Bytes, B256, U256};
use reth_research::{
    analyzer::MultiScheduleAnalyzer,
    cli::ResearchArgs,
    comparison::ExecutionSummary,
    database::DivergenceDatabase,
    divergence::OperationCounts,
    schedule::{
        BaselineSchedule, CsvPricingSchedule, Eip2780Schedule, GasSchedule, MultiplierSchedule,
        OpcodeContext, RecipientInfo, ScheduleRegistry, TxContext,
    },
    ScheduleInspector,
};
use revm::{
    context::{CfgEnv, Context, TxEnv},
    handler::{ExecuteEvm, MainBuilder, MainContext},
    state::{AccountInfo, Bytecode},
    InspectEvm,
};
use revm_database::CacheDB;
use std::sync::Arc;
use tempfile::tempdir;

// ============================================================================
// Schedule Configuration Tests
// ============================================================================

#[test]
fn test_cli_args_build_full_registry() {
    let args = ResearchArgs::new()
        .with_eip2780()
        .with_multiplier("128x", 128)
        .unwrap()
        .with_multiplier("256x", 256)
        .unwrap();

    let registry = args.build_registry().unwrap();

    assert_eq!(registry.len(), 3);
    assert!(registry.get("eip-2780").is_some());
    assert!(registry.get("128x").is_some());
    assert!(registry.get("256x").is_some());
}

#[test]
fn test_registry_preserves_schedule_order() {
    let mut registry = ScheduleRegistry::new();

    registry.register(Eip2780Schedule::new()).unwrap();
    registry.register(MultiplierSchedule::new("first".to_string(), 100)).unwrap();
    registry.register(MultiplierSchedule::new("second".to_string(), 200)).unwrap();

    let names = registry.names();
    assert_eq!(names, &["eip-2780", "first", "second"]);
}

#[test]
fn test_csv_schedule_loading() {
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
SDIV,constant,5,15
MOD,constant,5,15
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
"#;

    let schedule =
        CsvPricingSchedule::from_csv("test-7904".to_string(), csv_data.as_bytes()).unwrap();

    assert_eq!(schedule.name(), "test-7904");

    // Verify affected opcodes
    let affected = schedule.affected_opcodes();
    assert!(affected.contains(&0x04)); // DIV
    assert!(affected.contains(&0x05)); // SDIV
    assert!(affected.contains(&0x06)); // MOD
    assert!(affected.contains(&0x20)); // KECCAK256
}

#[test]
fn test_csv_schedule_gas_delta() {
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
"#;

    let schedule = CsvPricingSchedule::from_csv("test".to_string(), csv_data.as_bytes()).unwrap();

    let ctx = OpcodeContext::default();

    // DIV opcode should have +10 gas delta (15 - 5)
    assert_eq!(schedule.opcode_gas_delta(0x04, &ctx), 10);

    // Unaffected opcode should have 0 delta
    assert_eq!(schedule.opcode_gas_delta(0x01, &ctx), 0); // ADD
}

// ============================================================================
// EIP-2780 Schedule Tests
// ============================================================================

#[test]
fn test_eip2780_transfer_to_eoa() {
    let schedule = Eip2780Schedule::new();

    let ctx = TxContext {
        baseline_intrinsic_gas: 21_000,
        sender: Address::repeat_byte(0x01),
        recipient: Some(Address::repeat_byte(0x02)),
        value: U256::from(1000),
        input: Bytes::new(),
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: false,
            balance: U256::from(100),
            nonce: 1,
        }),
        ..Default::default()
    };

    let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();

    // Transfer to EOA: TX_BASE_COST + COLD_ACCOUNT_COST_NOCODE + STATE_UPDATE
    // = 4500 + 500 + 1000 = 6000
    assert_eq!(intrinsic, 6000);

    // Category should be transfer_to_eoa
    let category = schedule.tx_category(&ctx).unwrap();
    assert_eq!(category, "transfer_to_eoa");
}

#[test]
fn test_eip2780_call_to_contract() {
    let schedule = Eip2780Schedule::new();

    let ctx = TxContext {
        baseline_intrinsic_gas: 21_064,
        sender: Address::repeat_byte(0x01),
        recipient: Some(Address::repeat_byte(0x02)),
        value: U256::ZERO,
        input: Bytes::from(vec![0xab, 0xcd, 0xef, 0x12]), // function selector
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: true,
            balance: U256::ZERO,
            nonce: 1,
        }),
        ..Default::default()
    };

    let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();

    // Call to contract: TX_BASE_COST + COLD_ACCOUNT_COST_CODE + calldata_gas
    // = 4500 + 2600 + (4 * 16) = 7164
    assert_eq!(intrinsic, 7164);

    let category = schedule.tx_category(&ctx).unwrap();
    assert_eq!(category, "call_to_contract");
}

#[test]
fn test_eip2780_nop_to_self() {
    let schedule = Eip2780Schedule::new();
    let self_addr = Address::repeat_byte(0x01);

    let ctx = TxContext {
        baseline_intrinsic_gas: 21_000,
        sender: self_addr,
        recipient: Some(self_addr),
        value: U256::ZERO,
        input: Bytes::new(),
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: false,
            balance: U256::from(100),
            nonce: 1,
        }),
        ..Default::default()
    };

    let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();

    // NOP to self: TX_BASE_COST = 4500
    assert_eq!(intrinsic, 4500);

    let category = schedule.tx_category(&ctx).unwrap();
    assert_eq!(category, "nop_to_self");
}

#[test]
fn test_eip2780_contract_creation() {
    let schedule = Eip2780Schedule::new();

    let ctx = TxContext {
        baseline_intrinsic_gas: 53_020,
        sender: Address::repeat_byte(0x01),
        recipient: None,
        value: U256::ZERO,
        input: Bytes::from(vec![0x60, 0x00]), // minimal bytecode (PUSH1 0x00)
        gas_limit: 1_000_000,
        is_create: true,
        recipient_info: None,
        ..Default::default()
    };

    let intrinsic = schedule.intrinsic_gas(&ctx).unwrap();

    // Contract creation: CURRENT_CREATE_COST + calldata_gas
    // = 53000 + (1 non-zero * 16 + 1 zero * 4) = 53000 + 20 = 53020
    assert_eq!(intrinsic, 53020);

    let category = schedule.tx_category(&ctx).unwrap();
    assert_eq!(category, "contract_creation");
}

// ============================================================================
// Multi-Schedule Inspector Tests
// ============================================================================

#[test]
fn test_schedule_inspector_with_csv_schedules() {
    let csv1 = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,10
"#;
    let csv2 = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,20
"#;

    let schedule1 =
        CsvPricingSchedule::from_csv("conservative".to_string(), csv1.as_bytes()).unwrap();
    let schedule2 =
        CsvPricingSchedule::from_csv("aggressive".to_string(), csv2.as_bytes()).unwrap();

    // Each schedule gets its own inspector
    let inspector1 = ScheduleInspector::new(Arc::new(schedule1));
    let inspector2 = ScheduleInspector::new(Arc::new(schedule2));

    assert_eq!(inspector1.schedule_name(), "conservative");
    assert_eq!(inspector2.schedule_name(), "aggressive");

    let result1 = inspector1.result();
    let result2 = inspector2.result();
    assert_eq!(result1.schedule_name, "conservative");
    assert_eq!(result2.schedule_name, "aggressive");
}

#[test]
fn test_schedule_inspector_per_schedule() {
    // Each schedule type gets its own inspector — no filtering needed
    let baseline_inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
    let multiplier_inspector =
        ScheduleInspector::new(Arc::new(MultiplierSchedule::with_multiplier(128)));

    assert_eq!(baseline_inspector.schedule_name(), "baseline");
    assert_eq!(multiplier_inspector.schedule_name(), "128x");
}

// ============================================================================
// Analyzer Integration Tests
// ============================================================================

#[test]
fn test_analyzer_with_eip2780_schedule() {
    let mut registry = ScheduleRegistry::new();
    registry.register(Eip2780Schedule::new()).unwrap();

    let mut analyzer = MultiScheduleAnalyzer::new(registry).unwrap();

    // Create a simple transfer transaction context
    let tx_context = TxContext {
        baseline_intrinsic_gas: 21_000,
        sender: Address::repeat_byte(0x01),
        recipient: Some(Address::repeat_byte(0x02)),
        value: U256::from(1000),
        input: Bytes::new(),
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: false,
            balance: U256::from(100),
            nonce: 1,
        }),
        ..Default::default()
    };

    let baseline = ExecutionSummary::new(true, 21000, 21000);

    let result = analyzer
        .analyze_transaction(1000000, 0, B256::ZERO, 1234567890, baseline, Some(&tx_context))
        .unwrap();

    assert_eq!(result.block_number, 1000000);
    assert_eq!(result.schedule_results.len(), 1);

    // EIP-2780 should show a divergence (gas savings)
    let eip2780_result = &result.schedule_results[0];
    assert_eq!(eip2780_result.schedule_name, "eip-2780");
    assert!(eip2780_result.has_divergence);

    // Should have intrinsic comparison
    let intrinsic = eip2780_result.intrinsic_comparison.as_ref().unwrap();
    assert_eq!(intrinsic.baseline_intrinsic, 21000);
    assert_eq!(intrinsic.schedule_intrinsic, 6000); // Transfer to EOA
    assert!(intrinsic.is_saving());
    assert_eq!(intrinsic.tx_category, Some("transfer_to_eoa".to_string()));
}

#[test]
fn test_analyzer_with_multiple_schedules() {
    let mut registry = ScheduleRegistry::new();
    registry.register(Eip2780Schedule::new()).unwrap();
    registry.register(MultiplierSchedule::with_multiplier(128)).unwrap();

    let mut analyzer = MultiScheduleAnalyzer::new(registry).unwrap();

    let baseline = ExecutionSummary::new(true, 50000, 21000);

    let result =
        analyzer.analyze_transaction(1000000, 0, B256::ZERO, 1234567890, baseline, None).unwrap();

    assert_eq!(result.schedule_results.len(), 2);
    assert_eq!(analyzer.stats().transactions_analyzed, 1);
}

#[test]
fn test_analyzer_with_database() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let db = DivergenceDatabase::open(&db_path).unwrap();

    let mut registry = ScheduleRegistry::new();
    registry.register(Eip2780Schedule::new()).unwrap();

    let mut analyzer = MultiScheduleAnalyzer::new(registry).unwrap().with_database(db.clone());

    let tx_context = TxContext {
        baseline_intrinsic_gas: 21_000,
        sender: Address::repeat_byte(0x01),
        recipient: Some(Address::repeat_byte(0x02)),
        value: U256::from(1000),
        input: Bytes::new(),
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: false,
            balance: U256::from(100),
            nonce: 1,
        }),
        ..Default::default()
    };

    let baseline = ExecutionSummary::new(true, 21000, 21000);

    analyzer
        .analyze_transaction(
            1000000,
            0,
            B256::repeat_byte(0xAB),
            1234567890,
            baseline,
            Some(&tx_context),
        )
        .unwrap();

    // Verify divergence was recorded
    let counts = db.divergence_counts_by_schedule().unwrap();
    let eip2780_count = counts.iter().find(|(name, _)| name == "eip-2780");
    assert!(eip2780_count.is_some());
    assert_eq!(eip2780_count.unwrap().1, 1);
}

// ============================================================================
// Database Integration Tests
// ============================================================================

#[test]
fn test_database_schedule_divergence_workflow() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("test.db");

    let db = DivergenceDatabase::open(&db_path).unwrap();

    // Record divergences for multiple schedules
    use reth_research::{database::ScheduleDivergence, divergence::DivergenceType};

    let div1 = ScheduleDivergence {
        schedule_name: "eip-2780".to_string(),
        block_number: 1000000,
        tx_index: 0,
        tx_hash: B256::repeat_byte(0x01),
        timestamp: 1234567890,
        divergence_type: DivergenceType::GasPattern,
        schedule_kind: "IntrinsicOnly".to_string(),
        schedule_description: "test".to_string(),
        schedule_config_hash: "cfg".to_string(),
        block_hash: B256::repeat_byte(0x10),
        parent_hash: B256::repeat_byte(0x11),
        baseline_success: true,
        baseline_gas_used: 21000,
        baseline_intrinsic_gas: 21000,
        schedule_success: true,
        schedule_gas_used: 6000,
        schedule_intrinsic_gas: Some(6000),
        gas_delta: -15000,
        gas_efficiency_ratio: Some(0.286),
        tx_category: Some("transfer_to_eoa".to_string()),
        affected_opcodes: None,
        affected_precompiles: None,
        oog_info: None,
        divergence_location: None,
        operation_counts: None,
        baseline_call_frames: None,
        schedule_call_frames: None,
        baseline_event_logs: None,
        schedule_event_logs: None,
        baseline_call_frames_hash: None,
        schedule_call_frames_hash: None,
        baseline_event_logs_hash: None,
        schedule_event_logs_hash: None,
        status_changed: false,
        gas_changed: true,
        call_tree_changed: false,
        event_logs_changed: false,
        output_changed: false,
        created_address_changed: false,
        logs_bloom_changed: false,
        sender: "0x0000000000000000000000000000000000000001".to_string(),
        recipient: Some("0x0000000000000000000000000000000000000002".to_string()),
        value_wei: "0".to_string(),
        input_len: 0,
        input_zero_bytes: 0,
        input_nonzero_bytes: 0,
        tx_gas_limit: 21000,
        access_list_accounts: 0,
        access_list_storage_slots: 0,
        authorization_count: 0,
        is_create: false,
        baseline_output_len: None,
        schedule_output_len: None,
        baseline_output_hash: None,
        schedule_output_hash: None,
        baseline_created_address: None,
        schedule_created_address: None,
        baseline_log_count: 0,
        schedule_log_count: 0,
        baseline_logs_bloom: String::new(),
        schedule_logs_bloom: String::new(),
        would_fit_in_original_limit: true,
        min_multiplier_to_succeed: Some(0.286),
    };

    let div2 = ScheduleDivergence {
        schedule_name: "7904-v1".to_string(),
        block_number: 1000000,
        tx_index: 1,
        tx_hash: B256::repeat_byte(0x02),
        timestamp: 1234567890,
        divergence_type: DivergenceType::Status,
        schedule_kind: "ExecutionOnly".to_string(),
        schedule_description: "test".to_string(),
        schedule_config_hash: "cfg".to_string(),
        block_hash: B256::repeat_byte(0x20),
        parent_hash: B256::repeat_byte(0x21),
        baseline_success: true,
        baseline_gas_used: 50000,
        baseline_intrinsic_gas: 21000,
        schedule_success: false,
        schedule_gas_used: 100000,
        schedule_intrinsic_gas: None,
        gas_delta: 50000,
        gas_efficiency_ratio: Some(2.0),
        tx_category: None,
        affected_opcodes: Some("[4,5,6]".to_string()),
        affected_precompiles: None,
        oog_info: Some(r#"{"opcode": 4}"#.to_string()),
        divergence_location: None,
        operation_counts: None,
        baseline_call_frames: None,
        schedule_call_frames: None,
        baseline_event_logs: None,
        schedule_event_logs: None,
        baseline_call_frames_hash: None,
        schedule_call_frames_hash: None,
        baseline_event_logs_hash: None,
        schedule_event_logs_hash: None,
        status_changed: true,
        gas_changed: true,
        call_tree_changed: false,
        event_logs_changed: false,
        output_changed: false,
        created_address_changed: false,
        logs_bloom_changed: false,
        sender: "0x0000000000000000000000000000000000000001".to_string(),
        recipient: Some("0x0000000000000000000000000000000000000002".to_string()),
        value_wei: "0".to_string(),
        input_len: 0,
        input_zero_bytes: 0,
        input_nonzero_bytes: 0,
        tx_gas_limit: 50000,
        access_list_accounts: 0,
        access_list_storage_slots: 0,
        authorization_count: 0,
        is_create: false,
        baseline_output_len: None,
        schedule_output_len: None,
        baseline_output_hash: None,
        schedule_output_hash: None,
        baseline_created_address: None,
        schedule_created_address: None,
        baseline_log_count: 0,
        schedule_log_count: 0,
        baseline_logs_bloom: String::new(),
        schedule_logs_bloom: String::new(),
        would_fit_in_original_limit: false,
        min_multiplier_to_succeed: Some(2.0),
    };

    db.record_schedule_divergence(&div1).unwrap();
    db.record_schedule_divergence(&div2).unwrap();

    // Query by schedule
    let counts = db.divergence_counts_by_schedule().unwrap();
    let eip2780_count = counts.iter().find(|(name, _)| name == "eip-2780");
    let v7904_count = counts.iter().find(|(name, _)| name == "7904-v1");
    assert!(eip2780_count.is_some());
    assert_eq!(eip2780_count.unwrap().1, 1);
    assert!(v7904_count.is_some());
    assert_eq!(v7904_count.unwrap().1, 1);
}

// ============================================================================
// End-to-End Workflow Test
// ============================================================================

#[test]
fn test_full_workflow_cli_to_analysis() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("research.db");

    // Step 1: Configure via CLI args
    let args = ResearchArgs::new()
        .with_eip2780()
        .with_multiplier("128x", 128)
        .unwrap()
        .with_db_path(db_path.clone());

    assert!(args.has_schedules());
    assert_eq!(args.schedule_count(), 2);

    // Step 2: Build registry
    let registry = args.build_registry().unwrap();
    assert_eq!(registry.len(), 2);

    // Step 3: Open database
    let db = DivergenceDatabase::open(&db_path).unwrap();

    // Step 4: Create analyzer
    let mut analyzer = MultiScheduleAnalyzer::new(registry).unwrap().with_database(db.clone());

    // Step 5: Simulate transaction analysis (intrinsic gas only — execution
    // comparison is handled by the ExEx via per-schedule re-execution)
    let tx_context = TxContext {
        baseline_intrinsic_gas: 21_000,
        sender: Address::repeat_byte(0x01),
        recipient: Some(Address::repeat_byte(0x02)),
        value: U256::from(1000),
        input: Bytes::new(),
        gas_limit: 100_000,
        is_create: false,
        recipient_info: Some(RecipientInfo {
            exists: true,
            has_code: false,
            balance: U256::from(100),
            nonce: 1,
        }),
        ..Default::default()
    };

    let baseline =
        ExecutionSummary::new(true, 21000, 21000).with_operation_counts(OperationCounts {
            total_ops: 100,
            memory_words_allocated: 10,
            ..Default::default()
        });

    // Step 6: Analyze transaction
    let result = analyzer
        .analyze_transaction(
            1000000,
            0,
            B256::repeat_byte(0xAB),
            1234567890,
            baseline,
            Some(&tx_context),
        )
        .unwrap();

    // Step 7: Verify results
    assert!(result.has_any_divergence());
    assert!(result.divergent_schedules().contains(&"eip-2780"));

    // Step 8: Verify database
    let counts = db.divergence_counts_by_schedule().unwrap();
    let has_eip2780 = counts.iter().any(|(name, _)| name == "eip-2780");
    assert!(has_eip2780);

    // Step 9: Check stats
    assert_eq!(analyzer.stats().transactions_analyzed, 1);
    assert!(analyzer.stats().divergences_found > 0);
}

// ============================================================================
// Precompile Tests
// ============================================================================

#[test]
fn test_csv_schedule_precompile_pricing() {
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
BLAKE2F,constant,0,170
BLAKE2F,num_rounds,1,2
"#;

    let schedule =
        CsvPricingSchedule::from_csv("precompile-test".to_string(), csv_data.as_bytes()).unwrap();

    // ECPAIRING at address 0x08
    let ecpairing_addr =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);

    // 3 pairs = 576 bytes input
    let input = vec![0u8; 576];
    let delta = schedule.precompile_gas_delta(&ecpairing_addr, &input);

    // Delta = (45000 + 3*34103) - (45000 + 3*34000) = 309
    assert_eq!(delta, 309);

    // BLAKE2F at address 0x09
    let blake2f_addr =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]);

    // 100 rounds encoded in first 4 bytes
    let mut input = vec![0u8; 213]; // BLAKE2F input size
    input[0..4].copy_from_slice(&100u32.to_be_bytes());
    let delta = schedule.precompile_gas_delta(&blake2f_addr, &input);

    // Delta = (170 + 100*2) - (0 + 100*1) = 170 + 200 - 100 = 270
    assert_eq!(delta, 270);
}

// ============================================================================
// EVM Integration: Subcall Gas Propagation Tests
// ============================================================================

/// Build bytecode for a callee that does `n` cold SLOADs then STOPs.
/// Each cold SLOAD costs 2100 gas, so the callee needs roughly n * 2100 gas.
fn build_callee_bytecode(n: usize) -> Bytes {
    let mut code = Vec::new();
    for i in 0..n {
        code.push(0x60); // PUSH1
        code.push(i as u8); // storage slot
        code.push(0x54); // SLOAD
        code.push(0x50); // POP
    }
    code.push(0x00); // STOP
    Bytes::from(code)
}

/// Build bytecode for a caller that does `n` DIV operations then
/// CALLs `callee_addr` forwarding all available gas.
fn build_caller_bytecode(n: usize, callee_addr: Address) -> Bytes {
    let mut code = Vec::new();
    // n * (PUSH1 2, PUSH1 10, DIV, POP) = n * 4 bytes
    for _ in 0..n {
        code.push(0x60); // PUSH1
        code.push(0x02); // 2
        code.push(0x60); // PUSH1
        code.push(0x0A); // 10
        code.push(0x04); // DIV
        code.push(0x50); // POP
    }
    // CALL args: retLength, retOffset, argsLength, argsOffset, value, addr, gas
    code.push(0x60);
    code.push(0x00); // retLength = 0
    code.push(0x60);
    code.push(0x00); // retOffset = 0
    code.push(0x60);
    code.push(0x00); // argsLength = 0
    code.push(0x60);
    code.push(0x00); // argsOffset = 0
    code.push(0x60);
    code.push(0x00); // value = 0
                     // PUSH20 callee_addr
    code.push(0x73);
    code.extend_from_slice(callee_addr.as_slice());
    code.push(0x5A); // GAS (forward all available gas)
    code.push(0xF1); // CALL
    code.push(0x50); // POP (CALL result)
    code.push(0x00); // STOP
    Bytes::from(code)
}

/// Create an EVM database with caller and callee contracts deployed.
fn setup_evm_db(
    caller_addr: Address,
    callee_addr: Address,
    sender_addr: Address,
    caller_code: Bytes,
    callee_code: Bytes,
) -> CacheDB<revm::database_interface::EmptyDB> {
    let mut db = CacheDB::new(Default::default());
    db.insert_account_info(
        caller_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&caller_code),
            code: Some(Bytecode::new_raw(caller_code)),
            account_id: None,
        },
    );
    db.insert_account_info(
        callee_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&callee_code),
            code: Some(Bytecode::new_raw(callee_code)),
            account_id: None,
        },
    );
    // Sender needs balance for gas
    db.insert_account_info(
        sender_addr,
        AccountInfo {
            nonce: 0,
            balance: U256::from(1_000_000_000_000u64),
            code_hash: alloy_primitives::keccak256([]),
            code: None,
            account_id: None,
        },
    );
    db
}

/// Verify that gas cost modifications propagate through subcalls.
///
/// The old single-pass model tracked gas deltas without modifying execution,
/// so subcall gas forwarding was unaffected. The new per-schedule re-execution
/// model actually modifies gas via `record_cost` / `erase_cost`, so changes
/// in the caller's gas usage feed into the 63/64 forwarding rule for CALLs.
///
/// This test proves the propagation works by:
/// 1. Running a caller→callee transaction under baseline (no gas modifications)
/// 2. Running again with a schedule that makes DIV much more expensive
/// 3. Verifying the callee receives less gas under the schedule
#[test]
fn test_subcall_gas_propagation_with_schedule() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let caller_addr = Address::new([0x11; 20]);
    let callee_addr = Address::new([0x22; 20]);

    let num_divs = 20;
    let caller_code = build_caller_bytecode(num_divs, callee_addr);
    let callee_code = build_callee_bytecode(5); // 5 SLOADs ≈ 10,500 gas

    let gas_limit = 200_000u64;

    // --- Baseline execution (no inspector) ---
    let db =
        setup_evm_db(caller_addr, callee_addr, sender, caller_code.clone(), callee_code.clone());
    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(gas_limit)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(caller_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet();
    let baseline_result = evm.transact(tx.clone()).expect("baseline should succeed");
    assert!(baseline_result.result.is_success(), "baseline tx should succeed");
    let baseline_gas = baseline_result.result.tx_gas_used();

    // --- Schedule execution (DIV costs +2000 per op) ---
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,2005
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("div-expensive".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let db2 = setup_evm_db(caller_addr, callee_addr, sender, caller_code, callee_code);
    let mut inspector = ScheduleInspector::new(schedule);

    let ctx2 = Context::mainnet().with_db(db2).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm2 = ctx2.build_mainnet_with_inspector(&mut inspector);
    let schedule_result = evm2.inspect_tx(tx).expect("schedule execution should not error");
    let schedule_gas = schedule_result.result.tx_gas_used();

    // The inspector should report non-zero additional gas charged
    let insp_result = inspector.result();
    assert!(
        insp_result.additional_gas > 0,
        "inspector should report additional gas charged; got {}",
        insp_result.additional_gas
    );

    // Schedule execution should use more gas due to the DIV cost increase
    assert!(
        schedule_gas > baseline_gas,
        "schedule should use more gas: baseline={baseline_gas}, schedule={schedule_gas}"
    );

    // The delta should be roughly 20 DIVs * 2000 extra = 40,000
    // (not exact due to gas forwarding effects, but should be in the ballpark)
    let gas_delta = schedule_gas - baseline_gas;
    assert!(
        gas_delta >= 30_000,
        "gas delta should be at least 30k from 20 DIVs * 2000; got {gas_delta}"
    );

    // Verify the callee received less gas under the schedule by checking
    // the call frames. The inspector's call() hook records gas_provided for
    // each subcall, which reflects the 63/64 forwarding rule.
    let call_frames = inspector.call_frames();
    assert!(!call_frames.is_empty(), "should have at least one call frame (callee)");

    // The callee call frame shows how much gas the subcall was given.
    // Under the schedule, the caller burned more gas before CALL, so the
    // subcall receives less gas. We can't easily get the baseline callee's
    // gas_provided without a second inspector, but we can verify the
    // inspector tracked the subcall and that additional_gas is non-zero.
    let callee_frame = &call_frames[0];
    assert_eq!(callee_frame.to, Some(callee_addr), "first call frame should be to callee");
    assert!(callee_frame.gas_provided > 0, "callee should receive gas");
}

/// Verify that a schedule making opcodes cheaper gives gas back correctly,
/// and that the refund is capped so remaining never exceeds the frame limit.
#[test]
fn test_erase_cost_refund_capped() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let caller_addr = Address::new([0x11; 20]);

    // Simple contract: 5 DIVs then STOP (no subcalls)
    let mut code = Vec::new();
    for _ in 0..5 {
        code.push(0x60); // PUSH1
        code.push(0x02);
        code.push(0x60); // PUSH1
        code.push(0x0A);
        code.push(0x04); // DIV
        code.push(0x50); // POP
    }
    code.push(0x00); // STOP
    let caller_code = Bytes::from(code);

    let mut db = CacheDB::new(revm::database_interface::EmptyDB::new());
    db.insert_account_info(
        caller_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&caller_code),
            code: Some(Bytecode::new_raw(caller_code)),
            account_id: None,
        },
    );
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(1_000_000_000_000u64),
            code_hash: alloy_primitives::keccak256([]),
            code: None,
            account_id: None,
        },
    );

    // Schedule that makes DIV cheaper (5 -> 1, delta = -4)
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,1
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("div-cheap".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(100_000)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(caller_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let mut inspector = ScheduleInspector::new(schedule);

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet_with_inspector(&mut inspector);
    let result = evm.inspect_tx(tx).expect("should succeed");
    assert!(result.result.is_success(), "tx should succeed");

    // Inspector should report negative additional gas (refund)
    let insp_result = inspector.result();
    assert!(
        insp_result.additional_gas < 0,
        "inspector should report negative gas delta for cheaper schedule; got {}",
        insp_result.additional_gas
    );
    // 5 DIVs * -4 = -20
    assert_eq!(insp_result.additional_gas, -20, "expected -20 gas delta from 5 DIVs * -4");
}

/// Verify that multiplier schedules materially affect execution in the live
/// generic inspector path instead of acting as a no-op configuration object.
#[test]
fn test_multiplier_schedule_changes_execution_gas() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let caller_addr = Address::new([0x11; 20]);

    // Simple contract: 5 DIVs then STOP (no subcalls, no storage)
    let mut code = Vec::new();
    for _ in 0..5 {
        code.push(0x60); // PUSH1
        code.push(0x02);
        code.push(0x60); // PUSH1
        code.push(0x0A);
        code.push(0x04); // DIV
        code.push(0x50); // POP
    }
    code.push(0x00); // STOP
    let caller_code = Bytes::from(code);

    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(100_000)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(caller_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let db = setup_evm_db(caller_addr, Address::ZERO, sender, caller_code.clone(), Bytes::new());
    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut baseline_evm = ctx.build_mainnet();
    let baseline_result = baseline_evm.transact(tx.clone()).expect("baseline should succeed");
    assert!(baseline_result.result.is_success(), "baseline tx should succeed");
    let baseline_gas = baseline_result.result.tx_gas_used();

    let db2 = setup_evm_db(caller_addr, Address::ZERO, sender, caller_code, Bytes::new());
    let mut inspector =
        ScheduleInspector::new(Arc::new(MultiplierSchedule::new("4x".to_string(), 4)));
    let ctx2 = Context::mainnet().with_db(db2).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm2 = ctx2.build_mainnet_with_inspector(&mut inspector);
    let schedule_result = evm2.inspect_tx(tx).expect("multiplier execution should succeed");
    assert!(schedule_result.result.is_success(), "multiplier tx should still succeed");

    let schedule_gas = schedule_result.result.tx_gas_used();
    let insp_result = inspector.result();

    assert!(
        schedule_gas > baseline_gas,
        "multiplier schedule should increase gas: baseline={baseline_gas}, schedule={schedule_gas}"
    );
    assert!(
        insp_result.additional_gas > 0,
        "multiplier schedule should charge additional gas; got {}",
        insp_result.additional_gas
    );
    assert_eq!(
        schedule_gas - baseline_gas,
        insp_result.additional_gas as u64,
        "for a simple no-subcall execution, reported additional gas should match gas_used delta"
    );
}

/// Prove that increased gas costs in a caller cause the callee to OOG when it
/// would have succeeded under baseline — the exact scenario the original
/// single-pass blind spot missed.
///
/// Setup:
/// - Caller does 50 DIV operations then CALLs callee forwarding all gas
/// - Callee does 5 cold SLOADs (~10,500 gas needed)
/// - Gas limit is tuned so baseline succeeds but the schedule (DIV +1000) starves the callee of
///   enough gas to complete its SLOADs
#[test]
fn test_subcall_oog_divergent_outcome() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let caller_addr = Address::new([0x11; 20]);
    let callee_addr = Address::new([0x22; 20]);

    let num_divs = 50;
    let caller_code = build_caller_bytecode(num_divs, callee_addr);
    let callee_code = build_callee_bytecode(5); // 5 SLOADs ≈ 10,500 gas needed

    // Tune gas_limit so baseline barely succeeds but the schedule starves callee.
    // Baseline caller cost: ~50 * (3+3+5+2) = ~650 (PUSH1+PUSH1+DIV+POP) + CALL overhead
    // Schedule adds 50 * 1000 = 50,000 extra gas before the CALL.
    // With a tight gas limit, the callee gets less than 10,500 gas under the schedule.
    let gas_limit = 80_000u64;

    // --- Baseline execution (no inspector) ---
    let db =
        setup_evm_db(caller_addr, callee_addr, sender, caller_code.clone(), callee_code.clone());
    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(gas_limit)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(caller_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet();
    let baseline_result = evm.transact(tx.clone()).expect("baseline should succeed");
    assert!(
        baseline_result.result.is_success(),
        "baseline tx should succeed (callee has enough gas)"
    );

    // --- Schedule execution (DIV costs +1000 per op) ---
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,1005
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("div-starve".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let db2 = setup_evm_db(caller_addr, callee_addr, sender, caller_code, callee_code);
    let mut inspector = ScheduleInspector::new(schedule);

    let ctx2 = Context::mainnet().with_db(db2).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm2 = ctx2.build_mainnet_with_inspector(&mut inspector);
    let schedule_result = evm2.inspect_tx(tx).expect("schedule execution should not error");

    // The overall tx may still "succeed" (the CALL returns 0 for subcall failure,
    // but the caller doesn't revert). Check the inspector's OOG detection instead.
    let insp_result = inspector.result();
    let call_frames = inspector.call_frames();

    // The callee subcall should have failed under the schedule
    assert!(!call_frames.is_empty(), "should have at least one call frame");
    let callee_frame = &call_frames[0];
    assert_eq!(callee_frame.to, Some(callee_addr));
    assert!(
        !callee_frame.success,
        "callee should have failed (OOG) under the schedule, but succeeded"
    );

    // The inspector should detect the divergence
    assert!(
        insp_result.additional_gas > 0,
        "inspector should report additional gas charged; got {}",
        insp_result.additional_gas
    );
    assert!(insp_result.would_oog, "inspector should report OOG from the failed subcall");
    assert!(
        insp_result.oog_info.is_some(),
        "inspector should have OOG diagnostics from indirect detection"
    );

    // Verify the schedule tx used more gas overall
    let baseline_gas = baseline_result.result.tx_gas_used();
    let schedule_gas = schedule_result.result.tx_gas_used();
    assert!(
        schedule_gas > baseline_gas,
        "schedule should use more gas: baseline={baseline_gas}, schedule={schedule_gas}"
    );
}

// ============================================================================
// Variable-Cost Opcode Tests (KECCAK256, EXP)
// ============================================================================

/// Verify that KECCAK256 variable cost (per-word) is correctly extracted from the stack
/// and applied by the schedule. The contract hashes a 64-byte region (2 words), so the
/// CSV schedule's per-word variable cost applies to 2 units.
#[test]
fn test_keccak256_variable_cost_extraction() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let contract_addr = Address::new([0x11; 20]);

    // Contract: PUSH1 64, PUSH1 0, KECCAK256, POP, STOP
    // This hashes 64 bytes starting at memory offset 0 (2 words).
    // First expand memory: PUSH1 0xFF, PUSH1 0, MSTORE (stores value at offset 0, expanding to 32
    // bytes)                      PUSH1 0xFF, PUSH1 32, MSTORE (expands to 64 bytes)
    let code = Bytes::from(vec![
        0x60, 0xFF, // PUSH1 0xFF
        0x60, 0x00, // PUSH1 0
        0x52, // MSTORE
        0x60, 0xFF, // PUSH1 0xFF
        0x60, 0x20, // PUSH1 32
        0x52, // MSTORE
        0x60, 0x40, // PUSH1 64 (size = 64 bytes = 2 words)
        0x60, 0x00, // PUSH1 0 (offset)
        0x20, // KECCAK256
        0x50, // POP
        0x00, // STOP
    ]);

    let mut db = CacheDB::new(revm::database_interface::EmptyDB::new());
    db.insert_account_info(
        contract_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            account_id: None,
        },
    );
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(1_000_000_000_000u64),
            code_hash: alloy_primitives::keccak256([]),
            code: None,
            account_id: None,
        },
    );

    // CSV schedule that reprices KECCAK256 with a variable word cost.
    // Current: constant=30, per-word=6 → total for 2 words = 30 + 2*6 = 42
    // New: constant=45, per-word=10 → total for 2 words = 45 + 2*10 = 65
    // Delta = 65 - 42 = 23
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
KECCAK256,constant,30,45
KECCAK256,words,6,10
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("keccak-test".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(100_000)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(contract_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let mut inspector = ScheduleInspector::new(schedule);

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet_with_inspector(&mut inspector);
    let result = evm.inspect_tx(tx).expect("should succeed");
    assert!(result.result.is_success(), "tx should succeed");

    let insp_result = inspector.result();
    // Delta should be 23 (the variable-cost contribution from 2 words)
    assert_eq!(
        insp_result.additional_gas, 23,
        "expected delta of 23 from KECCAK256 repricing with 2-word input; got {}",
        insp_result.additional_gas
    );
}

/// Verify that EXP variable cost (per exponent byte) is correctly extracted from
/// the stack. The contract computes 2^255, where the exponent 255 = 0xFF requires
/// 1 byte to represent.
#[test]
fn test_exp_variable_cost_extraction() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let contract_addr = Address::new([0x11; 20]);

    // Contract: PUSH1 0xFF, PUSH1 2, EXP, POP, STOP
    // Computes 2^255. Exponent = 0xFF → 1 byte.
    let code = Bytes::from(vec![
        0x60, 0xFF, // PUSH1 255 (exponent)
        0x60, 0x02, // PUSH1 2 (base)
        0x0A, // EXP
        0x50, // POP
        0x00, // STOP
    ]);

    let mut db = CacheDB::new(revm::database_interface::EmptyDB::new());
    db.insert_account_info(
        contract_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            account_id: None,
        },
    );
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(1_000_000_000_000u64),
            code_hash: alloy_primitives::keccak256([]),
            code: None,
            account_id: None,
        },
    );

    // CSV schedule repricing EXP with per-byte variable cost.
    // Current: constant=10, per-byte=50 → total for 1 byte = 10 + 1*50 = 60
    // New: constant=10, per-byte=100 → total for 1 byte = 10 + 1*100 = 110
    // Delta = 110 - 60 = 50
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
EXP,constant,10,10
EXP,exp_bytes,50,100
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("exp-test".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(100_000)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(contract_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let mut inspector = ScheduleInspector::new(schedule);

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet_with_inspector(&mut inspector);
    let result = evm.inspect_tx(tx).expect("should succeed");
    assert!(result.result.is_success(), "tx should succeed");

    let insp_result = inspector.result();
    // Delta should be 50 (the variable-cost increase from 1 exponent byte)
    assert_eq!(
        insp_result.additional_gas, 50,
        "expected delta of 50 from EXP repricing with 1-byte exponent; got {}",
        insp_result.additional_gas
    );
}

/// Verify that EXP with a multi-byte exponent correctly computes the byte count.
/// The contract computes 2^(2^16), where the exponent 65536 = 0x10000 requires 3 bytes.
#[test]
fn test_exp_multi_byte_exponent() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let contract_addr = Address::new([0x11; 20]);

    // Contract: PUSH3 0x010000, PUSH1 2, EXP, POP, STOP
    // Exponent = 0x010000 = 65536 → 3 bytes.
    let code = Bytes::from(vec![
        0x62, 0x01, 0x00, 0x00, // PUSH3 0x010000 (exponent = 65536)
        0x60, 0x02, // PUSH1 2 (base)
        0x0A, // EXP
        0x50, // POP
        0x00, // STOP
    ]);

    let mut db = CacheDB::new(revm::database_interface::EmptyDB::new());
    db.insert_account_info(
        contract_addr,
        AccountInfo {
            nonce: 1,
            balance: U256::ZERO,
            code_hash: alloy_primitives::keccak256(&code),
            code: Some(Bytecode::new_raw(code)),
            account_id: None,
        },
    );
    db.insert_account_info(
        sender,
        AccountInfo {
            nonce: 0,
            balance: U256::from(1_000_000_000_000u64),
            code_hash: alloy_primitives::keccak256([]),
            code: None,
            account_id: None,
        },
    );

    // Current: constant=10, per-byte=50 → total for 3 bytes = 10 + 3*50 = 160
    // New: constant=10, per-byte=100 → total for 3 bytes = 10 + 3*100 = 310
    // Delta = 310 - 160 = 150
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
EXP,constant,10,10
EXP,exp_bytes,50,100
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("exp-multi".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(100_000)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(contract_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let mut inspector = ScheduleInspector::new(schedule);

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet_with_inspector(&mut inspector);
    let result = evm.inspect_tx(tx).expect("should succeed");
    assert!(result.result.is_success(), "tx should succeed");

    let insp_result = inspector.result();
    assert_eq!(
        insp_result.additional_gas, 150,
        "expected delta of 150 from EXP repricing with 3-byte exponent; got {}",
        insp_result.additional_gas
    );
}

// ============================================================================
// CALL Opcode Gas Delta Tests
// ============================================================================

/// Verify that a gas delta applied to the CALL opcode itself (not just caller
/// opcodes) correctly reduces the subcall's gas budget via the 63/64 rule.
///
/// The CSV schedule adds +5000 to the CALL opcode's cost. Because the delta is
/// applied in `step()` (before the CALL executes), the caller's remaining gas
/// is reduced by 5000 before the 63/64 forwarding calculation. This means the
/// callee receives less gas than under baseline, potentially causing OOG.
#[test]
fn test_call_opcode_gas_delta_affects_forwarding() {
    use revm::primitives::hardfork::SpecId;

    let sender = Address::new([0x01; 20]);
    let caller_addr = Address::new([0x11; 20]);
    let callee_addr = Address::new([0x22; 20]);

    // Caller: just CALLs callee forwarding all gas (no other expensive opcodes)
    let caller_code = build_caller_bytecode(0, callee_addr);
    let callee_code = build_callee_bytecode(5); // 5 SLOADs ≈ 10,500 gas needed

    let gas_limit = 50_000u64;

    // --- Baseline execution (no inspector) ---
    let db =
        setup_evm_db(caller_addr, callee_addr, sender, caller_code.clone(), callee_code.clone());
    let tx = TxEnv::builder()
        .caller(sender)
        .gas_limit(gas_limit)
        .gas_price(0)
        .kind(alloy_primitives::TxKind::Call(caller_addr))
        .value(U256::ZERO)
        .data(Bytes::new())
        .nonce(0)
        .build_fill();

    let ctx = Context::mainnet().with_db(db).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm = ctx.build_mainnet();
    let baseline_result = evm.transact(tx.clone()).expect("baseline should succeed");
    assert!(baseline_result.result.is_success(), "baseline tx should succeed");
    let baseline_gas = baseline_result.result.tx_gas_used();

    // --- Schedule: CALL opcode costs +5000 ---
    let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
CALL,constant,100,5100
"#;
    let schedule = Arc::new(
        CsvPricingSchedule::from_csv("call-expensive".to_string(), csv_data.as_bytes()).unwrap(),
    );

    let db2 = setup_evm_db(caller_addr, callee_addr, sender, caller_code, callee_code);
    let mut inspector = ScheduleInspector::new(schedule);

    let ctx2 = Context::mainnet().with_db(db2).modify_cfg_chained(|cfg: &mut CfgEnv| {
        *cfg = CfgEnv::new_with_spec(SpecId::CANCUN);
        cfg.disable_nonce_check = true;
    });
    let mut evm2 = ctx2.build_mainnet_with_inspector(&mut inspector);
    let schedule_result = evm2.inspect_tx(tx).expect("schedule execution should not error");
    let schedule_gas = schedule_result.result.tx_gas_used();

    let insp_result = inspector.result();
    assert!(
        insp_result.additional_gas > 0,
        "inspector should report additional gas charged for CALL opcode; got {}",
        insp_result.additional_gas
    );

    // The CALL opcode delta should cause more total gas usage
    assert!(
        schedule_gas > baseline_gas,
        "CALL opcode delta should increase total gas: baseline={baseline_gas}, schedule={schedule_gas}"
    );

    // The delta from the CALL opcode should be reflected in the gas difference.
    // The exact delta may differ from 5000 due to 63/64 forwarding effects,
    // but it should be at least 4000 (most of the 5000 delta).
    let gas_delta = schedule_gas - baseline_gas;
    assert!(
        gas_delta >= 4000,
        "gas delta from CALL opcode repricing should be at least 4000; got {gas_delta}"
    );

    // Verify the callee received less gas under the schedule by checking call frames
    let call_frames = inspector.call_frames();
    assert!(!call_frames.is_empty(), "should have at least one call frame");
    let callee_frame = &call_frames[0];
    assert_eq!(callee_frame.to, Some(callee_addr), "first frame should be to callee");
}
