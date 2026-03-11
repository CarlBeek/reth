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

## Export To Parquet

For analytics, export the SQLite database to Parquet and query it with DuckDB:

```bash
cargo run --release -p reth-research-bin --bin reth-research-export-parquet -- \
  --db-path ./divergences.db \
  --out-dir ./research_lake \
  --block-bucket-size 100000
```

This produces:

- `block_coverage/`
- `divergences_hot/`
- `divergence_artifacts/`
- `_manifest.json`
- `_checkpoint.json`

Each dataset is partitioned by encoded `schedule_name` and `block_bucket`, so common
schedule-scoped and block-range analysis can avoid full table scans. Partition values escape
special characters, for example `foo/bar` becomes `schedule_name=foo~2Fbar`.

By default the exporter is incremental and snapshot-correct: each run rewrites the current
schedule partitions from SQLite, so updated or deleted rows are reflected without leaving stale
Parquet files behind. Use `--full-refresh` to rebuild the entire output tree from scratch.

## Analyze With DuckDB

The intended analysis path is DuckDB over the exported Parquet datasets, not ad hoc scans over the
SQLite write store.

Open DuckDB:

```bash
duckdb
```

Inspect block-level incidence and gas impact:

```sql
SELECT
    schedule_name,
    sum(tx_count) AS txs,
    sum(divergence_count) AS divergent_txs,
    sum(total_gas_delta) AS total_gas_delta
FROM read_parquet('research_lake/block_coverage/schedule_name=*/block_bucket=*/*.parquet')
GROUP BY 1
ORDER BY 1;
```

Find the highest-impact divergences for one schedule:

```sql
SELECT
    block_number,
    tx_index,
    divergence_type,
    gas_delta,
    tx_hash
FROM read_parquet('research_lake/divergences_hot/schedule_name=4x/block_bucket=*/*.parquet')
ORDER BY abs(gas_delta) DESC
LIMIT 100;
```

Measure divergence mix by type:

```sql
SELECT
    schedule_name,
    divergence_type,
    count(*) AS rows
FROM read_parquet('research_lake/divergences_hot/schedule_name=*/block_bucket=*/*.parquet')
GROUP BY 1, 2
ORDER BY 1, 3 DESC;
```

Drill into full artifacts only when needed:

```sql
SELECT
    h.schedule_name,
    h.block_number,
    h.tx_index,
    a.operation_counts,
    a.oog_info,
    a.divergence_location
FROM read_parquet('research_lake/divergences_hot/schedule_name=*/block_bucket=*/*.parquet') h
JOIN read_parquet('research_lake/divergence_artifacts/schedule_name=*/block_bucket=*/*.parquet') a
  USING (divergence_id)
WHERE h.status_changed
ORDER BY h.block_number, h.tx_index
LIMIT 50;
```

Recommended workflow:

- use `block_coverage` for schedule-level rates and block-range summaries
- use `divergences_hot` for tx-level filtering and ranking
- join `divergence_artifacts` only for forensic inspection
- keep exports incremental during replay, then run `--full-refresh` when you need a clean rebuild

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
