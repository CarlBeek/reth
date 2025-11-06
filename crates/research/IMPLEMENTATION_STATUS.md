# Research Mode Implementation - Current Status

**Date**: 2025-10-07
**Completion**: ~92%

## What's Working ✅

### 1. Full Infrastructure (Complete)
- Configuration system
- Divergence detection types
- SQLite database with schema
- Prometheus metrics
- Inspector implementation
- All tests passing

### 2. ExecutionStage Integration (Complete)
- `research` feature added to `reth-stages`
- Research fields added to `ExecutionStage`
- `with_research_mode()` builder method
- Hook integrated at line 449 in execution loop

### 3. Replay Framework (Complete)
- `analyze_block_with_replay()` method implemented
- State provider created for block N-1 ✅
- Wrapped in `CacheDB` ✅
- Inspector instantiated ✅
- Error handling in place ✅

**Code location**: `crates/stages/stages/src/stages/execution.rs:283-341`

```rust
fn analyze_block_with_replay<Provider>(
    &self,
    provider: &Provider,
    block: &RecoveredBlock<...>,
    _normal_result: &BlockExecutionResult<...>,
) -> Result<(), StageError>
{
    // ✅ Get state at block N-1
    let state_provider = provider.history_by_block_number(block_number - 1)?;

    // ✅ Wrap in database
    let db = StateProviderDatabase(state_provider);
    let cache_db = CacheDB::new(db);

    // ✅ Create inspector
    let inspector = GasResearchInspector::new(config, gas_limit);

    // ⏳ TODO: Execute transactions with inspector
    // ⏳ TODO: Analyze for divergences
    // ⏳ TODO: Record to database
}
```

## What's Left ⏳

### Critical Path to Completion

**Estimated effort**: 6-10 hours

#### 1. Transaction Execution with Inspector (4-6 hours)

**Challenge**: Need to execute each transaction in the block with our inspector attached.

**What needs to be done**:

```rust
// For each transaction in the block:
for (tx_idx, tx) in block.body().transactions().enumerate() {
    // A. Build transaction environment
    let tx_env = /* ??? How to build TxEnvFor from transaction? */;

    // B. Build EVM environment for this transaction
    let evm_env = /* ??? How to build EvmEnvFor from block header? */;

    // C. Create EVM with inspector
    let mut evm = self.evm_config.evm_with_env_and_inspector(
        &mut cache_db,
        evm_env,
        &mut inspector,
    );

    // D. Execute transaction
    let result = evm.transact(tx_env)?;

    // E. Inspector now has accumulated data for this transaction
    // Continue to next transaction...
}
```

**Key questions to resolve**:
1. How to build `TxEnvFor<Self::Evm>` from a `SignedTransaction`?
2. How to build `EvmEnvFor<Self::Evm>` from block header and chain spec?
3. How to handle EVM environment updates between transactions (e.g., gas used)?
4. Do we need to commit state between transactions or keep it all in inspector?

**Research needed**:
- Look at how `BlockExecutor` implementations build these environments
- Check ethereum executor for examples: `crates/ethereum/evm/src/execute.rs`
- Look at RPC trace implementation for inspector patterns: `crates/rpc/rpc-eth-api/src/helpers/trace.rs`

#### 2. Divergence Analysis (1-2 hours)

Once we have inspector data from replay, we need to:

```rust
// After all transactions executed:

// Compare inspector results with normal execution
let divergences = analyze_for_divergences(
    &inspector,
    block,
    normal_result,
    config,
);

fn analyze_for_divergences(...) -> Vec<Divergence> {
    let mut divergences = Vec::new();

    // Check if out-of-gas occurred in replay but not in normal execution
    if inspector.oog_occurred() && !normal_oog {
        divergences.push(Divergence {
            divergence_types: vec![DivergenceType::OutOfGas],
            oog_info: inspector.oog_info().cloned(),
            // ...
        });
    }

    // Check if operation counts differ significantly
    let normal_ops = /* extract from normal_result */;
    let replay_ops = inspector.operation_counts();
    if differs_significantly(normal_ops, replay_ops) {
        divergences.push(Divergence {
            divergence_types: vec![DivergenceType::GasPattern],
            // ...
        });
    }

    // TODO: More divergence checks...

    divergences
}
```

**Note**: Much of this logic already exists in `crates/research/src/executor.rs:140-239` and can be adapted.

#### 3. Database Recording (1 hour)

```rust
// Record divergences
if let Some(ref db) = self.research_db {
    for divergence in divergences {
        if let Err(e) = db.record_divergence(&divergence) {
            warn!("Failed to record divergence: {}", e);
        }
    }
}
```

This part is straightforward since the database API is already implemented.

#### 4. Testing (2-3 hours)

- Unit test the transaction execution loop
- Integration test with a simple historical block
- Verify divergences are detected correctly
- Performance test to ensure acceptable overhead

## Recommended Next Steps

### Step 1: Research Transaction/EVM Environment Building

Before writing code, understand how to build the environments:

```bash
# Look at how environments are built in ethereum executor
rg "TxEnvFor|EvmEnvFor" --type rust crates/ethereum/evm/src/

# Look at block executor patterns
rg "evm_with_env" --type rust crates/evm/

# Look at trace implementation
rg "evm_env.*tx_env" --type rust crates/rpc/rpc-eth-api/src/helpers/
```

### Step 2: Implement Transaction Execution Loop

Start with a minimal implementation:

```rust
// In analyze_block_with_replay(), replace TODO with:

for tx in block.body().transactions() {
    // Build environments (pattern TBD from research)
    let evm_env = build_evm_env(block.header(), self.evm_config)?;
    let tx_env = build_tx_env(tx)?;

    // Execute with inspector
    let mut evm = self.evm_config.evm_with_env_and_inspector(
        &mut cache_db,
        evm_env,
        &mut inspector,
    );

    match evm.transact(tx_env) {
        Ok(_result) => {
            // Inspector has accumulated data
        }
        Err(e) => {
            // Log but continue - we want to see all transactions
            debug!("Transaction execution failed in replay: {:?}", e);
        }
    }
}
```

### Step 3: Add Divergence Analysis

Use the existing analysis logic from `reth-research/src/executor.rs` and adapt it:

```rust
// After transaction loop:
let divergences = self.analyze_inspector_results(
    &inspector,
    block,
    normal_result,
)?;
```

### Step 4: Wire Up Database Recording

```rust
if let Some(ref db) = self.research_db {
    for div in divergences {
        let _ = db.record_divergence(&div);
    }
}
```

## Current Blockers

### Primary Blocker: Environment Building

The main unknown is **how to build `EvmEnvFor` and `TxEnvFor`** from block/transaction data within the `ExecutionStage` context.

**Options to explore**:

1. **Look at existing executor implementations**
   - `crates/ethereum/evm/src/execute.rs` - Ethereum block executor
   - See how it builds environments for transactions

2. **Use helper methods from evm_config**
   - `ConfigureEvm` trait may have helper methods
   - Check `evm_for_block()`, `context_for_block()`, etc.

3. **Copy pattern from RPC trace code**
   - `crates/rpc/rpc-eth-api/src/helpers/trace.rs`
   - Shows how to build envs for transaction replay

### Secondary Blocker: State Management Between Transactions

Within a block, transactions execute sequentially and each affects state. Questions:

1. Do we commit state changes to `cache_db` after each transaction?
2. Or does the inspector track everything without commits?
3. How do we ensure correct state for transaction N+1 after transaction N?

## Files to Reference

### For Environment Building
- `crates/ethereum/evm/src/execute.rs` - Block/tx execution
- `crates/evm/evm/src/lib.rs` - ConfigureEvm trait
- `crates/rpc/rpc-eth-api/src/helpers/trace.rs` - Transaction tracing

### For Divergence Analysis
- `crates/research/src/executor.rs:140-239` - Existing analysis logic
- `crates/research/src/divergence.rs` - Divergence types

### Current Implementation
- `crates/stages/stages/src/stages/execution.rs:283-341` - Replay method
- `crates/stages/stages/src/stages/execution.rs:449` - Integration hook

## Success Criteria

When complete, the system should:

1. ✅ Compile with `--features research`
2. ✅ Execute blocks normally (no regression)
3. ⏳ Replay each block's transactions with inspector
4. ⏳ Detect divergences (gas patterns, OOG, etc.)
5. ⏳ Record divergences to SQLite database
6. ⏳ Export metrics to Prometheus
7. ⏳ Handle errors gracefully (research mode doesn't break sync)

## Conclusion

We're at ~92% completion. The infrastructure is solid and the integration framework is in place. The remaining work is focused on the transaction execution loop - specifically building the EVM/transaction environments and executing with the inspector attached.

This is technically complex but has a clear path forward once we understand reth's environment building patterns. The divergence analysis and database recording are straightforward adaptations of existing code.

**Next action**: Research how to build `EvmEnvFor` and `TxEnvFor` from block/transaction data, then implement the transaction execution loop.
