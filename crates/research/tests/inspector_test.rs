//! Test that the inspector works with the current revm API

use reth_research::{
    config::ResearchConfig, gas_pricing::GasPricingTable, inspector::GasResearchInspector,
};

const TEST_CSV: &str = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
"#;

#[test]
fn test_inspector_compiles_and_has_correct_api() {
    // Just verify that we can create an inspector
    let config = ResearchConfig::default();
    let gas_limit = 30_000_000;

    let _inspector = GasResearchInspector::new(config, gas_limit);

    // The fact that this compiles means our Inspector trait impl is compatible
    // with the current revm API
}

#[test]
fn test_inspector_tracks_operations() {
    let config = ResearchConfig::default();
    let gas_limit = 30_000_000;

    let inspector = GasResearchInspector::new(config, gas_limit);

    // Verify we can access operation counts
    let counts = inspector.operation_counts();
    assert_eq!(counts.sload_count, 0);
    assert_eq!(counts.sstore_count, 0);
    assert_eq!(counts.call_count, 0);
    assert_eq!(counts.total_ops, 0);
}

#[test]
fn test_inspector_with_csv_pricing() {
    let gas_pricing = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();
    let config = ResearchConfig { gas_pricing, ..Default::default() };
    let gas_limit = 30_000_000;

    let inspector = GasResearchInspector::new(config, gas_limit);

    // Verify the inspector is created with no OOG
    assert!(!inspector.oog_occurred());
    assert_eq!(inspector.additional_gas_charged(), 0);
}

#[test]
fn test_inspector_default_no_repricing() {
    let config = ResearchConfig::default();
    let gas_limit = 30_000_000;

    let inspector = GasResearchInspector::new(config, gas_limit);

    // Default config has no pricing table, so no additional gas should be charged
    assert!(!inspector.oog_occurred());
    assert_eq!(inspector.additional_gas_charged(), 0);
}
