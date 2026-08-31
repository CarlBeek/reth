# Storage redesign — producer side

Companion to `docs/storage-redesign.md` in the `repricing-forensics`
repo. This doc covers what changes in the producer (this crate).

**Historical (schema v9 era).** It records why the storage model looks the way
it does — SQLite over DuckDB, aggregates over per-tx rows for the silent
majority — and the DDL it quotes is the v9 shape. The schema has moved since:
v10 deleted the editorial bucket taxonomy this doc describes (`bucket` columns,
wallet-fixable / contract-broken cohorts) in favour of a two-value
execution-fact `class`, and v12 added the opt-in per-tx `tx_gas_results`
table (`--research.tx-gas-results`). For the current schema read the DDL in
`crates/research/src/database.rs`; its `SCHEMA_VERSION` doc comment carries the
version-by-version history.

> **Engine update (post-deploy)**: we tried DuckDB as the single
> writer+reader file. It hit DuckDB's hard cross-process lock — when
> reth holds the writer no other process can attach, even read-only.
> Pivoted to **SQLite (writer) + DuckDB sqlite_scanner (consumer
> reader)**: SQLite WAL handles 1 writer + N readers natively, and
> DuckDB's vectorized engine runs the consumer's analytical queries
> through the SQLite storage. Same logical schema, different writer
> engine. `SCHEMA_VERSION` bumped to 2 for the SQLite incarnation.

## Why

Three things land together:

1. **One on-disk file, no lock conflict.** Producer writes
   `divergences.sqlite` in WAL mode; consumer attaches the same file
   read-only via DuckDB's `sqlite_scanner`. SQLite WAL is the gold
   standard for OLTP-write + OLAP-read concurrency at our scale.

2. **Bucket assignment moves into the producer.** The legacy consumer
   derived "wallet-fixable vs contract-broken" from
   `oog_chain_proportional` plus a SQL heuristic. The new schema has
   the producer tag each tx at write time; the consumer trusts the
   tag.

3. **Granularity rebalance.** Per-tx data for wallet-fixable / gas-only
   / trace-only txs (most of the volume) collapses into per-block
   aggregates. Per-frame opcode counts and call-frame metadata get
   added for the drill-in cohort.

## What changes in this crate

| Module | Change |
|---|---|
| `database.rs` | now rusqlite-backed. WAL pragmas at open time, JSON for array/struct columns (histograms, per-opcode totals), INTEGER PRIMARY KEY AUTOINCREMENT for surrogate IDs. Public type renamed `DivergenceDatabase` (was `DuckDbDivergenceDatabase`); error renamed `DatabaseError`. |
| `multi_schedule_inspector.rs` | inspector emits per-(frame, opcode) counts in addition to per-tx totals |
| `oog_chain.rs` | unchanged behavior; consumed by the bucket classifier |
| `export.rs` | retired — parquet export is no longer the consumer's input format |
| `cli.rs` | adds `contract-metadata-backfill` subcommand |
| `divergence.rs` | `Bucket` enum + per-tx classifier function |
| `analyzer.rs` / `comparison.rs` | wires bucket assignment into the write path; aggregate-only buckets only update the in-memory block aggregator, not `divergences` |

## Bucket classifier

One function, runs once per replayed tx after both baseline and
schedule traces finish:

```rust
pub enum Bucket {
    Unchanged,           // outcome + traces match
    TraceOnly,           // trace differs, outcome identical
    GasOnly,             // status_changed=false, gas_delta>0
    EventLogsChanged,        // status_changed=false, event_logs differ
    WalletFixableShallow,    // status flipped, root/outer-limit gas failure
    WalletFixableDeepChain,  // status flipped, oog_chain_proportional=true, not shallow
    InconclusiveNeedsHigherSweep, // status flipped, OOG at highest sweep tier
    ContractBroken,          // status flipped, neither wallet-fixable form
}
```

`WalletFixableShallow` covers root-frame OOG and the tier-sweep case where a
higher outer-gas replay succeeds, matches baseline observable behavior, and only
fails the original schedule-success check because the computed gas exceeds the
tx's original limit. `WalletFixableDeepChain` is the chain-walk rescue.
`InconclusiveNeedsHigherSweep` means no throttled call-chain bottleneck was proven,
but the highest configured replay tier still halted OOG; rerun with a higher
`--research.gas-limit-multipliers` ceiling before deciding.

Storage rule per bucket:

- `Unchanged` — nothing written. Counted in `block_coverage.tx_count`.
- `TraceOnly` / `GasOnly` / `WalletFixableShallow` / `WalletFixableDeepChain`
  — increments `block_summaries.{bucket}.*`. No row in `divergences`.
- `EventLogsChanged` / `InconclusiveNeedsHigherSweep` / `ContractBroken` —
  full per-tx record in `divergences`, plus `divergence_call_frames`,
  `divergence_opcode_counts`, `divergence_event_logs`.

Inspector keeps the per-frame opcode counter running regardless of
bucket, since the bucket isn't known until both traces complete. When
the tx classifies as aggregate-only, the per-frame data is summed into
the block aggregator's running opcode totals and discarded.

## Schema

Canonical DDL lives in `crates/research/src/database.rs` (function
`initialize_schema`). Tables:

- `block_coverage(schedule_name, schedule_config_hash, block_number,
  block_hash, parent_hash, timestamp, tx_count, tx_count_unchanged,
  tx_count_trace_only, tx_count_gas_only, tx_count_event_logs_changed,
  tx_count_wallet_fixable_shallow, tx_count_wallet_fixable_deep_chain,
  tx_count_inconclusive_needs_higher_sweep, tx_count_contract_broken)`
- `block_summaries(schedule_name, block_number, bucket, tx_count,
  gas_delta_sum, gas_delta_sum_sq REAL, gas_delta_min, gas_delta_max,
  gas_delta_log2_hist TEXT/JSON, opcode_totals_7904 TEXT/JSON
  (sparse list of {opcode, count, gas_baseline, gas_schedule}),
  state_gas_sum,
  state_gas_spillover_sum, multiplier_log2_hist TEXT/JSON,
  tx_count_creation, tx_count_authorization, tx_count_runtime_state,
  tx_count_no_state)`
- `divergences(divergence_id INTEGER PRIMARY KEY AUTOINCREMENT,
  schedule_name, schedule_config_hash, block_number, tx_index,
  tx_hash BLOB, timestamp, bucket, sender, recipient, is_create,
  tx_gas_limit, baseline_success, schedule_success, status_changed,
  event_logs_changed, output_changed, logs_bloom_changed,
  baseline_gas_used, schedule_gas_used, gas_delta,
  baseline_total_gas_spent, baseline_gas_refunded,
  schedule_total_gas_spent, schedule_gas_refunded,
  schedule_intrinsic_gas, schedule_floor_gas,
  would_fit_in_original_limit, min_multiplier_to_succeed REAL,
  divergence_contract, divergence_pc, divergence_call_depth,
  divergence_opcode, oog_contract, oog_pc, oog_call_depth, oog_opcode,
  oog_pattern, oog_gas_remaining, oog_chain_proportional,
  oog_bottleneck_depth, oog_bottleneck_kind,
  schedule_state_gas_spent, schedule_initial_state_gas,
  schedule_initial_reservoir, runtime_state_gas,
  runtime_state_gas_spillover, state_gas_category,
  reservoir_exhausted, UNIQUE(schedule_name, block_number, tx_index,
  schedule_config_hash))`
- `divergence_call_frames(divergence_id, call_index,
  parent_call_index, depth, from_address, to_address, code_address,
  codehash BLOB, call_type, selector BLOB, value_wei, gas_provided,
  gas_used, gas_margin, success, parent_gas_at_call,
  gas_requested_on_stack, eip150_cap_binding, state_gas_running,
  PRIMARY KEY (divergence_id, call_index))`
- `divergence_opcode_counts(divergence_id, call_index, opcode, count,
  gas_baseline, gas_schedule, PRIMARY KEY (divergence_id, call_index,
  opcode))` — sparse; zero rows omitted at insert time
- `divergence_event_logs(divergence_id, trace_kind, log_index,
  address, topic0..topic3 BLOB, data_bytes BLOB, data_hash BLOB,
  PRIMARY KEY (divergence_id, trace_kind, log_index))`
- `contract_metadata(codehash BLOB PRIMARY KEY,
  representative_address, solc_version, solc_commit, evm_target,
  cbor_present, has_metadata_hash, bytecode_len, extracted_at)`
- `analysis_runs(run_id INTEGER PRIMARY KEY AUTOINCREMENT,
  schema_version, schedule_name, schedule_config_hash, reth_commit,
  run_started_at, run_finished_at, blocks_processed, notes)`

Column-type notes:
- SQLite is dynamically typed. Affinity hints in the DDL document
  intent. All numeric values bind as INTEGER/REAL.
- BOOLEAN columns store INTEGER 0/1; rusqlite's `ToSql for bool`
  handles the conversion.
- Array/struct columns (histograms, per-opcode totals) serialize as
  JSON TEXT. The consumer reads them via DuckDB's `json_each` /
  `json_extract` over `sqlite_scanner`.
- `gas_delta_sum_sq` is REAL (loses precision past 2^53). The dashboard
  derives variance/stddev from it; precision loss is acceptable for
  display.

Indexes:

```sql
CREATE INDEX idx_div_schedule       ON divergences(schedule_name);
CREATE INDEX idx_div_block          ON divergences(schedule_name, block_number);
CREATE INDEX idx_div_recipient      ON divergences(recipient);
CREATE INDEX idx_div_bucket         ON divergences(bucket);
CREATE INDEX idx_div_sched_bucket_recipient ON divergences(schedule_name, bucket, recipient);
CREATE INDEX idx_div_sched_bucket_oog       ON divergences(schedule_name, bucket, oog_call_depth, oog_bottleneck_kind);
CREATE INDEX idx_div_sched_bucket_opcode    ON divergences(schedule_name, bucket, divergence_opcode);
CREATE INDEX idx_dcf_to_addr        ON divergence_call_frames(to_address);
CREATE INDEX idx_dcf_codehash       ON divergence_call_frames(codehash);
CREATE INDEX idx_doc_opcode         ON divergence_opcode_counts(opcode);
CREATE INDEX idx_bs_schedule_block  ON block_summaries(schedule_name, block_number);
CREATE INDEX idx_bs_schedule_bucket ON block_summaries(schedule_name, bucket);
```

PRAGMAs set at `Connection::open` time (see
`DivergenceDatabase::initialize`):

```
journal_mode = WAL          -- 1 writer + N readers across processes
synchronous  = NORMAL       -- durable on commit, no per-page fsync
foreign_keys = OFF          -- producer manages referential integrity at app layer
temp_store   = MEMORY
```

## Schema versioning

`schema_version` is a single integer baked into `analysis_runs`. Bump
it on any column add/remove/type change. No migration shims — a major
schema change is a full re-replay.

The producer refuses to open a DB whose latest `analysis_runs.schema_version`
doesn't match its compiled-in version. The consumer warns and reads
anyway (so older replays remain inspectable in the dashboard during a
migration window).

Schema `v10` adds the ClickHouse export outbox (`analysis_manifests` +
`export_outbox`); the version check fails closed across **both**
`analysis_runs` and `analysis_manifests`, so an export-enabled v10 binary
rejects a v9 DB (and vice versa). There is no in-place migration — an
export-enabled deployment replays into a fresh database.

## ClickHouse export outbox (optional)

When `--research.export-config-path` is set, each `record_block_output` also
writes one `export_outbox` row **in the same transaction** as the analytical
rows, so there is no crash window between local persistence and the export
request. An embedded worker drains the outbox and ships rows to ClickHouse over
HTTPS; remote failures never touch replay or the SQLite writer (they grow the
outbox, bounded by `max_pending_bytes`).

The export path lives entirely in `crates/research/src/export/` and is keyed by
a deterministic `analysis_config_hash` (a manifest hash over the full schedule
set + normalized gas tiers + drill-in cap + commit + chain id). `ReplacingMergeTree`
plus deterministic row IDs make at-least-once delivery idempotent. The immutable
`analysis_manifests` row lets a restarted process export a pending item under the
manifest it was produced with, independent of the current CLI configuration. See
`bin/reth-research/README.md` and `bin/reth-research/clickhouse/migrations` for
the operator-facing contract.

## Per-frame opcode capture

The current `tracking_inspector.rs` records a single tx-level
`OperationCounts`. The new version maintains a stack of frame-keyed
counters:

```rust
struct FrameCounters {
    call_index: u32,
    counts: [u64; 256],       // by opcode byte
    gas_baseline: [u64; 256], // baseline cost × count
    gas_schedule: [u64; 256], // schedule cost × count
}
```

Pushed on `step()` when a CALL/CREATE family opcode opens a new
frame, popped on return. On pop, the frame's counters are emitted
(or staged for emission, since we don't yet know the bucket).

Cost: 256 × 3 × 8 bytes = 6 KB per frame. A typical tx has < 20
frames; even a pathological 200-frame tx is 1.2 MB transient memory.

Emission strategy: buffer all per-frame counters for a tx in memory,
classify the tx (Bucket), then:
- For drill-in buckets: `INSERT INTO divergence_opcode_counts ...`,
  skipping opcode rows where `count == 0`.
- For aggregate buckets: sum into the block aggregator.

## Block-summary aggregation

A `BlockAggregator` is created at `start_block`, fed each tx's
classification + opcode totals at tx-end, and flushed to
`block_summaries` at `end_block`. Owns:

- per-bucket counters (count, gas_delta sum/min/max, log2 hist)
- per-bucket opcode totals (sparse map, keyed by opcode byte)
- per-category 8037 counters

Zero memory cost when no divergences in a block — the aggregator is
created empty and flushed as a no-op (only `block_coverage` updates).

## Contract-metadata sidecar

New subcommand `cargo run --bin contract-metadata-backfill`. Reads
the producer DB:

```sql
SELECT DISTINCT codehash, representative_address
FROM (
    SELECT codehash, FIRST(to_address) AS representative_address
    FROM divergence_call_frames
    WHERE codehash IS NOT NULL
    GROUP BY codehash
)
WHERE codehash NOT IN (SELECT codehash FROM contract_metadata)
```

For each, fetch the bytecode from reth state (the producer has access
to this via the normal reth APIs), parse the CBOR trailer, and
UPSERT into `contract_metadata`. Runs out-of-band, idempotent, can
be interrupted and resumed.

CBOR-decode reference: the last 2 bytes of bytecode are the CBOR
length (big-endian); the preceding `len` bytes are the CBOR map.
Common keys: `solc` (3 bytes: major, minor, patch), `ipfs` or
`bzzr1` (metadata hash). EVM target is sometimes encoded; when
absent, fall back to "unknown".

## Migration order (historical)

The producer side has been migrated (DuckDB attempt → SQLite). The
remaining work is on the consumer side: deploy reth-research with the
new `database.rs`, then verify the consumer dashboard reads correctly
against the live SQLite file via `sqlite_scanner`.

## Risks & open issues

- **Per-frame counters on memory-heavy txs.** 200-frame txs at 6 KB
  per frame is fine; a malicious 10K-frame tx would be 60 MB. Cap
  the frame buffer and emit an `OperationCountsTruncated` flag
  rather than panicking.
- **EOF / future opcodes.** `[u64; 256]` indexes by opcode byte and
  handles every byte value, so EOF additions are free. EOF function
  references inside a single contract don't open new frames, so the
  frame stack remains a sound model.
- **Storage size of `divergence_opcode_counts`.** Sparse: ~5-20
  nonzero opcodes per frame, ~10-50 frames per drill-in tx, ~100K
  drill-in txs across the dataset = 5–100M rows. SQLite's WAL +
  row-oriented storage will be larger on disk than a columnar
  alternative, but for our scale (tens of millions of rows) it's well
  within "single-file workable".
- **Bytecode availability for old contracts.** Some self-destructed
  contracts no longer have bytecode in state. `contract_metadata`
  has a NULL row for them; consumer should handle gracefully.
- **WAL file checkpointing.** WAL grows as writes accumulate; SQLite
  auto-checkpoints by default on commit when WAL exceeds ~1000 pages
  (`PRAGMA wal_autocheckpoint`). For long-running replays we may want
  to explicitly checkpoint between block batches to keep WAL bounded.

## Out of scope (intentionally)

- Writing the producer DB from multiple concurrent processes.
  Single writer is fine; we already serialize.
- Streaming export to S3 / object storage. Easy to add later as a
  `snapshot_to_parquet` helper; not needed for the consumer
  rewrite.
- 4byte selector resolution. Stays a frontend concern (openchain
  lookup at render time).
