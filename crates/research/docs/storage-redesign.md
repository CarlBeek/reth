# Storage redesign — producer side

Companion to `docs/storage-redesign.md` in the `repricing-forensics`
repo. This doc covers what changes in the producer (this crate).

## Why

Three things land together:

1. **Engine swap: SQLite → DuckDB.** The consumer (`repricing-forensics`)
   needs columnar analytics over the same data the producer writes.
   Today we maintain SQLite on this side, parquet exports in the
   middle, and a derived DuckDB on the consumer side — three copies
   of the same data on disk. Switching this crate to DuckDB makes
   reth's DB the single source of truth that the consumer can read
   directly.

2. **Bucket assignment moves into the producer.** The current consumer
   derives "wallet-fixable vs contract-broken" from
   `oog_chain_proportional` plus a SQL heuristic. With the new schema
   the producer tags each tx at write time and the consumer trusts
   the tag.

3. **Granularity rebalance.** Per-tx data for wallet-fixable / gas-only
   / trace-only txs (most of the volume) collapses into per-block
   aggregates. Per-frame opcode counts and call-frame metadata get
   added for the drill-in cohort.

## What changes in this crate

| Module | Change |
|---|---|
| `database.rs` | rewritten against DuckDB (or new `database_duckdb.rs` alongside; SQLite version retired in step 6 below) |
| `multi_schedule_inspector.rs` | inspector now emits per-(frame, opcode) counts in addition to per-tx totals |
| `oog_chain.rs` | unchanged behavior; consumed by the new bucket classifier |
| `export.rs` | retired — parquet export is no longer the consumer's input format. Keep a thin `snapshot_to_parquet` helper for offline distribution |
| `cli.rs` | new subcommand `contract-metadata-backfill` |
| `divergence.rs` | new `Bucket` enum and per-tx classifier function |
| `analyzer.rs` / `comparison.rs` | wire bucket assignment into the write path; for aggregate-only buckets, write only to the in-memory block aggregator, not to `divergences` |

## Bucket classifier

One function, runs once per replayed tx after both baseline and
schedule traces finish:

```rust
pub enum Bucket {
    Unchanged,           // outcome + traces match
    TraceOnly,           // trace differs, outcome identical
    GasOnly,             // status_changed=false, gas_delta>0
    EventLogsChanged,    // status_changed=false, event_logs differ
    WalletFixableShallow,    // status flipped, depth ≤ 1, no internal calls
    WalletFixableDeepChain,  // status flipped, oog_chain_proportional=true, not shallow
    ContractBroken,      // status flipped, neither wallet-fixable form
}
```

`WalletFixableShallow` is the legacy heuristic (kept because it's a
useful distinction in the UI, but applies even when the chain-walk
classifier wasn't run — root-frame OOG with no subcalls is trivially
proportional). `WalletFixableDeepChain` is the chain-walk rescue.

Storage rule per bucket:

- `Unchanged` — nothing written. Counted in `block_coverage.tx_count`.
- `TraceOnly` / `GasOnly` / `WalletFixableShallow` / `WalletFixableDeepChain`
  — increments `block_summaries.{bucket}.*`. No row in `divergences`.
- `EventLogsChanged` / `ContractBroken` — full per-tx record in
  `divergences`, plus `divergence_call_frames`, `divergence_opcode_counts`,
  `divergence_event_logs`.

Inspector keeps the per-frame opcode counter running regardless of
bucket, since the bucket isn't known until both traces complete. When
the tx classifies as aggregate-only, the per-frame data is summed into
the block aggregator's running opcode totals and discarded.

## DuckDB schema

DDL sketch. Use this as the column-level spec; types are DuckDB native.

```sql
-- One row per (schedule, block). Always emitted, even for blocks with
-- zero divergences, so coverage joins work.
CREATE TABLE block_coverage (
    schedule_name        VARCHAR NOT NULL,
    schedule_config_hash VARCHAR NOT NULL,
    block_number         UBIGINT NOT NULL,
    block_hash           BLOB    NOT NULL,
    parent_hash          BLOB    NOT NULL,
    timestamp            UBIGINT NOT NULL,
    tx_count             UINTEGER NOT NULL,
    -- counts per bucket for fast headline queries
    tx_count_unchanged                   UINTEGER NOT NULL,
    tx_count_trace_only                  UINTEGER NOT NULL,
    tx_count_gas_only                    UINTEGER NOT NULL,
    tx_count_event_logs_changed          UINTEGER NOT NULL,
    tx_count_wallet_fixable_shallow      UINTEGER NOT NULL,
    tx_count_wallet_fixable_deep_chain   UINTEGER NOT NULL,
    tx_count_contract_broken             UINTEGER NOT NULL,
    PRIMARY KEY (schedule_name, block_number, block_hash)
);

-- One row per (schedule, block, bucket). Only emitted for buckets that
-- had at least one tx in the block.
CREATE TABLE block_summaries (
    schedule_name VARCHAR NOT NULL,
    block_number  UBIGINT NOT NULL,
    bucket        VARCHAR NOT NULL,         -- one of the Bucket values
    tx_count      UINTEGER NOT NULL,
    -- gas-delta moments + histogram
    gas_delta_sum         BIGINT,
    gas_delta_sum_sq      HUGEINT,           -- BIGINT can overflow for big aggregates
    gas_delta_min         BIGINT,
    gas_delta_max         BIGINT,
    gas_delta_log2_hist   INTEGER[12],       -- bins for 0,1,2..2^11+ gas
    -- 7904: opcode totals across this bucket in this block (sparse list)
    opcode_count_totals_7904 STRUCT(opcode UTINYINT, count UBIGINT)[],
    opcode_gas_delta_totals_7904 STRUCT(opcode UTINYINT, delta BIGINT)[],
    -- 8037: state-gas totals + per-category tx counts
    state_gas_sum           UBIGINT,
    state_gas_spillover_sum UBIGINT,
    multiplier_log2_hist    INTEGER[12],     -- bins for 1.0x, 1.25x, 1.5x, 2x, ... > 8x
    tx_count_creation              UINTEGER,
    tx_count_authorization         UINTEGER,
    tx_count_runtime_state         UINTEGER,
    tx_count_no_state              UINTEGER,
    PRIMARY KEY (schedule_name, block_number, bucket)
);

-- One row per drill-in divergence. event_logs_changed + contract_broken
-- only. Identity is the surrogate divergence_id (sequence); the natural
-- key is (schedule_name, block_number, tx_index).
CREATE SEQUENCE seq_divergence_id START 1;
CREATE TABLE divergences (
    divergence_id        UBIGINT PRIMARY KEY DEFAULT nextval('seq_divergence_id'),
    schedule_name        VARCHAR NOT NULL,
    schedule_config_hash VARCHAR NOT NULL,
    block_number         UBIGINT NOT NULL,
    tx_index             UINTEGER NOT NULL,
    tx_hash              BLOB NOT NULL,
    timestamp            UBIGINT NOT NULL,
    bucket               VARCHAR NOT NULL,

    -- Parties
    sender    VARCHAR NOT NULL,
    recipient VARCHAR,
    is_create BOOLEAN NOT NULL,
    tx_gas_limit UBIGINT NOT NULL,

    -- Outcomes
    baseline_success BOOLEAN NOT NULL,
    schedule_success BOOLEAN NOT NULL,
    status_changed       BOOLEAN NOT NULL,
    event_logs_changed   BOOLEAN NOT NULL,
    output_changed       BOOLEAN NOT NULL,
    logs_bloom_changed   BOOLEAN NOT NULL,

    -- Gas
    baseline_gas_used     UBIGINT NOT NULL,
    schedule_gas_used     UBIGINT NOT NULL,
    gas_delta             BIGINT NOT NULL,
    baseline_total_gas_spent UBIGINT,
    baseline_gas_refunded    UBIGINT,
    schedule_total_gas_spent UBIGINT,
    schedule_gas_refunded    UBIGINT,
    schedule_intrinsic_gas   UBIGINT,
    schedule_floor_gas       UBIGINT,
    would_fit_in_original_limit BOOLEAN,
    min_multiplier_to_succeed   DOUBLE,

    -- 7904 OOG
    divergence_contract  VARCHAR,
    divergence_pc        UINTEGER,
    divergence_call_depth INTEGER,
    divergence_opcode    UTINYINT,
    oog_contract         VARCHAR,
    oog_pc               UINTEGER,
    oog_call_depth       INTEGER,
    oog_opcode           UTINYINT,
    oog_pattern          VARCHAR,
    oog_gas_remaining    UBIGINT,
    oog_chain_proportional BOOLEAN,
    oog_bottleneck_depth   INTEGER,
    oog_bottleneck_kind    VARCHAR,

    -- 8037 state gas
    schedule_state_gas_spent     UBIGINT,
    schedule_initial_state_gas   UBIGINT,
    schedule_initial_reservoir   UBIGINT,
    runtime_state_gas            UBIGINT,
    runtime_state_gas_spillover  UBIGINT,
    state_gas_category           VARCHAR,
    reservoir_exhausted          BOOLEAN,

    UNIQUE (schedule_name, block_number, tx_index, schedule_config_hash)
);

-- One row per call frame in the schedule trace of a drill-in
-- divergence. Baseline frames are NOT stored separately — derived
-- counts (gas_baseline) live in divergence_opcode_counts.
CREATE TABLE divergence_call_frames (
    divergence_id        UBIGINT NOT NULL REFERENCES divergences(divergence_id),
    call_index           UINTEGER NOT NULL,   -- 0 = root
    parent_call_index    UINTEGER,            -- NULL for root
    depth                UINTEGER NOT NULL,
    from_address         VARCHAR NOT NULL,
    to_address           VARCHAR NOT NULL,
    code_address         VARCHAR,              -- != to for DELEGATECALL
    codehash             BLOB,                 -- for joining to contract_metadata
    call_type            VARCHAR NOT NULL,     -- CALL/DELEGATECALL/STATICCALL/CALLCODE/CREATE/CREATE2
    selector             BLOB,                 -- first 4 bytes of input, NULL for create
    value_wei            VARCHAR,              -- u256 as text
    gas_provided         UBIGINT NOT NULL,
    gas_used             UBIGINT NOT NULL,
    gas_margin           BIGINT,               -- gas_provided - gas_used under baseline
    success              BOOLEAN NOT NULL,
    -- EIP-150 chain-walk inputs
    parent_gas_at_call       UBIGINT,
    gas_requested_on_stack   UBIGINT,
    eip150_cap_binding       BOOLEAN,
    -- 8037: running state gas at frame exit (cumulative across the tx)
    state_gas_running    UBIGINT,
    PRIMARY KEY (divergence_id, call_index)
);

-- Sparse opcode counts keyed by frame. Zero rows omitted. Keys are
-- per-frame so we can answer "which contract burned the KECCAKs".
CREATE TABLE divergence_opcode_counts (
    divergence_id   UBIGINT NOT NULL,
    call_index      UINTEGER NOT NULL,
    opcode          UTINYINT NOT NULL,
    count           UBIGINT NOT NULL,
    gas_baseline    UBIGINT NOT NULL,        -- gas this opcode would have cost under baseline
    gas_schedule    UBIGINT NOT NULL,        -- gas it actually cost under the schedule
    PRIMARY KEY (divergence_id, call_index, opcode),
    FOREIGN KEY (divergence_id, call_index)
        REFERENCES divergence_call_frames(divergence_id, call_index)
);

CREATE TABLE divergence_event_logs (
    divergence_id  UBIGINT NOT NULL REFERENCES divergences(divergence_id),
    trace_kind     VARCHAR NOT NULL,         -- 'baseline' | 'schedule'
    log_index      UINTEGER NOT NULL,
    address        VARCHAR NOT NULL,
    topic0         BLOB,
    topic1         BLOB,
    topic2         BLOB,
    topic3         BLOB,
    data_bytes     BLOB,
    data_hash      BLOB,                     -- keccak(data) for fast diff
    PRIMARY KEY (divergence_id, trace_kind, log_index)
);

-- Static contract metadata, keyed by codehash so identical
-- implementations dedupe across addresses.
CREATE TABLE contract_metadata (
    codehash         BLOB PRIMARY KEY,
    representative_address VARCHAR,           -- one address with this codehash
    solc_version     VARCHAR,                 -- e.g. "0.8.21"
    solc_commit      VARCHAR,                 -- from CBOR if present
    evm_target       VARCHAR,                 -- "london", "shanghai", etc., when extractable
    cbor_present     BOOLEAN NOT NULL,
    has_metadata_hash BOOLEAN NOT NULL,       -- ipfs/bzzr hash in CBOR
    bytecode_len     UINTEGER NOT NULL,
    extracted_at     UBIGINT NOT NULL
);

-- One row per analysis run. Lets a consumer detect "is this lake
-- written by the current code or do I need to migrate?".
CREATE TABLE analysis_runs (
    run_id               UBIGINT PRIMARY KEY,
    schema_version       UINTEGER NOT NULL,   -- bumped on any schema change
    schedule_name        VARCHAR NOT NULL,
    schedule_config_hash VARCHAR NOT NULL,
    reth_commit          VARCHAR,
    run_started_at       UBIGINT NOT NULL,
    run_finished_at      UBIGINT,
    blocks_processed     UBIGINT,
    notes                VARCHAR
);
```

Indexes:

```sql
CREATE INDEX idx_div_schedule       ON divergences(schedule_name);
CREATE INDEX idx_div_block          ON divergences(schedule_name, block_number);
CREATE INDEX idx_div_recipient      ON divergences(recipient);
CREATE INDEX idx_div_bucket         ON divergences(bucket);
CREATE INDEX idx_dcf_to_addr        ON divergence_call_frames(to_address);
CREATE INDEX idx_dcf_codehash       ON divergence_call_frames(codehash);
CREATE INDEX idx_doc_opcode         ON divergence_opcode_counts(opcode);
CREATE INDEX idx_bs_schedule_block  ON block_summaries(schedule_name, block_number);
```

DuckDB doesn't strictly need indexes for analytical scans, but the
per-recipient and per-codehash lookups (contract page, clustering
queries) benefit.

## Schema versioning

`schema_version` is a single integer baked into `analysis_runs`. Bump
it on any column add/remove/type change. No migration shims — a major
schema change is a full re-replay.

The producer refuses to open a DB whose latest `analysis_runs.schema_version`
doesn't match its compiled-in version. The consumer warns and reads
anyway (so older replays remain inspectable in the dashboard during a
migration window).

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

## Migration order

Same sequence as in the consumer doc:

1. Implement new DuckDB module in this crate (behind feature flag).
2. Wire the `Bucket` classifier into the inspector pipeline.
3. Add per-frame opcode capture.
4. Add `contract-metadata-backfill` subcommand.
5. Smoke-test a single schedule end-to-end on a small block range
   (e.g. the 100-block reservoir we use in tests).
6. Once the consumer has ported its endpoints, retire the SQLite
   module and the parquet exporter.

## Risks & open issues

- **DuckDB Rust client maturity.** The `duckdb` crate (v1.x) is
  reasonable but has fewer eyes than `rusqlite`. Validate
  concurrent read-while-write before committing.
- **MVCC during long replays.** A multi-day replay holding a single
  long-running transaction will pin temp space. Need to commit per-
  block (or per N blocks) and confirm DuckDB releases checkpoints
  cleanly.
- **Per-frame counters on memory-heavy txs.** 200-frame txs at 6 KB
  per frame is fine; a malicious 10K-frame tx would be 60 MB. Cap
  the frame buffer and emit an `OperationCountsTruncated` flag
  rather than panicking.
- **EOF / future opcodes.** `[u64; 256]` indexes by opcode byte and
  handles every byte value, so EOF additions are free. EOF function
  references inside a single contract don't open new frames, so the
  frame stack remains a sound model.
- **Storage size of `divergence_opcode_counts`.** Sparse sparse:
  ~5-20 nonzero opcodes per frame, ~10-50 frames per drill-in tx,
  ~100K drill-in txs across the dataset = 5–100M rows. DuckDB
  compresses this aggressively (long runs of same opcode int).
  Estimate < 5 GB at the upper bound.
- **Bytecode availability for old contracts.** Some self-destructed
  contracts no longer have bytecode in state. `contract_metadata`
  has a NULL row for them; consumer should handle gracefully.

## Out of scope (intentionally)

- Writing the producer DB from multiple concurrent processes.
  Single writer is fine; we already serialize.
- Streaming export to S3 / object storage. Easy to add later as a
  `snapshot_to_parquet` helper; not needed for the consumer
  rewrite.
- 4byte selector resolution. Stays a frontend concern (openchain
  lookup at render time).
