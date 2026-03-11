# `reth-research` ExEx

This binary runs an execution extension that replays committed canonical blocks under one or more
alternate gas schedules and writes schedule divergences to SQLite.

## Status

Experimental. The ExEx is usable for historical replay research, but it is not production-ready
evidence for Ethereum EIP ship decisions.

## How It Works

For each committed block at or above `--research.start-block`:

1. Build the block EVM environment.
2. Load historical state at `block - 1`.
3. Execute each transaction once under baseline pricing with `TrackingInspector`.
4. Re-execute the same transaction once per configured execution schedule with
   `ScheduleInspector`.
5. Record any divergence in gas, status, call tree, or emitted logs.

Each execution-modifying schedule gets its own state view for the block, so schedule-induced
failures can affect later transactions under that same schedule.

## Supported Flags

- `--research.eip2780`
- `--research.csv NAME=PATH`
- `--research.multiplier NAME=MULT`
- `--research.db-path PATH`
- `--research.start-block BLOCK`

At least one schedule flag is required.

## Example

```bash
cargo run --release -p reth-research -- node \
  --research.eip2780 \
  --research.csv 7904-prelim=./schedules/7904_prelim.csv \
  --research.multiplier 4x=4 \
  --research.db-path ./divergences.db \
  --research.start-block 18000000
```

## What Gets Stored

The SQLite `schedule_divergences` table stores, per schedule and transaction:

- baseline and schedule success
- baseline and schedule gas used
- intrinsic gas data
- gas delta and gas efficiency ratio
- tx category for intrinsic schedules
- affected opcode / precompile metadata
- OOG and first-divergence metadata
- serialized call frames when the call tree diverges
- serialized event logs when emitted logs diverge

## Important Limits

- This is canonical historical replay, not a simulation of how users or builders would adapt.
- State-root comparison is not yet persisted in the live ExEx path.
- A schedule that lowers intrinsic gas can still be conservatively modeled in the execution replay
  because the baseline EVM transaction pipeline is being reused.
- Full verification is currently blocked by unrelated workspace compile failures in
  [`receipt.rs`](/Users/carl/projects/reth/crates/ethereum/primitives/src/receipt.rs).
