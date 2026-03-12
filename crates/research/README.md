# `reth-research`

Experimental tooling for replaying canonical Ethereum blocks under alternate gas schedules and
recording per-transaction divergences.

## What It Does

The current system is centered on the `reth-research` ExEx in
[`bin/reth-research`](/Users/carl/projects/reth/bin/reth-research). For each committed block:

1. It loads historical state at `block - 1`.
2. It executes each transaction once under baseline gas pricing.
3. It re-executes the same transaction once per configured execution schedule.
4. It records schedule-specific divergences to SQLite.

Execution schedules are isolated from one another: each configured schedule gets its own per-block
state so schedule-induced failures can cascade across later transactions in the same block.

## Supported Schedules

- `EIP-2780` intrinsic repricing
- CSV-driven opcode and precompile repricing
- Uniform execution gas multipliers
- Combined intrinsic + execution schedules via the shared `GasSchedule` trait

## What The ExEx Currently Records

For each schedule divergence, the live path can persist:

- baseline vs schedule success status
- baseline vs schedule gas used
- intrinsic gas deltas
- transaction category for intrinsic schedules
- affected opcode / precompile metadata
- inspector-derived operation counts
- first detected OOG / divergence location metadata
- baseline vs schedule call frames when the call tree differs
- baseline vs schedule event logs when emitted logs differ

## Current Limits

This crate is still research infrastructure, not decision-grade Ethereum analysis on its own.

- It replays already-included canonical transactions only.
- It does not model mempool admission, transaction replacement, builder behavior, or new block
  packing under alternate gas prices.
- Intrinsic repricing is chain-accurate for the transaction envelope, but live replay still relies
  on the EVM's normal transaction pipeline rather than a forked protocol implementation.
- Schedule-specific state drift is preserved within a block, but the live ExEx resets from
  canonical parent state at the start of each new block. It is therefore not a cross-block forked
  chain simulation.
- State-root or full state-diff persistence is not yet implemented in the live ExEx path.
- The crate-level `ResearchExecutor` remains as a legacy API surface and does not implement the
  full multi-schedule analysis path. Use the `reth-research` ExEx instead.
- `cargo check -p reth-research-bin` is expected to pass; broader workspace health should be
  validated separately from research-specific checks.

## Recommended Use

Use this crate for:

- early historical replay experiments
- spotting candidate breakage patterns
- quantifying how often a repricing changes gas or status on historical chain data

Do not use it as the sole basis for ship / no-ship decisions on Ethereum protocol changes.
