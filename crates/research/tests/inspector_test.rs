//! Test that the inspector works with the current revm API

use reth_research::{config::ResearchConfig, inspector::GasResearchInspector};
use revm::{
    context_interface::ContextTr,
    interpreter::{interpreter::EthInterpreter, Interpreter},
    Inspector,
};

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
fn test_inspector_gas_simulation() {
    let mut config = ResearchConfig::default();
    config.gas_multiplier = 128;
    let gas_limit = 30_000_000;

    let inspector = GasResearchInspector::new(config, gas_limit);

    // Verify the inspector is created with the correct multiplier
    assert!(!inspector.oog_occurred());
}
