# `reth-research` ExEx

This binary runs an execution extension that replays committed canonical
blocks under one or more alternate gas schedules and writes per-schedule
results to a DuckDB lake. See
[`crates/research/docs/storage-redesign.md`](../../crates/research/docs/storage-redesign.md)
for the schema.

## Status

Experimental. The ExEx is usable for historical replay research, but it is
not production-ready evidence for Ethereum EIP ship decisions.

## How It Works

For each committed block at or above `--research.start-block`:

1. Build the block EVM environment.
2. Load historical state at `block - 1`.
3. Execute each transaction once under baseline pricing with
   `TrackingInspector`.
4. Re-execute the same transaction once per configured execution schedule
   with `ScheduleInspector`.
5. Classify each (tx, schedule) pair into one of eight buckets
   (`unchanged` / `trace_only` / `gas_only` / `event_logs_changed` /
   `wallet_fixable_shallow` / `wallet_fixable_deep_chain` /
   `inconclusive_needs_higher_sweep` / `contract_broken`).
6. Aggregate-only buckets roll into per-block summaries; drill-in buckets
   (`event_logs_changed`, `inconclusive_needs_higher_sweep`,
   `contract_broken`) get the full per-tx record (call frames, per-frame
   opcode counts, event logs).

Each execution-modifying schedule gets its own state view for the block,
so schedule-induced failures can affect later transactions under that
same schedule.

## Supported Flags

- `--research.eip2780`
- `--research.eip8037`
- `--research.eip8038` (state access/write repricing, 3x, native-spec; independent of `--research.eip8037`)
- `--research.csv NAME=PATH`
- `--research.multiplier NAME=MULT`
- `--research.db-path PATH` (DuckDB file)
- `--research.start-block BLOCK`
- `--research.backfill`
- `--research.backfill-min-block BLOCK`
- `--research.backfill-concurrency N`
- `--research.gas-limit-multiplier MULT`
- `--research.max-divergences-per-block N`
- `--research.metadata-backfill` (run the contract-metadata backfill in
  one-shot mode instead of starting live analysis; see below)

At least one schedule flag is required.

## Example

```bash
cargo run --release -p reth-research-bin -- node \
  --research.eip8037 \
  --research.eip8038 \
  --research.csv 7904-prelim=./schedules/7904_prelim.csv \
  --research.multiplier 4x=4 \
  --research.db-path ./divergences.sqlite \
  --research.start-block 18000000
```

`--research.eip8037` and `--research.eip8038` can run together: each is a separate schedule with its
own `eip-8037` / `eip-8038` rows, and 8038 stays on the block's native spec so it neither alters
8037's replay nor its persisted data.

## Analyze With DuckDB

Attach the producer DB directly from the DuckDB shell:

```bash
sqlite3 -readonly ./divergences.sqlite
```

Block-level incidence and gas impact:

```sql
SELECT schedule_name,
       sum(tx_count) AS txs,
       sum(tx_count_contract_broken) AS broken,
       sum(tx_count_inconclusive_needs_higher_sweep) AS needs_higher_sweep,
       sum(tx_count_wallet_fixable_shallow + tx_count_wallet_fixable_deep_chain) AS wallet_fixable
FROM block_coverage
GROUP BY 1
ORDER BY 1;
```

Highest-impact divergences for one schedule:

```sql
SELECT block_number, tx_index, bucket, gas_delta, tx_hash
FROM divergences
WHERE schedule_name = '4x'
ORDER BY abs(gas_delta) DESC
LIMIT 100;
```

Bucket mix by schedule:

```sql
SELECT schedule_name, bucket, count(*) AS rows
FROM divergences
GROUP BY 1, 2
ORDER BY 1, 3 DESC;
```

Drill into call-frame data for forensics:

```sql
SELECT d.schedule_name, d.block_number, d.tx_index,
       f.depth, f.call_type, f.to_address, f.gas_provided, f.gas_used
FROM divergences d
JOIN divergence_call_frames f USING (divergence_id)
WHERE d.bucket IN ('contract_broken', 'inconclusive_needs_higher_sweep')
ORDER BY d.block_number, d.tx_index, f.call_index
LIMIT 100;
```

## What Gets Stored

Per (schedule, block):

- `block_coverage`: tx counts split by bucket
- `block_summaries`: per-bucket aggregates (gas-delta histograms,
  sums/min/max, eventually 8037 state-gas metrics)

Per drill-in transaction (event-logs-changed, inconclusive, or contract-broken only):

- `divergences`: outcome flags, gas figures, OOG / divergence location,
  chain-walk classification (`oog_chain_proportional`,
  `oog_bottleneck_depth`, `oog_bottleneck_kind`)
- `divergence_call_frames`: one row per call frame with depth, addresses,
  gas, parent-gas / stack-gas / EIP-150 binding
- `divergence_opcode_counts`: sparse per-(frame, opcode) counts plus
  baseline and schedule gas charged for each opcode
- `divergence_event_logs`: baseline and schedule logs

Static helpers:

- `analysis_runs`: producer manifest (schema version, schedule config
  hash, reth commit, run start/end)
- `contract_metadata`: bytecode-derived metadata (solc version, CBOR
  marker), populated by `--research.metadata-backfill` (see below)

## Contract Metadata Backfill

Once the producer has populated `divergence_call_frames` with some
drill-in cohort, run:

```bash
cargo run --release -p reth-research-bin -- node \
  --research.eip2780 \
  --research.db-path ./divergences.sqlite \
  --research.metadata-backfill
```

The binary opens the producer DB, walks every distinct `to_address` in
`divergence_call_frames`, fetches deployed bytecode from reth state,
hashes it for a codehash, parses the Solidity CBOR trailer, and UPSERTs
a row into `contract_metadata`. It's idempotent and resumable — re-runs
skip codehashes already in the DB.

After the backfill completes the process exits; the node launch is just
to get a provider handle on reth state. At least one schedule flag is
still required because reth-research's CLI parser shares args with the
live mode.

## Important Limits

- This is canonical historical replay, not a simulation of how users or
  builders would adapt.
- State-root comparison is not yet persisted.
- A schedule that lowers intrinsic gas is conservatively modelled in the
  execution replay because the baseline EVM transaction pipeline is being
  reused.
- DuckDB foreign-key enforcement isn't a great fit for the producer's
  per-block transactional delete pattern; referential integrity across
  the drill-in tables is maintained at the application layer.
