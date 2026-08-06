# `reth-research`

Experimental tooling for replaying canonical Ethereum blocks under alternate gas schedules and
recording per-transaction divergences.

## What It Does

The current system is centered on the `reth-research` ExEx in
[`bin/reth-research`](/Users/carl/projects/reth/bin/reth-research). For each committed block:

1. It loads historical state at `block - 1`.
2. It executes each transaction once under baseline gas pricing.
3. It re-executes the same transaction once per configured execution schedule.
4. It classifies each (tx, schedule) into a storage bucket (see
   [`docs/storage-redesign.md`](docs/storage-redesign.md)) and writes the
   block's coverage + per-bucket summaries + drill-in rows to SQLite.

Execution schedules are isolated from one another: each configured schedule gets its own per-block
state so schedule-induced failures can cascade across later transactions in the same block.

## Supported Schedules

- `EIP-2780` intrinsic repricing
- `EIP-8037` native state creation gas and reservoir accounting
- `EIP-8038` state access/write repricing (cold account access, storage and account writes, create
  and access-list costs), applied on the block's native spec independently of EIP-8037
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
- `would_fit_in_original_limit`: whether the schedule's `gas_used` would still
  fit inside the transaction's original `tx_gas_limit` (i.e. the original tx
  would have survived the schedule unchanged)
- `min_multiplier_to_succeed`: smallest gas-limit multiplier (`gas_used /
  tx_gas_limit`) that lets the replay finish; `NULL` when the replay halted
  for non-gas reasons or even at the inflated limit
- `baseline_total_gas_spent` / `schedule_total_gas_spent`: pre-refund total gas
  (regular + state) for each replay
- `baseline_gas_refunded` / `schedule_gas_refunded`: raw `Gas::refunded()`
  (capped per EIP-3529) — lets you separate refund effects from raw cost
- `schedule_state_gas_spent`: net state gas charged under the schedule (initial
  + runtime, already net of EIP-7702 reservoir refund). Zero unless EIP-8037 is
  enabled
- `schedule_initial_state_gas`: state gas charged at tx start (auth-list +
  create-tx state gas, gross of EIP-7702 reservoir refund). Lets you derive
  `runtime_state_gas ≈ schedule_state_gas_spent - schedule_initial_state_gas`
- `schedule_initial_reservoir`: per-tx state-gas reservoir budget at the start
  of execution (after the initial state-gas deduction and EIP-7702 reservoir
  refund). Compare against `runtime_state_gas` to see whether the tx spilled
  state gas into regular gas
- `schedule_floor_gas`: EIP-7623 floor gas for the schedule replay

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
- `cargo check -p reth-research-bin` is expected to pass; broader workspace health should be
  validated separately from research-specific checks.

## Recommended Use

Use this crate for:

- early historical replay experiments
- spotting candidate breakage patterns
- quantifying how often a repricing changes gas or status on historical chain data

For EIP-8037 reservoir experiments, use `--research.eip8037` and optionally
`--research.gas-limit-multiplier <N>`. The multiplier only inflates gas limits for the schedule
replay (so the reservoir can fill); `would_fit_in_original_limit` and `min_multiplier_to_succeed`
then tell you whether the original tx survived the schedule unchanged and, if not, how much extra
gas it would have needed.

Do not use it as the sole basis for ship / no-ship decisions on Ethereum protocol changes.
