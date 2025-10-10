//! Metrics for research mode.

use metrics::{counter, describe_counter, describe_histogram, histogram};

/// Register all research metrics.
pub fn register_metrics() {
    describe_counter!(
        "reth_research_blocks_processed_total",
        "Total number of blocks processed in research mode"
    );

    describe_counter!(
        "reth_research_transactions_processed_total",
        "Total number of transactions processed in research mode"
    );

    describe_counter!("reth_research_divergences_total", "Total number of divergences detected");

    describe_counter!(
        "reth_research_divergences_by_type",
        "Divergences by type (state_root, call_tree, etc.)"
    );

    describe_counter!(
        "reth_research_oog_total",
        "Total number of out-of-gas events in experimental execution"
    );

    describe_histogram!(
        "reth_research_block_execution_seconds",
        "Time to execute a block in research mode (both executions)"
    );

    describe_histogram!("reth_research_gas_efficiency_ratio", "Gas efficiency ratio distribution");

    describe_histogram!(
        "reth_research_divergence_detection_seconds",
        "Time spent detecting divergences"
    );
}

/// Record a block being processed.
pub fn record_block_processed(block_number: u64, tx_count: usize, duration_secs: f64) {
    counter!("reth_research_blocks_processed_total").increment(1);
    counter!("reth_research_transactions_processed_total").increment(tx_count as u64);
    histogram!("reth_research_block_execution_seconds").record(duration_secs);

    tracing::debug!(
        target: "reth::research",
        block = block_number,
        tx_count = tx_count,
        duration_ms = duration_secs * 1000.0,
        "Block processed in research mode"
    );
}

/// Record a divergence being detected.
pub fn record_divergence(
    divergence_types: &[crate::divergence::DivergenceType],
    gas_efficiency_ratio: f64,
) {
    counter!("reth_research_divergences_total").increment(1);

    for dtype in divergence_types {
        counter!("reth_research_divergences_by_type", "type" => dtype.to_string()).increment(1);
    }

    histogram!("reth_research_gas_efficiency_ratio").record(gas_efficiency_ratio);
}

/// Record an out-of-gas event.
pub fn record_oog(pattern: crate::divergence::OogPattern) {
    counter!("reth_research_oog_total").increment(1);
    counter!("reth_research_oog_by_pattern", "pattern" => pattern.to_string()).increment(1);
}

/// Record divergence detection time.
pub fn record_divergence_detection_time(duration_secs: f64) {
    histogram!("reth_research_divergence_detection_seconds").record(duration_secs);
}
