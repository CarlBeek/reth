# Pending ClickHouse schema update

The `reth-research` **producer** was rebased onto the merged EIP-8038 / F-series
research schema (the analytical SQLite tables `block_coverage` / `block_summaries`
/ `divergences` were reshaped). The Rust export model (`crates/research/src/export/model.rs`)
and the producer's column contract (`DestinationTable::required_columns()` in
`crates/research/src/export/clickhouse.rs`) have been updated to mirror it.

**The ClickHouse warehouse schema has NOT been changed by this PR** — neither the
migration files in this directory (`migrations/001_gas_analysis.up.sql` /
`.down.sql`) nor the live cluster. That update is owned by the ClickHouse
maintainer (**@mattevans**) and must be applied before export is enabled.

Until the cluster matches, this is **safe**: the worker's startup `DESCRIBE TABLE`
check (`ClickHouseClient`, driven by `required_columns()`) refuses to export to a
table missing any required column, so no rows are shipped against a stale schema.

**Authoritative target:** the `required_columns()` allow-lists + the
`CoverageRow` / `SummaryRow` / `DivergenceExportRow` structs in `model.rs`. Apply
the delta below to `001_gas_analysis.up.sql` (and the live cluster), keeping the
existing `ReplicatedReplacingMergeTree(..., updated_at)` engine, the `Distributed`
wrappers, `ON CLUSTER '{cluster}'`, and the `ORDER BY` identity tuples (except the
one rename noted).

## `gas_analysis_block_coverage`
- **DROP** the 8 editorial-bucket columns (the taxonomy was removed):
  `tx_count_trace_only`, `tx_count_event_logs_changed`, `tx_count_schedule_rescued`,
  `tx_count_wallet_fixable_shallow`, `tx_count_wallet_fixable_deep_chain`,
  `tx_count_inconclusive_needs_higher_sweep`, `tx_count_contract_broken`,
  `tx_count_aa_gas_reestimation`.
- **KEEP** `tx_count_unchanged`, `tx_count_gas_only`.
- **ADD** `tx_count_stored UInt32`, `block_gas_used UInt64`, `block_gas_limit UInt64`.
- `ORDER BY` unchanged.

## `gas_analysis_block_summary`
- **RENAME** `bucket` → `class` (`LowCardinality(String)`), and update the table's
  `ORDER BY (..., block_hash, bucket)` → `(..., block_hash, class)`.
- **ADD** (all `Nullable(UInt64)`): `cold_account_access_count`; the F8 storage
  drivers `sload_cold_count`, `sload_warm_count`, `sstore_cold_count`,
  `sstore_set_count`, `sstore_reset_count`, `sstore_clear_count`,
  `sstore_noop_count`, `sstore_dirty_count`; the F2/F3 account drivers
  `warm_account_access_count`, `value_transfer_count`, `create_opcode_count`,
  `access_list_address_count`, `access_list_storage_key_count`.

## `gas_analysis_divergence`
- **DROP** `bucket` (not part of the `ORDER BY`, so index-safe — the merged model
  has no per-tx class).
- **RENAME/REPLACE** `would_fit_in_original_limit` → `outer_limit_only_failure`
  (`Nullable(UInt8)`; the old column was an exact duplicate of `schedule_success`
  and was dropped from the source — the new column is a distinct witness).
- **ADD** the full-parity scalar columns (the producer now mirrors every
  `divergences` scalar). Types follow the mapping table below:
  - F2 cold-access / F4 surcharge: `cold_account_access_count Nullable(UInt64)`,
    `additional_gas_charged Nullable(Int64)`, `surcharge_at_oog Nullable(Int64)`.
  - F6 selector path: `failure_selector_path Nullable(String)`.
  - F5 tx shape: `tx_type Nullable(UInt8)`, `tx_nonce Nullable(UInt64)`,
    `entry_selector Nullable(String)` (`0x`-hex), `input_zero_bytes Nullable(UInt64)`,
    `input_nonzero_bytes Nullable(UInt64)`, `has_authorization Nullable(UInt8)`.
  - F1/F2 failure: `failure_reason Nullable(String)`, `revert_data Nullable(String)`
    (`0x`-hex), `revert_decoded Nullable(String)`, `tx_output Nullable(String)` (`0x`-hex).
  - F7 baseline frame: `baseline_frame_success Nullable(UInt8)`,
    `baseline_frame_gas_used Nullable(UInt64)`, `baseline_frame_gas_provided Nullable(UInt64)`.
  - F10 first-gas-divergence: `gas_div_contract Nullable(String)`,
    `gas_div_pc Nullable(UInt32)`, `gas_div_call_depth Nullable(Int32)`,
    `gas_div_opcode Nullable(UInt8)`.
  - F12 tax decomposition: `tax_second_db_read Nullable(Int64)`,
    `tax_other Nullable(Int64)`, `tax_intrinsic Nullable(Int64)`.
  - F8 storage drivers (all `Nullable(UInt64)`): `sload_cold_count`,
    `sload_warm_count`, `sstore_cold_count`, `sstore_set_count`,
    `sstore_reset_count`, `sstore_clear_count`, `sstore_noop_count`,
    `sstore_dirty_count`.
  - F2/F3 account drivers (all `Nullable(UInt64)`): `warm_account_access_count`,
    `value_transfer_count`, `create_opcode_count`, `access_list_address_count`,
    `access_list_storage_key_count`.
  - F1 tier-1 (1×) forensics: `tier1_failure_reason Nullable(String)`,
    `tier1_oog_opcode Nullable(UInt8)`, `tier1_oog_contract Nullable(String)`,
    `tier1_oog_pc Nullable(UInt32)`, `tier1_oog_depth Nullable(Int32)`,
    `tier1_oog_gas_remaining Nullable(UInt64)`,
    `tier1_failing_selector Nullable(String)` (`0x`-hex),
    `tier1_failing_gas_provided Nullable(UInt64)`,
    `tier1_failing_gas_requested Nullable(UInt64)`.
- The `trace_*` columns and the `block_*` / identity columns are unchanged.

## Rust → ClickHouse type mapping
| Rust (`model.rs`)            | ClickHouse                |
|------------------------------|---------------------------|
| `u32` / `Option<u32>`        | `UInt32` / `Nullable(UInt32)` |
| `u64` / `Option<u64>`        | `UInt64` / `Nullable(UInt64)` |
| `i64` / `Option<i64>`        | `Int64` / `Nullable(Int64)`   |
| `i32` (`Option`)             | `Nullable(Int32)`         |
| `u8` (`Option`)              | `Nullable(UInt8)`         |
| `bool` / `Option<bool>`      | `UInt8` / `Nullable(UInt8)` (or `Bool`) |
| `String` address (`0x…`, 42) | `String` / `FixedString(42)` |
| `String` selector (`0x…`, 10)| `Nullable(String)` / `Nullable(FixedString(10))` |
| `String` hex blob            | `Nullable(String)`        |

## Outbox payload versions (FYI — producer side, already applied)
`ENVELOPE_FORMAT_VERSION` and `TRACE_FORMAT_VERSION` were bumped `1 → 2`
(`model.rs`). The `trace_format_version` column in `gas_analysis_divergence` will
carry `2` for rows produced after this change.
