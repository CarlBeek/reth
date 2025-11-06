# Research Mode Integration Strategy

## The Problem

The `Executor` trait doesn't provide access to the EVM/Inspector system. We need to integrate at a lower level where we can actually attach our `GasResearchInspector`.

## Current Architecture (What We Have)

```
ExecutionStage
  └── Executor::execute_one()
        └── Returns BlockExecutionResult
```

We can wrap `Executor`, but we can't inject inspectors through this interface.

## Solution: Integrate at ConfigureEvm Level

Instead of wrapping the `Executor`, we need to wrap or modify the `ConfigureEvm` implementation.

### Option 1: Custom ConfigureEvm Wrapper (RECOMMENDED)

Create a `ResearchEvmConfig` that wraps the base config and injects our inspector:

```rust
pub struct ResearchEvmConfig<Base> {
    base: Base,
    config: ResearchConfig,
    divergence_db: Arc<Mutex<Option<DivergenceDatabase>>>,
}

impl<Base: ConfigureEvm> ConfigureEvm for ResearchEvmConfig<Base> {
    type Primitives = Base::Primitives;

    fn evm_with_env_and_inspector<'a, DB, I>(
        &self,
        db: DB,
        env: EvmEnv<Self::Spec>,
        inspector: &'a mut I,
    ) -> Evm<'a, I, DB>
    where
        DB: Database,
        I: Inspector<DB>,
    {
        // Wrap the inspector with our GasResearchInspector
        let research_inspector = GasResearchInspector::new(self.config.clone(), env.block.gas_limit);

        // Create a composite inspector that calls both
        let composite = CompositeInspector {
            inner: inspector,
            research: research_inspector,
        };

        self.base.evm_with_env_and_inspector(db, env, &mut composite)
    }
}
```

### Option 2: Modify ExecutionStage Directly

Directly modify `ExecutionStage` to optionally create and use a `GasResearchInspector`:

```rust
// In ExecutionStage::execute()
let mut inspector = if let Some(research_config) = self.research_config {
    Some(GasResearchInspector::new(research_config, block.gas_limit()))
} else {
    None
};

let result = if let Some(ref mut insp) = inspector {
    executor.execute_one_with_inspector(block, insp)?
} else {
    executor.execute_one(block)?
};

// After execution, analyze inspector data
if let Some(inspector) = inspector {
    self.analyze_and_record(block, &result, &inspector)?;
}
```

## Recommended Approach: ConfigureEvm Wrapper

This is cleaner and more modular.

### Implementation Steps

1. **Create `ResearchEvmConfig`** wrapper
2. **Create `CompositeInspector`** that forwards to multiple inspectors
3. **Modify node builder** to optionally wrap EVM config
4. **Add CLI flags** for research mode

### File Changes Needed

```
crates/research/src/
├── evm_config.rs          # NEW: ResearchEvmConfig wrapper
├── composite_inspector.rs # NEW: Multi-inspector support
├── lib.rs                 # Export new types
└── ...existing files...

crates/ethereum/node/src/
└── builder.rs             # Wrap EVM config if research mode enabled

bin/reth/src/
└── cli.rs                 # Add --research-mode flags
```

## Alternative: Post-Execution Analysis

If inspector integration is too complex, we can:

1. Execute normally and get receipts
2. **Replay** transactions with our inspector in a separate thread
3. Analyze the replay data

This is simpler but less accurate (doesn't catch state-dependent issues).

```rust
impl ResearchExecutor {
    fn execute_one(&mut self, block) -> Result<...> {
        // Execute normally
        let result = self.inner.execute_one(block)?;

        // Spawn background task to replay with inspector
        if self.config.enabled {
            let block_clone = block.clone();
            let config = self.config.clone();
            tokio::spawn(async move {
                replay_with_inspector(block_clone, config).await
            });
        }

        Ok(result)
    }
}
```

## Decision: What To Implement Now

For the MVP (minimum viable product), let's do **Post-Execution Analysis with Replay**.

### Why?

1. ✅ No changes to core reth code needed
2. ✅ Isolated in research crate
3. ✅ Can be improved later
4. ✅ Gets us data quickly
5. ❌ Less accurate (but good enough for research)

### How It Works

```
ExecutionStage
  └── ResearchExecutor::execute_one()
        ├── inner.execute_one()  // Normal execution
        │     └── Returns result
        └── replay_with_inspector()  // Background analysis
              ├── Create new State from same DB
              ├── Attach GasResearchInspector
              ├── Re-execute transactions
              ├── Collect inspector data
              └── Record divergences
```

## Next Steps

1. Implement state replay function
2. Create inspector-attached execution
3. Wire up the analysis pipeline
4. Test with historical blocks

This approach lets us complete the research system without deep integration into reth's execution pipeline, which we can add later if needed.
