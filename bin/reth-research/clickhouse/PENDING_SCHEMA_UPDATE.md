# Pending ClickHouse schema update — producer schema v11

> Historical note: the previous version of this file described the v9 → v10
> F-series delta. That delta is fully implemented by
> `migrations/002_gas_analysis_v10.up.sql` (merged and applied to the live
> cluster), so it is no longer pending and was removed from this file.

The `reth-research` **producer** bumped its SQLite schema to **v11**
(`SCHEMA_VERSION` in `crates/research/src/database.rs`): additive
per-`(schedule_name, block_number, class)` transaction-taxonomy and
relative-gas columns on `block_summaries`, requested by the dashboard because
the aggregate-only cohorts (`unchanged` / `gas_only`) have no per-tx rows to
derive them from.
The Rust export model (`SummaryRow` in `crates/research/src/export/model.rs`)
and the producer's column contract (`DestinationTable::required_columns()` in
`crates/research/src/export/clickhouse.rs`) already mirror it.

**Status: migration 003 was applied to the live cluster on 2026-07-13**
(schema_migrations version 3, clean) — all ten columns AND both
`COMMENT COLUMN` corrections verified live via `system.columns`. The v11
producer's startup `DESCRIBE TABLE` gate now passes.

**003 is frozen.** An applied migration file must never be edited (the
cluster's golang-migrate state records it as done, so edits would silently
diverge from reality). Any future gas_analysis schema delta ships as a new
`004_*` pair in this directory.

## `gas_analysis_block_summary` — 10 new columns

| Column | Type | Meaning |
|---|---|---|
| `tx_count_type_legacy` | `Nullable(UInt32)` | Aggregated txs with EIP-2718 type 0 |
| `tx_count_type_access_list` | `Nullable(UInt32)` | type 1 (EIP-2930) |
| `tx_count_type_dynamic_fee` | `Nullable(UInt32)` | type 2 (EIP-1559) |
| `tx_count_type_blob` | `Nullable(UInt32)` | type 3 (EIP-4844) |
| `tx_count_type_set_code` | `Nullable(UInt32)` | type 4 (EIP-7702) |
| `tx_count_type_other` | `Nullable(UInt32)` | any other type — the six always sum to `tx_count` |
| `tx_count_simple_transfer` | `Nullable(UInt32)` | non-create txs with empty calldata |
| `tx_count_contract_call` | `Nullable(UInt32)` | non-create txs with non-empty calldata |
| `gas_delta_pct_hist` | `Array(Int32)` | 13 closed-left bins of `100·gas_delta/baseline_gas_used`, edges `[-100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf)`; bin sum == `tx_count`; `[]` on pre-v11 rows (`Nullable(Array)` is illegal in ClickHouse — empty is the missing marker, as for the existing hist columns) |
| `baseline_gas_used_sum` | `Nullable(UInt64)` | Σ baseline gas over the class's txs — class-grain denominator for ratio-of-sums against `gas_delta_sum` |

The migration also **corrects two v10 column comments**: `tx_count_creation`
and `tx_count_authorization` count **tx-level** facts (envelope create-kind,
i.e. `to IS NULL`, and the presence of an EIP-7702 authorization list) — the
old comments claimed "state op", which misled at least one consumer into
believing a tx-level creation count was missing.

## Consumer notes

- **The requested `tx_count_contract_creation` was NOT added** — it already
  exists as `tx_count_creation` (tx-level create count per class). Partition
  law: `tx_count_creation + tx_count_simple_transfer + tx_count_contract_call
  == tx_count`.
- **Naming**: the request called the histogram `gas_diff_pct_hist`; it ships
  as `gas_delta_pct_hist` for consistency with the `gas_delta_*` family. Same
  semantics as requested.
- `tx_count_simple_transfer` is **envelope-shape** classification: an
  empty-calldata send *to a contract* (receive()/fallback) still counts, and
  EIP-7702 set-code txs — as well as empty-calldata blob txs — land here too
  (cross-filter with `tx_count_type_set_code` / `tx_count_type_blob`).
  EOA-only transfer detection would need state knowledge the aggregator
  doesn't have.
- These columns cover the **aggregated** (non-drill-in) cohorts only; stored
  txs carry per-tx `tx_type` etc. on `gas_analysis_divergence`.
- **Always pin `analysis_config_hash`.** Rows written before this change read
  `NULL` / `[]` in the new columns; v11 producers mint a **new**
  `analysis_config_hash` (the manifest embeds `producer_schema_version` and
  the git commit), so mixed-hash reads would see fake empties.

## Versions

- SQLite `SCHEMA_VERSION`: 10 → **11** (no in-place migration: a v10
  `divergences.sqlite` is refused by the v11 binary — start fresh or rename
  aside after draining its `export_outbox` with the old binary).
- `ENVELOPE_FORMAT_VERSION`: 2 → **3** (additive `BlockSummaryRow` fields, all
  `serde(default)` — v2 outbox payloads still decode; the version is
  provenance, not a decode gate).
- `TRACE_FORMAT_VERSION`: unchanged (2) — drill-in payloads are untouched.

## Rust → ClickHouse type mapping (reference; the `Vec<i32>` row is new in v11)

| Rust (`model.rs`)            | ClickHouse                |
|------------------------------|---------------------------|
| `u32` / `Option<u32>`        | `UInt32` / `Nullable(UInt32)` |
| `u64` / `Option<u64>`        | `UInt64` / `Nullable(UInt64)` |
| `i64` / `Option<i64>`        | `Int64` / `Nullable(Int64)`   |
| `i32` (`Option`)             | `Nullable(Int32)`         |
| `u8` (`Option`)              | `Nullable(UInt8)`         |
| `bool` / `Option<bool>`      | `UInt8` / `Nullable(UInt8)` (or `Bool`) |
| `Vec<i32>`                   | `Array(Int32)`            |
| `String` address (`0x…`, 42) | `String` / `FixedString(42)` |
| `String` selector (`0x…`, 10)| `Nullable(String)` / `Nullable(FixedString(10))` |
| `String` hex blob            | `Nullable(String)`        |
