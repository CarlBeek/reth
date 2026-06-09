-- Canonical ClickHouse schema for reth-research embedded export.
--
-- This file is the producer/consumer COLUMN CONTRACT. The reth-research binary
-- inserts JSONEachRow rows whose field names and types match the columns below,
-- and verifies a subset of them with `DESCRIBE TABLE` at startup (see
-- crates/research/src/export/clickhouse.rs `required_columns`). Column names and
-- types must not drift from crates/research/src/export/model.rs.
--
-- The engine here is single-node `ReplacingMergeTree(updated_at)`. For a
-- clustered deployment, swap each local table to
-- `ReplicatedReplacingMergeTree(updated_at)` and add the matching `Distributed`
-- wrappers and `ON CLUSTER` clauses; the exporter inserts into the distributed
-- table names. Column names and types are fixed; engine paths are not.
--
-- Identity model: each row carries deterministic IDs (see model.rs). Re-sending
-- a block is idempotent — `ReplacingMergeTree` collapses duplicates by the
-- ORDER BY key, keeping the row with the greatest `updated_at`. Replacement is
-- eventual; consumers needing immediate exactness should use an `argMax` view or
-- `FINAL` selectively.
--
-- Hashes are `0x`-prefixed lowercase hex: FixedString(66) for 32-byte hashes,
-- FixedString(42) for 20-byte addresses. DateTime columns are inserted as Unix
-- second integers.

-- One row per deterministic analysis configuration.
CREATE TABLE IF NOT EXISTS reth_research_run
(
    updated_at              DateTime,
    analysis_config_hash    FixedString(66),
    chain_id                UInt64,
    producer_schema_version UInt32,
    producer_git_commit     String,
    replay_semantics        LowCardinality(String),
    manifest_json           String CODEC(ZSTD(3))
)
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (chain_id, analysis_config_hash);

-- One row per (analysis config, schedule, block hash). Coverage is the remote
-- completion marker: it is inserted LAST, so a coverage row implies the block's
-- summary and divergence rows already landed.
CREATE TABLE IF NOT EXISTS reth_research_block_coverage
(
    updated_at                               DateTime,
    row_id                                   FixedString(66),
    analysis_config_hash                     FixedString(66),
    chain_id                                 UInt64,
    producer_schema_version                  UInt32,
    producer_git_commit                      String,
    replay_semantics                         LowCardinality(String),
    schedule_name                            LowCardinality(String),
    schedule_config_hash                     FixedString(66),
    block_number                             UInt64,
    block_hash                               FixedString(66),
    parent_hash                              FixedString(66),
    block_timestamp                          DateTime,
    tx_count                                 UInt32,
    tx_count_unchanged                       UInt32,
    tx_count_trace_only                      UInt32,
    tx_count_gas_only                        UInt32,
    tx_count_event_logs_changed              UInt32,
    tx_count_schedule_rescued                UInt32,
    tx_count_wallet_fixable_shallow          UInt32,
    tx_count_wallet_fixable_deep_chain       UInt32,
    tx_count_inconclusive_needs_higher_sweep UInt32,
    tx_count_contract_broken                 UInt32,
    tx_count_aa_gas_reestimation             UInt32,
    expected_drill_in_count                  UInt32,
    retained_drill_in_count                  UInt32,
    drill_ins_truncated                      Bool
)
ENGINE = ReplacingMergeTree(updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_number, block_hash);

-- One row per non-empty (analysis config, schedule, block, bucket).
CREATE TABLE IF NOT EXISTS reth_research_block_summary
(
    updated_at              DateTime,
    row_id                  FixedString(66),
    analysis_config_hash    FixedString(66),
    chain_id                UInt64,
    producer_schema_version UInt32,
    producer_git_commit     String,
    replay_semantics        LowCardinality(String),
    schedule_name           LowCardinality(String),
    schedule_config_hash    FixedString(66),
    block_number            UInt64,
    block_hash              FixedString(66),
    block_timestamp         DateTime,
    bucket                  LowCardinality(String),
    tx_count                UInt32,
    gas_delta_sum           Nullable(Int64),
    gas_delta_sum_sq        Nullable(Float64),
    gas_delta_min           Nullable(Int64),
    gas_delta_max           Nullable(Int64),
    gas_delta_log2_hist     Array(Int32),
    opcode                  Array(UInt8),
    opcode_count            Array(UInt64),
    opcode_gas_baseline     Array(UInt64),
    opcode_gas_schedule     Array(UInt64),
    state_gas_sum           Nullable(UInt64),
    state_gas_spillover_sum Nullable(UInt64),
    multiplier_log2_hist    Array(Int32),
    tx_count_creation       Nullable(UInt32),
    tx_count_authorization  Nullable(UInt32),
    tx_count_runtime_state  Nullable(UInt32),
    tx_count_no_state       Nullable(UInt32)
)
ENGINE = ReplacingMergeTree(updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_number, block_hash, bucket);

-- One row per retained drill-in (per-tx forensic record). `trace_payload` holds
-- the child components (call frames, opcode counts, event logs) as a versioned
-- JSON blob — NOT a full EVM step trace — plus an identity header. Its content
-- hash and component counts are stored alongside for verification.
CREATE TABLE IF NOT EXISTS reth_research_divergence
(
    updated_at                    DateTime,
    row_id                        FixedString(66),
    analysis_config_hash          FixedString(66),
    chain_id                      UInt64,
    producer_schema_version       UInt32,
    producer_git_commit           String,
    replay_semantics              LowCardinality(String),
    schedule_name                 LowCardinality(String),
    schedule_config_hash          FixedString(66),
    block_number                  UInt64,
    block_hash                    FixedString(66),
    block_timestamp               DateTime,
    tx_index                      UInt32,
    tx_hash                       FixedString(66),
    bucket                        LowCardinality(String),
    sender                        FixedString(42),
    recipient                     Nullable(FixedString(42)),
    is_create                     Bool,
    tx_gas_limit                  UInt64,
    baseline_success              Bool,
    schedule_success              Bool,
    status_changed                Bool,
    event_logs_changed            Bool,
    output_changed                Bool,
    logs_bloom_changed            Bool,
    baseline_gas_used             UInt64,
    schedule_gas_used             UInt64,
    gas_delta                     Int64,
    baseline_total_gas_spent      Nullable(UInt64),
    baseline_gas_refunded         Nullable(UInt64),
    schedule_total_gas_spent      Nullable(UInt64),
    schedule_gas_refunded         Nullable(UInt64),
    schedule_intrinsic_gas        Nullable(UInt64),
    schedule_floor_gas            Nullable(UInt64),
    would_fit_in_original_limit   Nullable(Bool),
    min_multiplier_to_succeed     Nullable(Float64),
    divergence_contract           Nullable(FixedString(42)),
    divergence_pc                 Nullable(UInt32),
    divergence_call_depth         Nullable(Int32),
    divergence_opcode             Nullable(UInt8),
    oog_contract                  Nullable(FixedString(42)),
    oog_pc                        Nullable(UInt32),
    oog_call_depth                Nullable(Int32),
    oog_opcode                    Nullable(UInt8),
    oog_pattern                   Nullable(String),
    oog_gas_remaining             Nullable(UInt64),
    oog_chain_proportional        Nullable(Bool),
    oog_bottleneck_depth          Nullable(Int32),
    oog_bottleneck_kind           Nullable(String),
    schedule_state_gas_spent      Nullable(UInt64),
    schedule_state_gas_demanded   Nullable(UInt64),
    schedule_initial_state_gas    Nullable(UInt64),
    schedule_initial_reservoir    Nullable(UInt64),
    runtime_state_gas             Nullable(UInt64),
    runtime_state_gas_spillover   Nullable(UInt64),
    state_gas_category            Nullable(String),
    reservoir_exhausted           Nullable(Bool),
    replay_halt_oog               Nullable(Bool),
    trace_payload                 String CODEC(ZSTD(3)),
    trace_content_hash            FixedString(66),
    trace_uncompressed_size_bytes UInt64,
    trace_format                  LowCardinality(String),
    trace_format_version          UInt16,
    call_frame_count              UInt32,
    opcode_count_row_count        UInt32,
    baseline_event_log_count      UInt32,
    schedule_event_log_count      UInt32,
    opcode_capture_complete       Nullable(Bool)
)
ENGINE = ReplacingMergeTree(updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_hash, tx_index, tx_hash);
