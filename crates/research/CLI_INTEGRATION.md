# CLI Integration for Research Mode

## Status

CLI arguments have been added to reth-node-core, making it easy to configure research mode from the command line.

## Available CLI Flags

```bash
reth node \
  --research.enabled \
  --research.gas-multiplier 128 \
  --research.start-block 18000000 \
  --research.db-path ./divergence.db \
  --research.refund-multiplier 128.0 \
  --research.stipend-multiplier 128.0
```

### Flags

- `--research.enabled`: Enable gas price research mode (default: false)
- `--research.gas-multiplier`: Gas cost multiplier for research mode (default: 128)
- `--research.start-block`: Block number to start research analysis (default: 0)
- `--research.db-path`: Path to divergence database file (default: ./divergence.db)
- `--research.refund-multiplier`: Refund multiplier for research mode (default: 128.0)
- `--research.stipend-multiplier`: Stipend multiplier for research mode (default: 128.0)

## Building with Research Feature

```bash
cargo build --release --features research
```

## Current Limitation

**Note**: The CLI flags are defined and accessible via `NodeConfig.research`, but the automatic wiring into `ExecutionStage` requires additional work due to Rust trait system constraints.

### Manual Integration Required

To actually enable research mode, you currently need to modify your node builder code:

```rust
use reth_research::{config::ResearchConfig, database::DivergenceDatabase};

// In your node builder (after parsing CLI args):
let node_config = /* your NodeConfig with parsed CLI args */;

if node_config.research.enabled {
    let config = node_config.research.to_research_config();
    let db = node_config.research.open_database()?;

    // When building ExecutionStage:
    let execution_stage = ExecutionStage::new(
        evm_config,
        consensus,
        thresholds,
        external_clean_threshold,
        exex_manager_handle,
    ).with_research_mode(config, db);
}
```

### Why Manual Integration?

The Rust trait system doesn't allow conditional trait bounds based on cargo features. The `ExecutionStage` needs `StateProviderFactory` when research mode is used, but this can't be made conditional at compile time without breaking the `Stage` trait impl.

The cleanest approach is to:
1. Use CLI flags to configure research parameters
2. Manually wire up research mode in your node builder where you have access to the full provider infrastructure

## Alternative: Direct Configuration

You can also skip the CLI flags entirely and directly configure research mode:

```rust
let config = ResearchConfig {
    gas_multiplier: 128,
    start_block: 18_000_000,
    divergence_db_path: PathBuf::from("./divergence.db"),
    ..Default::default()
};

let db = DivergenceDatabase::open("./divergence.db")?;

let execution_stage = ExecutionStage::new(...)
    .with_research_mode(config, db);
```

## Future Work

To make CLI integration fully automatic, we would need to:

1. Refactor ExecutionStage to store ProviderFactory reference
2. Add a research-specific execution path that doesn't go through the Stage trait
3. Or create a wrapper Stage implementation that handles research mode separately

For now, the CLI flags provide a convenient way to configure research parameters, but the actual integration requires a few lines of manual code in your node builder.

## See Also

- [COMPLETE.md](./COMPLETE.md) - Full implementation details
- [SUMMARY.md](./SUMMARY.md) - Quick reference guide
- [README.md](./README.md) - Architecture overview
