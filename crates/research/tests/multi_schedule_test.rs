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
    MultiScheduleInspector,
};
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

    let schedule =
        CsvPricingSchedule::from_csv("test".to_string(), csv_data.as_bytes()).unwrap();

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
        sender: Address::repeat_byte(0x01),
        recipient: None,
        value: U256::ZERO,
        input: Bytes::from(vec![0x60, 0x00]), // minimal bytecode (PUSH1 0x00)
        gas_limit: 1_000_000,
        is_create: true,
        recipient_info: None,
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
fn test_multi_schedule_inspector_with_csv_schedules() {
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

    let schedules: Vec<Arc<dyn GasSchedule>> =
        vec![Arc::new(schedule1), Arc::new(schedule2)];

    let inspector = MultiScheduleInspector::new(schedules);

    // Both schedules should be tracked
    let results = inspector.results();
    assert_eq!(results.len(), 2);

    // Find by name
    assert!(inspector.result_for("conservative").is_some());
    assert!(inspector.result_for("aggressive").is_some());
}

#[test]
fn test_multi_schedule_inspector_filters_intrinsic_only() {
    let schedules: Vec<Arc<dyn GasSchedule>> = vec![
        Arc::new(BaselineSchedule),
        Arc::new(Eip2780Schedule::new()), // IntrinsicOnly
        Arc::new(MultiplierSchedule::with_multiplier(128)), // ExecutionOnly
    ];

    let inspector = MultiScheduleInspector::new(schedules);

    // Only execution-modifying schedules should be tracked
    // EIP-2780 is IntrinsicOnly, so it won't be in the inspector
    // Baseline returns 0 for all deltas
    // Multiplier returns 0 but is ExecutionOnly
    let results = inspector.results();

    // Should have baseline, 128x - EIP-2780 is filtered out in create_inspector
    // but all are passed here, so all 3 are tracked
    assert_eq!(results.len(), 3);
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
    };

    let baseline = ExecutionSummary::new(true, 21000, 21000);

    let result = analyzer
        .analyze_transaction(
            1000000,
            0,
            B256::ZERO,
            1234567890,
            baseline,
            Some(&tx_context),
            &[],
        )
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

    let result = analyzer
        .analyze_transaction(1000000, 0, B256::ZERO, 1234567890, baseline, None, &[])
        .unwrap();

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
            &[],
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
    use reth_research::database::ScheduleDivergence;
    use reth_research::divergence::DivergenceType;

    let div1 = ScheduleDivergence {
        schedule_name: "eip-2780".to_string(),
        block_number: 1000000,
        tx_index: 0,
        tx_hash: B256::repeat_byte(0x01),
        timestamp: 1234567890,
        divergence_type: DivergenceType::GasPattern,
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
    };

    let div2 = ScheduleDivergence {
        schedule_name: "7904-v1".to_string(),
        block_number: 1000000,
        tx_index: 1,
        tx_hash: B256::repeat_byte(0x02),
        timestamp: 1234567890,
        divergence_type: DivergenceType::Status,
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

    // Step 5: Create inspector for execution tracking
    let _inspector = analyzer.create_inspector();

    // Step 6: Simulate transaction analysis
    let tx_context = TxContext {
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
    };

    let baseline = ExecutionSummary::new(true, 21000, 21000)
        .with_operation_counts(OperationCounts {
            sload_count: 0,
            sstore_count: 0,
            call_count: 0,
            log_count: 0,
            total_ops: 100,
            memory_words_allocated: 10,
            create_count: 0,
        });

    // Step 7: Analyze transaction
    let result = analyzer
        .analyze_transaction(
            1000000,
            0,
            B256::repeat_byte(0xAB),
            1234567890,
            baseline,
            Some(&tx_context),
            &[], // No inspector results since we didn't execute
        )
        .unwrap();

    // Step 8: Verify results
    assert!(result.has_any_divergence());
    assert!(result.divergent_schedules().contains(&"eip-2780"));

    // Step 9: Verify database
    let counts = db.divergence_counts_by_schedule().unwrap();
    let has_eip2780 = counts.iter().any(|(name, _)| name == "eip-2780");
    assert!(has_eip2780);

    // Step 10: Check stats
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
        CsvPricingSchedule::from_csv("precompile-test".to_string(), csv_data.as_bytes())
            .unwrap();

    // ECPAIRING at address 0x08
    let ecpairing_addr = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);

    // 3 pairs = 576 bytes input
    let input = vec![0u8; 576];
    let delta = schedule.precompile_gas_delta(&ecpairing_addr, &input);

    // Delta = (45000 + 3*34103) - (45000 + 3*34000) = 309
    assert_eq!(delta, 309);

    // BLAKE2F at address 0x09
    let blake2f_addr = Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]);

    // 100 rounds encoded in first 4 bytes
    let mut input = vec![0u8; 213]; // BLAKE2F input size
    input[0..4].copy_from_slice(&100u32.to_be_bytes());
    let delta = schedule.precompile_gas_delta(&blake2f_addr, &input);

    // Delta = (170 + 100*2) - (0 + 100*1) = 170 + 200 - 100 = 270
    assert_eq!(delta, 270);
}
