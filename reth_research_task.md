# Reth Gas Price Research Modification - Task Specification

## Context

I'm proposing an EIP that will change the gas costs for nearly all opcodes and precompiles on Ethereum. Before finalizing this proposal, I need empirical data on how sensitive real-world contract executions are to significant gas price changes.

This research fork of reth will execute historical mainnet transactions under both current and modified gas schedules to identify behavioral changes, helping demonstrate the real-world impact of the proposed EIP for an ethresear.ch post and EIP discussion.

## Research Objectives

**Primary Question:** Under significantly higher gas prices (~128x current levels), how do transactions behave differently?

**Specific Interests:**
- Do transactions take different execution paths (different branches, loops)?
- Do hardcoded gas stipends (2300 for value transfers, explicit `.gas()` calls) cause failures?
- Do gas-dependent loops (`while(gasleft() > X)`) behave differently?
- Which contracts/patterns are most sensitive to gas pricing changes?
- What percentage of transactions show behavioral changes vs just higher costs?

## High-Level Requirements

### Dual Execution System

Execute every transaction twice in each block:

1. **Normal Execution** (standard gas schedule)
   - Proceeds as usual
   - Updates chain state
   - Affects P2P, RPC, all external interfaces
   - This is the "real" execution

2. **Experimental Execution** (modified gas schedule)
   - Runs in parallel with modified gas costs
   - State is discarded after comparison
   - Completely transparent to the network
   - Used only for divergence detection

### Gas Schedule Modifications

**For experimental execution:**

- Multiply ALL gas costs by a configurable factor (default: 128x):
  - Opcode execution costs
  - Intrinsic transaction cost (21,000 base)
  - Memory expansion costs
  - Precompile costs
  - Storage operation costs (SLOAD/SSTORE)

- **EXCEPTION:** Keep the 2300 gas stipend at 2300 (don't multiply)
  - This hardcoded value is a key research interest
  - Changes here would reveal brittle assumptions

- **Gas refunds:** Keep at normal amounts by default
  - Make this configurable

### Gas Limit Handling

To prevent trivial out-of-gas failures:

- Inflate transaction gas limits by the same multiplier (default: 128x)
- **Ignore** block-level gas limits in experimental execution
- **Ignore** transaction-level gas limits in experimental execution
  - Unless the normal execution also hit OOG
- Goal: Only catch behavioral changes, not artificial limit errors

### Divergence Detection

Compare experimental vs normal execution and identify when ANY of these differ:

1. **State Root**: Post-transaction state diverged
2. **Execution Trace**: Different opcodes executed or different control flow
3. **Status**: Success/revert status differs
4. **Event Logs**: Different events emitted (LOG0-4 operations)
5. **Call Tree**: Different external calls made (CALL/DELEGATECALL/STATICCALL)
6. **Gas Pattern**: Significantly different operation counts (structural difference)

Record which type(s) of divergence occurred for each transaction.

## Data Collection Schema

### Primary Divergence Database

For each transaction showing divergence, collect:

**Identity:**
- Block number
- Transaction index
- Transaction hash
- Timestamp

**Divergence Classification:**
- Divergence types present: state_root | event_logs | status | call_tree | gas_pattern
- Multiple types can be present simultaneously

**Gas Analysis:**
- `normal_gas_used`: Gas consumed in normal execution
- `experimental_gas_used`: Gas consumed in experimental execution
- `gas_efficiency_ratio`: (experimental_gas / multiplier) / normal_gas
  - Values near 1.0: Same execution path, just more expensive
  - Values ≠ 1.0: Different execution path taken (key finding!)

**Operation Counts** (both executions):
- `sload_count`: Number of SLOAD operations
- `sstore_count`: Number of SSTORE operations
- `call_count`: Number of external calls (CALL/DELEGATECALL/STATICCALL)
- `log_count`: Number of LOG operations
- `total_ops`: Total opcodes executed
- `memory_words_allocated`: Peak memory usage

**Divergence Location:**
- `divergence_contract`: Contract address where divergence first occurred
- `divergence_function_selector`: First 4 bytes of calldata (if available)
- `divergence_pc`: Program counter at divergence point
- `divergence_call_depth`: Call depth when divergence occurred
- `divergence_opcode`: The opcode where execution diverged

**Call Trees** (if divergence involves external calls):
- For both executions, store: `from → to, call_type, depth, gas_provided, success`
- This reveals changes in inter-contract interactions

**Event Logs** (if logs differ):
- Store both normal and experimental event logs
- Include: contract address, topics, data

**Out-of-Gas Analysis** (if experimental hit OOG despite inflated limits):
- `oog_opcode`: Opcode that caused OOG
- `oog_pc`: Program counter location
- `oog_contract`: Contract address
- `oog_call_depth`: How deep in the call stack
- `oog_gas_remaining`: Gas remaining when OOG occurred
- `oog_pattern`: Detected pattern (loop | memory_expansion | call_chain | storage_heavy)

### Gas-Sensitive Loop Database (Separate)

Detect and log contracts using gas-dependent loop patterns:
- `while(gasleft() > X)`
- `for(...; gasleft() > X; ...)`

For each detection:
- Contract address
- Function selector
- Block number and transaction where first detected
- Approximate loop bounds observed
- Pattern specifics (gas threshold used)

## Configuration

Provide CLI flags and/or environment variables:

```bash
--research-mode=<bool>              # Enable research mode (default: false)
--gas-multiplier=<u64>              # Multiply gas costs (default: 128)
--research-start-block=<u64>        # Block height to start (default: 0)
--refund-multiplier=<f64>           # Multiply refunds (default: 1.0)
--stipend-multiplier=<f64>          # Multiply 2300 stipend (default: 1.0)
--divergence-db-path=<path>         # Divergence data storage path
--loop-detection-db-path=<path>     # Gas-loop detection storage path
--divergence-trace-detail=<level>   # minimal | standard | detailed
```

## Operational Requirements

### Network Compatibility
- Node must remain fully functional for P2P networking
- Must gossip blocks correctly
- Must respond to RPC requests accurately
- Must participate in consensus normally
- **Only normal execution affects any external interface**

### Performance Requirements
- Must keep up with chain tip on a high-spec machine
  - Many CPU cores available
  - Abundant RAM available
  - Fast NVMe storage available
- Should work during initial sync (processing historical blocks)
- Should work in follow mode (processing new blocks as they arrive)

### Block Range Support
- Support any block since the last hard fork
- Don't worry about pre-merge or ancient hard fork edge cases
- Focus on recent Ethereum mainnet (post-Dencun/Pectra era)

## Implementation Guidance

### Exploration Phase (Complete This First)

Before writing any code, please explore the reth codebase and provide a summary of:

1. **Transaction Execution Pipeline:**
   - Where does reth execute transactions?
   - Trace the flow: block import → transaction execution → state updates
   - Identify the key structs/traits involved

2. **Gas Metering:**
   - Where are gas costs defined? (likely in `revm` dependency)
   - How are gas schedules applied?
   - Where are gas limits checked?
   - Can we create a modified gas schedule easily?

3. **State Management:**
   - Where are state roots calculated?
   - How is state committed after transaction execution?
   - Can we easily create a temporary state fork?

4. **Tracing Infrastructure:**
   - Does reth have built-in execution tracing?
   - Can we capture operation counts (SLOADs, SSTOREs, CALLs)?
   - Can we capture call trees and event logs?

5. **Storage/Database:**
   - What database does reth use? (likely RocksDB)
   - Can we easily add a separate DB for research data?
   - What's the best format for our divergence data?

### Architecture Proposal (Do This Second)

After exploration, propose an architecture covering:

1. **Integration Point:**
   - Where should we inject the parallel execution?
   - Should we hook into an existing trait or wrap the executor?
   - Show specific function signatures

2. **Gas Schedule Modification:**
   - How do we create a modified gas schedule?
   - Is it a simple multiplier or do we need deep changes?
   - How do we handle the 2300 stipend exception?

3. **State Isolation:**
   - How do we fork state for experimental execution?
   - How do we efficiently discard experimental state?
   - Any performance concerns?

4. **Divergence Detection:**
   - How do we capture all necessary data from both executions?
   - Where do we perform the comparison?
   - How do we minimize performance impact?

5. **Data Storage:**
   - What database/format for divergence data?
   - Schema design (suggest using SQLite for easy analysis later?)
   - How to handle high write throughput?

**Please propose 2-3 architectural approaches and recommend one with justification.**

### Implementation Priorities

After we agree on architecture, implement in this order:

1. **Gas schedule modification** - Get the multiplier working
2. **Basic parallel execution** - Execute twice, even if we don't compare yet
3. **Divergence detection** - Compare state roots and status
4. **Data collection** - Store divergence data
5. **Advanced analysis** - Operation counts, call trees, loop detection
6. **Configuration** - CLI flags and parameters
7. **Testing** - Verify on known blocks
8. **Optimization** - Make it fast enough

## Testing Strategy

For validation, please:

1. **Test on known blocks:**
   - Pick a few recent mainnet blocks
   - Run with 2x multiplier first (should show minimal divergence)
   - Run with 128x multiplier
   - Verify divergence detection works

2. **Create test cases:**
   - Simple transfer (should not diverge)
   - Contract with hardcoded `.gas(X)` call (should diverge)
   - Contract with `gasleft()` loop (should diverge)
   - Complex DeFi transaction (interesting to see)

3. **Performance testing:**
   - Measure sync speed with research mode on/off
   - Verify node keeps up with chain tip
   - Check database growth rate

## Anticipated Challenges

Please consider and address:

1. **Performance:** Double execution will slow sync. Can we parallelize across CPU cores?

2. **Memory expansion:** With 128x cost multiplier, memory operations become extremely expensive. May need higher gas limit multiplier?

3. **State management:** Forking and discarding state efficiently without memory leaks

4. **Loop detection:** Heuristically detecting `gasleft()` checks - what's practical?

5. **Data volume:** Could generate GB of divergence data. Need efficient storage and compression?

6. **Revm integration:** Changes to gas schedule might require forking `revm` or using internal APIs

## Success Criteria

The implementation is successful when:

1. ✅ Node syncs normally with research mode enabled
2. ✅ Every transaction executes twice with different gas schedules
3. ✅ Divergences are detected and recorded accurately
4. ✅ Database contains analyzable data with the specified schema
5. ✅ Node performance is acceptable (keeps up with chain tip)
6. ✅ Normal node operation is unaffected (P2P, RPC work correctly)
7. ✅ Can process historical blocks and live blocks

## Expected Outputs for Analysis

The collected data will enable analysis showing:

- **Quantitative impact:** "X% of transactions showed behavioral changes under 128x gas prices"
- **Impact categories:** "Main failure modes: hardcoded gas stipends (Y%), gas loops (Z%), call depth (W%)"
- **Specific examples:** "Popular protocol Q would fail due to pattern P"
- **Resilience data:** "A% of transactions unaffected, showing robust gas handling"
- **Gas sensitivity distribution:** Histogram of gas_efficiency_ratio values

This analysis will form the empirical evidence section of the EIP discussion and ethresear.ch post.

## Questions and Edge Cases

Please raise questions if you encounter:

- Edge cases in gas calculation
- Unusual execution patterns that don't fit the schema
- Performance bottlenecks
- Database design trade-offs
- Alternative approaches that might work better

## Notes

- This is a research fork, not production code - prioritize correctness and data quality over extreme optimization
- Feel free to suggest improvements to the schema or approach
- The goal is empirical evidence for EIP impact analysis
- We can iterate on the implementation as we learn from initial results

---

**Ready to begin?** Please start with the exploration phase and report your findings before proposing an architecture.
