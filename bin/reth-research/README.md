# Research Mode ExEx

**Status: Production Ready ✅**

This ExEx performs dual execution analysis to detect divergences caused by modified gas costs.
It processes committed blocks in real-time, executing each transaction twice (normal and experimental)
and recording any divergences to a SQLite database.

## Overview

The research mode ExEx:
1. Subscribes to committed blocks from the execution pipeline via ExEx notifications
2. Executes each transaction twice for every committed block:
   - Once with normal gas costs using `TrackingInspector` (baseline)
   - Once with multiplied gas costs using `GasResearchInspector` (experimental)
3. Compares execution results across multiple dimensions
4. Records divergences asynchronously to a SQLite database
5. Exports metrics for monitoring

## Divergence Types Detected

- **Status**: Success/failure differs between executions
- **Gas Pattern**: Structural differences in gas usage (>5% deviation from expected ratio)
- **State Root**: Different post-execution state (account balances, storage, nonces)
- **Event Logs**: Different logs emitted (count, topics, data, or addresses differ)
- **Out of Gas (OOG)**: Experimental execution runs out of gas while normal succeeds

## Features

✅ **Real-time Analysis**: Processes blocks as they're committed, no re-sync needed
✅ **Async Database Writes**: Non-blocking SQLite writes with dedicated task
✅ **Reorg Handling**: Automatically processes new chain after reorgs
✅ **Detailed Tracing**: Optional call tree and event log recording
✅ **Metrics Export**: Prometheus-compatible metrics for monitoring
✅ **Configurable Start Block**: Skip early blocks with `--research.start-block`
✅ **OOG Detection**: Identifies when experimental execution hits gas limits
✅ **Operation Counting**: Tracks opcodes executed in both normal and experimental runs

## Configuration

Research mode supports the following CLI arguments:

```bash
--research.enabled                    # Enable research mode (required)
--research.gas-multiplier <N>         # Multiply gas costs by N (default: 128)
--research.start-block <BLOCK>        # Start analyzing from this block (default: 0)
--research.db-path <PATH>             # SQLite database path (default: ./divergence.db)
--research.refund-multiplier <N>      # Multiply gas refunds (default: 128.0)
--research.stipend-multiplier <N>     # Multiply gas stipends (default: 128.0)
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

## Running

### On an Already-Synced Node (Recommended)

For analyzing blocks as your node processes them (no re-sync required):

```bash
# Build the research binary
cargo build --release -p reth-research

# Run with research mode enabled
./target/release/reth-research node \
  --research.enabled \
  --research.gas-multiplier 128 \
  --research.start-block 18000000 \
  --research.db-path ./divergences.db
```

The ExEx will start analyzing committed blocks from your current tip (or `--research.start-block` if higher).

### For Re-analyzing Historical Blocks

To re-execute a specific block range:

```bash
# Unwind execution stage to start block
reth stage unwind execution --to 18000000

# Re-run execution with research mode
reth stage run execution --to 18500000 \
  --research.enabled \
  --research.start-block 18000000 \
  --research.gas-multiplier 128 \
  --research.db-path ./divergences.db
```

### Development Mode

For testing on a dev chain:

```bash
cargo run --release -p reth-research -- node --dev --dev.block-time 5s \
  --research.enabled \
  --research.gas-multiplier 128 \
  --research.db-path ./divergences.db
```

## Querying Divergences

### Basic Queries

```bash
# Count total divergences
sqlite3 divergences.db "SELECT COUNT(*) FROM divergences;"

# View recent divergences
sqlite3 divergences.db "SELECT block_number, tx_index, divergence_types FROM divergences ORDER BY block_number DESC LIMIT 10;"

# Find status divergences (critical)
sqlite3 divergences.db "SELECT * FROM divergences WHERE divergence_types LIKE '%Status%' LIMIT 10;"

# Find high gas efficiency changes
sqlite3 divergences.db "SELECT block_number, tx_hash, gas_efficiency_ratio FROM divergences WHERE ABS(gas_efficiency_ratio - 1.0) > 0.1 ORDER BY ABS(gas_efficiency_ratio - 1.0) DESC LIMIT 20;"
```

## Architecture

The ExEx operates as a separate task that:

1. **Subscribes to Block Notifications**: Receives `ChainCommitted` events with finalized blocks
2. **Dual Execution Pipeline**:
   - Creates two independent EVM instances per transaction
   - Normal execution with `TrackingInspector` for baseline
   - Experimental execution with `GasResearchInspector` applying gas multipliers
3. **Divergence Detection**: Compares execution results across:
   - Success/failure status
   - Gas consumption patterns
   - State changes (account info, storage)
   - Event logs emitted
4. **Async Database Writer**: Queues divergences for non-blocking SQLite writes
5. **Metrics Export**: Publishes Prometheus metrics for monitoring

## Metrics

The ExEx exports the following metrics:

- `research_blocks_processed`: Total blocks analyzed
- `research_divergences_total`: Count by divergence type
- `research_gas_efficiency_ratio`: Distribution of gas efficiency changes
- `research_oog_events`: Out-of-gas occurrences by pattern
- `research_block_processing_time`: Per-block analysis duration

## Performance Considerations

- **CPU**: Dual execution approximately doubles CPU usage during block processing
- **Memory**: Maintains two separate EVM caches per transaction (typically <100MB extra)
- **Disk I/O**: Async SQLite writes minimize impact on block processing
- **State Access**: Re-fetches historical state for each execution to ensure isolation

## Related Files

- `bin/reth-research/src/main.rs` - ExEx entry point and dual execution logic
- `crates/research/` - Core research mode implementation
  - `src/inspector.rs` - Gas multiplier inspector
  - `src/tracking_inspector.rs` - Baseline tracking inspector
  - `src/divergence.rs` - Divergence types and analysis
  - `src/database.rs` - SQLite persistence layer
- `crates/stages/stages/src/stages/execution.rs` - Stage-based research integration (alternative approach)
