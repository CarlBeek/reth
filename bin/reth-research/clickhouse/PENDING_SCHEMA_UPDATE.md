# Pending ClickHouse schema update — producer schema v12

> Historical note: earlier versions of this file described the v9 → v10
> F-series delta and the v10 → v11 summary-taxonomy delta. Both are fully
> implemented by `migrations/002_gas_analysis_v10.up.sql` and
> `migrations/003_gas_analysis_v11.up.sql` (merged and applied to the live
> cluster), so neither is pending and both were removed from this file.
> **002 and 003 are frozen** — an applied migration file must never be edited.

The `reth-research` **producer** bumped its SQLite schema to **v12**
(`SCHEMA_VERSION` in `crates/research/src/database.rs`): a new
**unconditional per-tx gas spine**, written for every transaction rather than
only the `store_full_forensics` minority that earns a `divergences` row.

The motivation is a consumer the existing tables can't serve. The forensic
tables answer "what broke and why" for the divergent tail; a repricing
**simulator** needs the repriced gas of *every* tx, including the
byte-identical majority that previously dissolved into a `block_summaries`
class aggregate with no per-tx row at all.

The Rust export model (`TxGasResultExportRow` in
`crates/research/src/export/model.rs`) and the producer's column contract
(`DestinationTable::TxGasResult` in
`crates/research/src/export/clickhouse.rs`) already mirror this.

**Status: migration 004 is NOT yet applied.** It ships as
`migrations/004_gas_analysis_v12.{up,down}.sql`. The v12 producer's startup
`DESCRIBE TABLE` gate will fail until it is applied.

## New table — `gas_analysis_tx_gas_result`

One row per `(schedule_name, block_number, tx_index)`. Slim (no
`trace_payload`, no call frames, no opcode counts) and **uncapped** — unlike
drill-ins, which `--research.max-divergences-per-block` truncates.

**Volume warning.** Expect roughly `tx_count` rows per `(schedule, block)`,
against the divergence table's divergent-tail-only row count. On a
~200-tx/block chain with N enabled schedules this is ~200·N rows/block. Same
`PARTITION BY (chain_id, intDiv(block_number, 1000000))` and `ORDER BY` as
the divergence table, `ZSTD(1)` on every column.

### Gas columns that are NOT interchangeable

| Column | Meaning |
|---|---|
| `schedule_gas_used` | **Sender-facing**: post-refund, floor-applied. What the sender pays. |
| `schedule_total_gas_spent` | **Pre-refund.** EIP-7778 makes block-level gas accounting ignore refunds, so *this* is the figure that feeds block-fill / gas-limit analysis — **not** `schedule_gas_used`. |
| `schedule_gas_refunded` | The difference, kept explicit so neither figure has to be back-derived. |

### Full column list

| Column | Type | Meaning |
|---|---|---|
| `updated_at` | `DateTime` | `ReplacingMergeTree` version |
| `row_id` | `FixedString(66)` | Deterministic identity; framed-keccak domain `tx_gas_result`, so it never collides with the `divergence` row_id for the same tx |
| `analysis_config_hash` | `FixedString(66)` | Dataset identity |
| `chain_id` | `UInt64` | |
| `producer_schema_version` | `UInt32` | |
| `producer_git_commit` | `String` | |
| `replay_semantics` | `LowCardinality(String)` | |
| `schedule_name` | `LowCardinality(String)` | |
| `schedule_config_hash` | `FixedString(66)` | |
| `block_number` | `UInt64` | |
| `block_hash` | `FixedString(66)` | |
| `block_timestamp` | `DateTime` | |
| `tx_index` | `UInt32` | |
| `tx_hash` | `FixedString(66)` | |
| `tx_type` | `UInt8` | EIP-2718 envelope type byte |
| `tx_gas_limit` | `UInt64` | As declared by the sender |
| `max_fee_per_gas` | `String` | U256 **decimal string** — never narrowed to an integer column |
| `max_priority_fee_per_gas` | `Nullable(String)` | U256 decimal string; `NULL` for envelope types with no priority cap (legacy, access-list) |
| `baseline_success` | `Bool` | |
| `baseline_gas_used` | `UInt64` | The repricing denominator |
| `baseline_total_gas_spent` | `UInt64` | Baseline pre-refund |
| `schedule_success` | `Bool` | Succeeded within the **original** tx gas limit |
| `schedule_gas_used` | `UInt64` | Post-refund (see above) |
| `schedule_total_gas_spent` | `UInt64` | Pre-refund (see above) |
| `schedule_gas_refunded` | `UInt64` | |
| `schedule_floor_gas` | `UInt64` | Calldata floor (EIP-7623 lineage) |
| `schedule_state_gas_spent` | `UInt64` | EIP-8037 state gas; 0 for schedules that don't engage it |
| `schedule_intrinsic_gas` | `Nullable(UInt64)` | `NULL` for execution-only schedules, which have no intrinsic opinion |
| `min_multiplier_to_succeed` | `Nullable(Float64)` | `NULL` when the replay halted regardless of gas |

## `gas_analysis_block_coverage` — 1 new column

| Column | Type | Meaning |
|---|---|---|
| `block_base_fee_per_gas` | `Nullable(UInt64)` | EIP-1559 base fee from the header. `NULL` for pre-London blocks **and** for rows written before v12. |

## Consumer notes

- **Xatu is no longer needed for per-tx fee data.** Between
  `max_fee_per_gas` / `max_priority_fee_per_gas` here and
  `block_base_fee_per_gas` / `block_gas_used` / `block_gas_limit` on coverage,
  the fee-market and block-fill inputs all come from the replay itself.
- **`would_fit_in_original_limit` was deliberately not added.** v10 dropped it
  from `gas_analysis_divergence` as an exact duplicate of `schedule_success`;
  re-adding it here would reintroduce the same redundancy.
- **The two per-tx tables overlap by design.** A tx that earns a
  `gas_analysis_divergence` row also gets a `gas_analysis_tx_gas_result` row.
  Join on `(analysis_config_hash, schedule_name, block_hash, tx_index,
  tx_hash)`; do **not** union them.
- **Reorgs**: the producer clears `tx_gas_results` for the reverted range in
  the same SQLite transaction as every other per-block table. As in v1, the
  remote rows are not retracted — live-head reorg tombstones remain out of
  scope.
- **Always pin `analysis_config_hash`.** Rows written before this change have
  no `gas_analysis_tx_gas_result` rows at all and read `NULL` in
  `block_base_fee_per_gas`; v12 producers mint a **new**
  `analysis_config_hash` (the manifest embeds `producer_schema_version` and
  the git commit), so mixed-hash reads would see fake gaps.

## Versions

- SQLite `SCHEMA_VERSION`: 11 → **12** (no in-place migration: a v11
  `divergences.sqlite` is refused by the v12 binary — start fresh or rename
  aside after draining its `export_outbox` with the old binary).
- `ENVELOPE_FORMAT_VERSION`: 3 → **4** (additive `BlockOutput::tx_gas_results`
  and `BlockCoverageRow::block_base_fee_per_gas`, both `serde(default)` — v3
  outbox payloads still decode as empty / `None`; the version is provenance,
  not a decode gate).
- `TRACE_FORMAT_VERSION`: unchanged (2) — drill-in payloads are untouched.

## Rust → ClickHouse type mapping (reference)

| Rust (`model.rs`)            | ClickHouse                |
|------------------------------|---------------------------|
| `u32` / `Option<u32>`        | `UInt32` / `Nullable(UInt32)` |
| `u64` / `Option<u64>`        | `UInt64` / `Nullable(UInt64)` |
| `i64` / `Option<i64>`        | `Int64` / `Nullable(Int64)`   |
| `i32` (`Option`)             | `Nullable(Int32)`         |
| `u8` / `Option<u8>`          | `UInt8` / `Nullable(UInt8)` |
| `f64` (`Option`)             | `Nullable(Float64)`       |
| `bool` / `Option<bool>`      | `UInt8` / `Nullable(UInt8)` (or `Bool`) |
| `Vec<i32>`                   | `Array(Int32)`            |
| `String` U256 decimal        | `String` / `Nullable(String)` |
| `String` address (`0x…`, 42) | `String` / `FixedString(42)` |
| `String` selector (`0x…`, 10)| `Nullable(String)` / `Nullable(FixedString(10))` |
| `String` hex blob            | `Nullable(String)`        |
