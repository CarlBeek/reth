# Research Mode ExEx Example

**Status: Work in Progress**

This example demonstrates how to create an Execution Extension (ExEx) for Reth's research mode,
which performs dual execution analysis to detect divergences caused by modified gas costs.

## Overview

The research mode ExEx:
1. Receives committed blocks from the execution pipeline
2. Executes each transaction twice:
   - Once with normal gas costs (baseline)
   - Once with multiplied gas costs (experimental)
3. Compares execution results to detect divergences
4. Records divergences to a SQLite database

## Divergence Types Detected

- **Status**: Success/failure differs between executions
- **Gas Pattern**: Structural differences in gas usage (>5% difference)
- **State Root**: Different post-execution state
- **Event Logs**: Different logs emitted
- **Call Tree**: Different external calls made

## Implementation Status

The ExEx implementation is partially complete but requires additional work to properly handle:
- Generic type constraints for `FullNodeComponents`
- Proper access to EVM configuration and state providers
- Block and transaction type conversions

## Alternative: Direct Integration

Until the ExEx is complete, research mode can be invoked directly using the
`analyze_block_with_replay()` method from `ExecutionStage`. This requires access to:
- `StateProviderFactory` for historical state
- `ConfigureEvm` for EVM configuration
- A recovered block with transactions

See `crates/stages/stages/src/stages/execution.rs` for the complete implementation.

## Configuration

Research mode supports the following configuration:

```rust
ResearchConfig {
    gas_multiplier: 128,              // Multiply gas costs by this factor
    divergence_db_path: "./divergences.db".into(),
    start_block: 0,                    // Block to start analysis
    max_divergences_per_block: None,  // Optional limit
    trace_detail: TraceDetail::Standard,
}
```

## Database Schema

Divergences are stored in SQLite with the following structure:

```sql
CREATE TABLE divergences (
    id INTEGER PRIMARY KEY,
    block_number INTEGER,
    tx_index INTEGER,
    tx_hash TEXT,
    timestamp INTEGER,
    divergence_types TEXT,  -- JSON array
    normal_gas_used INTEGER,
    experimental_gas_used INTEGER,
    gas_efficiency_ratio REAL,
    divergence_data TEXT    -- JSON with full details
);
```

## Future Work

- [ ] Fix generic type constraints
- [ ] Add proper error handling for failed experimental executions
- [ ] Implement call tree extraction
- [ ] Add detailed event log recording
- [ ] Support for dual inspector tracking (normal + experimental operation counts)
- [ ] Add CLI args for all configuration options
- [ ] Add metrics export

## Running (Once Complete)

```bash
cargo run -p example-exex-research -- node --dev --dev.block-time 5s \
  --research.gas-multiplier 128 \
  --research.db-path ./divergences.db \
  --research.start-block 0
```

## Querying Divergences

```bash
sqlite3 divergences.db "SELECT * FROM divergences WHERE block_number > 1000 LIMIT 10;"
```

## Related Files

- `crates/research/` - Research mode implementation
- `crates/stages/stages/src/stages/execution.rs` - Dual execution logic
- `crates/research/src/inspector.rs` - Gas multiplier inspector
