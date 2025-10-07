# Research Mode - Next Steps for Full Implementation

**Date**: 2025-10-07
**Status**: Integration framework complete, awaiting replay implementation

## What's Been Done

### ✅ Complete Infrastructure (90%)

1. **Core Research Crate** (`crates/research/`)
   - Configuration system with gas multipliers, block ranges
   - Complete divergence detection types (6 types)
   - SQLite database with full schema
   - Prometheus metrics
   - Inspector that tracks operations and simulates high gas costs
   - All tests passing (15/15)

2. **ExecutionStage Integration** (`crates/stages/stages/`)
   - Added optional `research` feature to reth-stages
   - Added research config and database fields to `ExecutionStage`
   - Added `with_research_mode()` builder method
   - Added hook in execution loop at crates/stages/stages/src/stages/execution.rs:388
   - Compiles successfully with `--features research`

3. **Error Handling**
   - Generic error types that work with any executor
   - Proper error propagation

## What Remains: Transaction Replay Implementation

### The Challenge

The `ExecutionStage` has a `batch_executor` that processes blocks efficiently but doesn't expose inspector attachment. To use our `GasResearchInspector`, we need to:

1. After normal execution of a block
2. Create a separate EVM instance with our inspector attached
3. Re-execute that block's transactions
4. Collect inspector data
5. Analyze for divergences
6. Record to database

### Current Hook Point

File: `crates/stages/stages/src/stages/execution.rs:388`

```rust
// Perform research mode analysis on this block (if enabled)
#[cfg(feature = "research")]
if self.should_analyze_for_research(block_number) {
    debug!("Research mode: would analyze block");
    // TODO: Implement actual replay with inspector
}
```

This is called after:
- Block has been executed normally
- Result has been validated
- State has been updated

### Implementation Options

#### Option A: Create Single-Block Executor with Inspector (Recommended)

```rust
#[cfg(feature = "research")]
if self.should_analyze_for_research(block_number) {
    // 1. Create a GasResearchInspector
    let mut inspector = reth_research::inspector::GasResearchInspector::new(
        self.research_config.as_ref().unwrap().clone(),
        block.header().gas_limit(),
    );

    // 2. Create a new EVM with inspector attached
    // This is the tricky part - we need to create an EVM that:
    // - Uses the state BEFORE this block (for replay)
    // - Has our inspector attached
    // - Can execute the block's transactions

    // Approach: Use ConfigureEvm to create an EVM with inspector
    let evm = self.evm_config.create_evm_with_inspector(
        /* db */ todo!("Need state provider for block N-1"),
        /* env */ todo!("Need EVM environment for block N"),
        &mut inspector,
    );

    // 3. Execute each transaction
    for tx in block.body().transactions() {
        // Execute transaction with inspector
        todo!("Execute transaction and handle result");
    }

    // 4. Analyze inspector data
    let divergences = analyze_inspector_results(&inspector, &block, &result);

    // 5. Record to database
    if let Some(ref db) = self.research_db {
        for divergence in divergences {
            let _ = db.record_divergence(&divergence);
        }
    }
}
```

**Challenges**:
- Need to access state provider for block N-1 (the state before this block)
- Need to construct proper EVM environment
- Need to handle transaction execution correctly
- Current `db` is moved to the batch executor, can't reuse it

#### Option B: Defer to Background Task

```rust
#[cfg(feature = "research")]
if self.should_analyze_for_research(block_number) {
    // Clone necessary data
    let block_clone = block.clone();
    let config = self.research_config.clone();
    let db_path = self.research_db.as_ref().map(|db| db.path().to_owned());

    // Spawn background task
    std::thread::spawn(move || {
        // Replay block with inspector in background
        // This thread would:
        // 1. Open its own database connection
        // 2. Create state provider for replay
        // 3. Execute with inspector
        // 4. Record results
    });
}
```

**Pros**:
- Doesn't slow down sync
- Simpler lifetime management

**Cons**:
- More complex coordination
- Need to manage background threads
- State provider access from background thread

#### Option C: Use StateProviderDatabase from Provider

Looking at the execution loop, we have access to `provider`:

```rust
fn execute(&mut self, provider: &Provider, input: ExecInput) -> Result<...> {
    // provider is available here
    // We can create a state provider for any block from it
}
```

We could:
1. Store the provider reference (needs lifetime annotations)
2. Or pass provider to the analysis function
3. Create a state provider for block N-1
4. Use that to create an EVM with inspector

### Recommended Approach: Option C with Provider Access

Modify the analysis hook to pass the provider:

```rust
// In execute() method, after validation:
#[cfg(feature = "research")]
if self.should_analyze_for_research(block_number) {
    self.analyze_block_with_replay(provider, &block, &result);
}

// New method:
#[cfg(feature = "research")]
fn analyze_block_with_replay<Provider>(
    &self,
    provider: &Provider,
    block: &RecoveredBlock<...>,
    result: &BlockExecutionResult<...>,
) where
    Provider: /* appropriate bounds */
{
    // 1. Get state at block N-1
    let state_provider = provider.state_by_block_number(block.number() - 1)?;
    let db = StateProviderDatabase(state_provider);

    // 2. Create inspector
    let mut inspector = GasResearchInspector::new(...);

    // 3. For each transaction, create EVM and execute
    for (tx_idx, tx) in block.body().transactions().enumerate() {
        // Create EVM with inspector for this transaction
        let mut evm = self.evm_config.create_evm_with_inspector(
            db.clone(), // or appropriate db reference
            /* env for this tx */,
            &mut inspector,
        );

        // Execute transaction
        let tx_result = evm.transact()?;

        // Inspector now has data for this transaction
    }

    // 4. Analyze all collected data
    // 5. Record divergences
}
```

### What's Needed Next

1. **Add provider parameter to analysis function**
   - Modify `should_analyze_for_research` to `analyze_block_with_replay`
   - Pass `provider` reference
   - Add appropriate trait bounds

2. **Implement state provider creation**
   - Use `provider.state_by_block_number(N-1)` or similar
   - Wrap in `StateProviderDatabase`

3. **Implement EVM creation with inspector**
   - Use `self.evm_config.create_evm_with_inspector()`
   - Build proper `EnvWithHandlerCfg` for each transaction

4. **Execute transactions with inspector**
   - For each transaction in block:
     - Prepare transaction environment
     - Execute with EVM
     - Inspector collects data

5. **Implement divergence analysis**
   - Compare inspector results with actual block execution
   - Detect the 6 divergence types
   - Create `Divergence` objects

6. **Record to database**
   - Use `self.research_db.record_divergence()`
   - Handle errors gracefully

### Testing Strategy

1. **Unit test the replay mechanism**
   - Create a test block with known transactions
   - Replay with inspector
   - Verify inspector sees the right operations

2. **Integration test with real block**
   - Pick a simple mainnet block (e.g., block 18000000)
   - Run with research mode enabled
   - Verify divergence detection works

3. **Performance test**
   - Measure overhead of replay
   - Ensure it doesn't slow sync too much
   - Consider only replaying every Nth block for initial testing

### Files to Modify

1. `crates/stages/stages/src/stages/execution.rs:273-281`
   - Replace `should_analyze_for_research` with full implementation
   - Add `analyze_block_with_replay` method

2. `crates/research/src/executor.rs`
   - The divergence analysis logic in `analyze_transactions_for_divergences`
   - This already exists but isn't called
   - May need to adapt it for use from ExecutionStage

3. Integration point at line 388 in execution.rs
   - Call the full implementation instead of just logging

### Estimated Effort

- **Replay mechanism**: 4-6 hours
  - State provider setup: 1 hour
  - EVM creation with inspector: 2 hours
  - Transaction execution loop: 1-2 hours
  - Error handling: 1 hour

- **Divergence analysis**: 2-3 hours
  - Integrate existing analysis code: 1 hour
  - Adapt for execution stage context: 1 hour
  - Testing and debugging: 1 hour

- **Testing and validation**: 2-4 hours
  - Unit tests: 1 hour
  - Integration test with real block: 1-2 hours
  - Performance verification: 1 hour

**Total: 8-13 hours** to complete implementation

### Current Blocking Issue

The main technical challenge is understanding how to:
1. Get a state provider for block N-1 from within the execution loop
2. Create an EVM instance with an inspector attached using that state
3. Execute transactions one by one and collect inspector data

This requires deeper knowledge of:
- reth's provider traits and state access patterns
- revm's EVM builder and inspector integration
- How to properly construct transaction environments

### References

- Inspector implementation: `crates/research/src/inspector.rs`
- Divergence analysis: `crates/research/src/executor.rs:140-239`
- EVM config trait: `crates/evm/evm/src/lib.rs`
- State provider: `crates/storage/provider/src/providers/`

## Summary

We've successfully built 90% of the research mode infrastructure and integrated the hook points into `ExecutionStage`. The remaining work is implementing the transaction replay mechanism with inspector attachment. This is the most technically complex part but has a clear path forward once we understand reth's state provider and EVM builder APIs better.

The code is ready for this final integration step!
