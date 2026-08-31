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

- `--research.amsterdam` (the whole repricing stack — EIP-2780 + 7976 + 7981 + 8037 + 8038 — via
  revm's native `SpecId::AMSTERDAM`, so the recorded gas reflects the EIPs interacting rather than a
  sum of independent single-EIP deltas)
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
- `--research.export-config-path PATH` (enable durable ClickHouse export; see
  [ClickHouse Export](#clickhouse-export))

At least one schedule flag is required.

## Example

```bash
cargo run --release -p reth-research-bin -- node \
  --research.amsterdam \
  --research.csv 7904-prelim=./schedules/7904_prelim.csv \
  --research.multiplier 4x=4 \
  --research.db-path ./divergences.sqlite \
  --research.start-block 18000000
```

`--research.amsterdam` replays the stack as one lane rather than as isolated per-EIP lanes, which
is the only way to see the interaction effect: a call that only runs out of gas *because* of the new
intrinsic cost changes which cold/warm accesses happen downstream, so the composite result is not
the sum of independent single-EIP deltas.

The lane overrides no gas values — it switches the spec and lets revm charge. That is sound only
while revm's Amsterdam table matches `execution-specs`, which
`crates/research/tests/amsterdam_matches_execution_specs.rs` asserts (constants, composed table
slots, per-arm intrinsic, and the EIP-8038 EXT* surcharge measured against a real EVM). A revm bump
that renumbers Amsterdam fails those tests and re-keys the dataset through the schedule's
`config_fingerprint`, which reads the same constants.

The composite schedule's constants are ported from `ethereum/execution-specs` branch
`forks/amsterdam` and locked by unit tests, because revm's own baked-in `AMSTERDAM` gas table is
several devnet iterations stale. A devnet renumber breaks those tests and re-keys the dataset via
`config_fingerprint` rather than silently mixing rows priced under different rules.

## Analyze With DuckDB

Attach the producer DB directly from the DuckDB shell:

```bash
sqlite3 -readonly ./divergences.sqlite
```

Block-level incidence and gas impact:

```sql
SELECT schedule_name,
       sum(tx_count)           AS txs,
       sum(tx_count_stored)    AS stored,    -- failures + trace divergences (per-tx rows)
       sum(tx_count_gas_only)  AS gas_only,  -- gas changed, trace identical
       sum(tx_count_unchanged) AS unchanged  -- byte-identical to baseline
FROM block_coverage
GROUP BY 1
ORDER BY 1;
```

The producer no longer applies an editorial taxonomy (wallet-fixable /
contract-broken / …). Every failure and trace divergence gets a full per-tx
`divergences` row; those cohorts are re-derived downstream from the raw stored
facts (`baseline_success`, `schedule_success`, OOG chain-walk, `code_address`,
selector path, etc.).

Highest-impact stored divergences for one schedule:

```sql
SELECT block_number, tx_index, gas_delta, schedule_success, tx_hash
FROM divergences
WHERE schedule_name = '4x'
ORDER BY abs(gas_delta) DESC
LIMIT 100;
```

Re-derive the failure cohorts from raw facts:

```sql
SELECT schedule_name,
       sum(baseline_success AND NOT schedule_success) AS breaks,
       sum(NOT baseline_success AND schedule_success)  AS rescues,
       sum(outer_limit_only_failure)                   AS outer_limit_only
FROM divergences
GROUP BY 1
ORDER BY 1;
```

Drill into call-frame data for forensics (all stored divergences are failures or
trace divergences):

```sql
SELECT d.schedule_name, d.block_number, d.tx_index,
       f.depth, f.call_type, f.to_address, f.gas_provided, f.gas_used
FROM divergences d
JOIN divergence_call_frames f USING (divergence_id)
WHERE NOT d.schedule_success
ORDER BY d.block_number, d.tx_index, f.call_index
LIMIT 100;
```

## What Gets Stored

Per (schedule, block):

- `block_coverage`: tx counts split by execution-fact class
  (`unchanged` / `gas_only` / `stored`)
- `block_summaries`: per-class aggregates (gas-delta histograms — absolute
  log2 and 13-bin percentage of baseline, sums/min/max, the class's
  baseline-gas denominator, EIP-2718 tx-type counts, envelope-shape counts
  (creation / simple-transfer / contract-call), opcode totals, 8037
  state-gas + 8038 cold-account metrics), keyed by
  `class ∈ {unchanged, gas_only}`
- `block_recipients`: top-K recipient/selector attribution per `class`

Per transaction, unconditionally — **every** tx, not just the divergent ones:

- `tx_gas_results`: the slim per-tx gas spine a repricing simulator needs.
  Gas limit, fee caps (`max_fee_per_gas` / `max_priority_fee_per_gas` as U256
  decimal strings), baseline vs schedule gas, and the schedule's
  intrinsic / floor / state-gas figures. Two gas columns that are **not**
  interchangeable: `schedule_gas_used` is sender-facing (post-refund,
  floor-applied), while `schedule_total_gas_spent` is pre-refund — the
  figure EIP-7778 block-level accounting uses. Unlike the drill-in tables
  below this is never truncated by `--research.max-divergences-per-block`.

Per stored transaction (every failure + every trace divergence):

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
  --research.amsterdam \
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

## ClickHouse Export

Optional. When `--research.export-config-path` points at a TOML config, every
analyzed block output is also shipped to ClickHouse. SQLite remains the local
source of truth; ClickHouse is a downstream copy.

### Architecture

```text
block replay -> BlockOutput
  -> SQLite analytical rows + export_outbox row (one transaction)
  -> embedded async export worker
  -> ClickHouse HTTPS JSONEachRow inserts
```

The outbox row is written in the **same SQLite transaction** as the analytical
rows, so there is no crash window where a block is durable locally but its
export request is lost. An embedded worker drains the outbox and inserts the run
manifest, divergences, summaries, then **coverage last** (the remote completion
marker). Delivery is at-least-once; deterministic row IDs plus ClickHouse
`ReplacingMergeTree` make re-sends idempotent, so a crash mid-flight is recovered
by simply retrying.

Remote outages never block replay or the SQLite writer. They grow the outbox on
disk instead; that is observable (logged pending count/bytes/age) and bounded by
`max_pending_bytes` — once exceeded the process stops loudly rather than grow
without bound.

### Config

The destination schema lives in
[`clickhouse/migrations`](./clickhouse/migrations) — golang-migrate up/down
pairs that create a dedicated `gas_analysis` database with
`ReplicatedReplacingMergeTree` local tables + `Distributed` wrappers,
`ON CLUSTER '{cluster}'`. An example config is in
[`clickhouse/config.example.toml`](./clickhouse/config.example.toml):

```toml
endpoint = "https://clickhouse.example.org:8443"   # must be https://
database = "gas_analysis"                            # defaults to gas_analysis
username = "gas_analysis"
password_env = "CLICKHOUSE_PASSWORD"                 # password read from env, never a flag
# ... batching/retry/backlog tunables, optional ca_cert_path ...
```

The password is **never** a CLI argument or a value in the file — it is resolved
once at startup from the environment variable named by `password_env`. Grant the
ingest account only `INSERT` on the four destination tables plus the metadata
read needed for the startup `DESCRIBE TABLE` schema check.

Apply the migrations in `clickhouse/migrations` to your cluster first (column
names and types are the fixed producer contract). Then:

```bash
export CLICKHOUSE_PASSWORD=...
cargo run --release -p reth-research-bin -- node \
  --research.amsterdam \
  --research.db-path ./divergences.sqlite \
  --research.export-config-path ./clickhouse/config.example.toml
```

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
