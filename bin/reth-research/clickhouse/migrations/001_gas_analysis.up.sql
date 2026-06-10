-- Gas-repricing analysis export schema (reth-research embedded ClickHouse export).
--
-- This is the producer/consumer COLUMN CONTRACT. The reth-research binary inserts
-- JSONEachRow rows whose field names + types match the columns below, and verifies
-- a subset with `DESCRIBE TABLE` at startup (see
-- crates/research/src/export/clickhouse.rs `required_columns`). Names/types must
-- not drift from crates/research/src/export/model.rs.
--
-- Layout targets a replicated ClickHouse cluster: a dedicated database, one
-- `ReplicatedReplacingMergeTree` `_local` table per logical table plus a
-- `Distributed` wrapper, everything `ON CLUSTER '{cluster}'`. The standard
-- `{cluster}` / `{shard}` / `{replica}` / `{installation}` macros must be defined
-- in your ClickHouse config. The exporter inserts into the distributed
-- (unsuffixed) table names. `updated_at` is the `ReplacingMergeTree` version
-- column, so re-sending a block is idempotent.

CREATE DATABASE IF NOT EXISTS gas_analysis ON CLUSTER '{cluster}';

-- ── gas_analysis_run: one row per deterministic analysis configuration ──
CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_run_local ON CLUSTER '{cluster}'
(
    `updated_at` DateTime COMMENT 'Timestamp when the record was last produced (ReplacingMergeTree version)' CODEC(DoubleDelta, ZSTD(1)),
    `analysis_config_hash` FixedString(66) COMMENT 'Dataset identity: keccak of the analysis manifest' CODEC(ZSTD(1)),
    `chain_id` UInt64 COMMENT 'Chain ID the analysis ran against' CODEC(ZSTD(1)),
    `producer_schema_version` UInt32 COMMENT 'Producer SQLite schema version' CODEC(ZSTD(1)),
    `producer_git_commit` String COMMENT 'Full git SHA of the producer binary' CODEC(ZSTD(1)),
    `replay_semantics` LowCardinality(String) COMMENT 'Replay semantics tag (e.g. canonical_pre_tx_state)',
    `manifest_json` String COMMENT 'Canonical JSON of the analysis manifest (hash preimage)' CODEC(ZSTD(3))
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
ORDER BY (chain_id, analysis_config_hash)
COMMENT 'One row per deterministic gas-analysis configuration (the dataset manifest).';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_run ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_run_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_run_local', cityHash64(chain_id, analysis_config_hash))
COMMENT 'One row per deterministic gas-analysis configuration (the dataset manifest).';

-- ── gas_analysis_block_coverage: one row per (config, schedule, block hash) ──
CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_block_coverage_local ON CLUSTER '{cluster}'
(
    `updated_at` DateTime COMMENT 'Timestamp when the record was last produced (ReplacingMergeTree version)' CODEC(DoubleDelta, ZSTD(1)),
    `row_id` FixedString(66) COMMENT 'Deterministic coverage row identity' CODEC(ZSTD(1)),
    `analysis_config_hash` FixedString(66) COMMENT 'Dataset identity (keccak of the analysis manifest)' CODEC(ZSTD(1)),
    `chain_id` UInt64 COMMENT 'Chain ID the analysis ran against' CODEC(ZSTD(1)),
    `producer_schema_version` UInt32 COMMENT 'Producer SQLite schema version' CODEC(ZSTD(1)),
    `producer_git_commit` String COMMENT 'Full git SHA of the producer binary' CODEC(ZSTD(1)),
    `replay_semantics` LowCardinality(String) COMMENT 'Replay semantics tag',
    `schedule_name` LowCardinality(String) COMMENT 'Gas schedule name (e.g. eip-2780)',
    `schedule_config_hash` FixedString(66) COMMENT 'Keccak of the schedule config fingerprint' CODEC(ZSTD(1)),
    `block_number` UInt64 COMMENT 'Block number' CODEC(DoubleDelta, ZSTD(1)),
    `block_hash` FixedString(66) COMMENT 'Canonical block hash' CODEC(ZSTD(1)),
    `parent_hash` FixedString(66) COMMENT 'Parent block hash' CODEC(ZSTD(1)),
    `block_timestamp` DateTime COMMENT 'Block timestamp' CODEC(DoubleDelta, ZSTD(1)),
    `tx_count` UInt32 COMMENT 'Total transactions in the block' CODEC(ZSTD(1)),
    `tx_count_unchanged` UInt32 COMMENT 'Txs with identical outcome and traces' CODEC(ZSTD(1)),
    `tx_count_trace_only` UInt32 COMMENT 'Txs whose trace differs but outcome and gas match' CODEC(ZSTD(1)),
    `tx_count_gas_only` UInt32 COMMENT 'Txs with a gas delta and no other difference' CODEC(ZSTD(1)),
    `tx_count_event_logs_changed` UInt32 COMMENT 'Drill-in: emitted event logs differ, same outcome' CODEC(ZSTD(1)),
    `tx_count_schedule_rescued` UInt32 COMMENT 'Outcome flipped baseline-failed to schedule-succeeded' CODEC(ZSTD(1)),
    `tx_count_wallet_fixable_shallow` UInt32 COMMENT 'Break attributed to the root frame (raise gas limit)' CODEC(ZSTD(1)),
    `tx_count_wallet_fixable_deep_chain` UInt32 COMMENT 'Break with proportional subcall gas (wallet-fixable)' CODEC(ZSTD(1)),
    `tx_count_inconclusive_needs_higher_sweep` UInt32 COMMENT 'Drill-in: still OOG at the highest tier, no bottleneck proven' CODEC(ZSTD(1)),
    `tx_count_contract_broken` UInt32 COMMENT 'Drill-in: break not resolvable by raising gas' CODEC(ZSTD(1)),
    `tx_count_aa_gas_reestimation` UInt32 COMMENT 'Drill-in: ERC-4337 EntryPoint OOG, off-chain re-estimation fix' CODEC(ZSTD(1)),
    `expected_drill_in_count` UInt32 COMMENT 'Drill-in count implied by the bucket counts' CODEC(ZSTD(1)),
    `retained_drill_in_count` UInt32 COMMENT 'Drill-in records actually retained/exported' CODEC(ZSTD(1)),
    `drill_ins_truncated` Bool COMMENT 'Whether retention dropped some drill-ins'
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_number, block_hash)
COMMENT 'Per-(config, schedule, block) bucket counts. Coverage is the remote completion marker (inserted last).';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_block_coverage ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_block_coverage_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_block_coverage_local', cityHash64(chain_id, analysis_config_hash, schedule_name, block_number, block_hash))
COMMENT 'Per-(config, schedule, block) bucket counts. Coverage is the remote completion marker (inserted last).';

-- ── gas_analysis_block_summary: one row per non-empty (config, schedule, block, bucket) ──
CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}'
(
    `updated_at` DateTime COMMENT 'Timestamp when the record was last produced (ReplacingMergeTree version)' CODEC(DoubleDelta, ZSTD(1)),
    `row_id` FixedString(66) COMMENT 'Deterministic summary row identity' CODEC(ZSTD(1)),
    `analysis_config_hash` FixedString(66) COMMENT 'Dataset identity (keccak of the analysis manifest)' CODEC(ZSTD(1)),
    `chain_id` UInt64 COMMENT 'Chain ID the analysis ran against' CODEC(ZSTD(1)),
    `producer_schema_version` UInt32 COMMENT 'Producer SQLite schema version' CODEC(ZSTD(1)),
    `producer_git_commit` String COMMENT 'Full git SHA of the producer binary' CODEC(ZSTD(1)),
    `replay_semantics` LowCardinality(String) COMMENT 'Replay semantics tag',
    `schedule_name` LowCardinality(String) COMMENT 'Gas schedule name',
    `schedule_config_hash` FixedString(66) COMMENT 'Keccak of the schedule config fingerprint' CODEC(ZSTD(1)),
    `block_number` UInt64 COMMENT 'Block number' CODEC(DoubleDelta, ZSTD(1)),
    `block_hash` FixedString(66) COMMENT 'Canonical block hash' CODEC(ZSTD(1)),
    `block_timestamp` DateTime COMMENT 'Block timestamp' CODEC(DoubleDelta, ZSTD(1)),
    `bucket` LowCardinality(String) COMMENT 'Aggregate bucket name (gas_only, unchanged, ...)',
    `tx_count` UInt32 COMMENT 'Transactions in this bucket for this block' CODEC(ZSTD(1)),
    `gas_delta_sum` Nullable(Int64) COMMENT 'Sum of schedule-minus-baseline gas over the bucket' CODEC(ZSTD(1)),
    `gas_delta_sum_sq` Nullable(Float64) COMMENT 'Sum of squared gas deltas (approximate variance source)' CODEC(ZSTD(1)),
    `gas_delta_min` Nullable(Int64) COMMENT 'Minimum gas delta in the bucket' CODEC(ZSTD(1)),
    `gas_delta_max` Nullable(Int64) COMMENT 'Maximum gas delta in the bucket' CODEC(ZSTD(1)),
    `gas_delta_log2_hist` Array(Int32) COMMENT '12-bin log2 histogram of abs(gas_delta)' CODEC(ZSTD(1)),
    `opcode` Array(UInt8) COMMENT 'Sparse opcode list (parallel to opcode_* arrays)' CODEC(ZSTD(1)),
    `opcode_count` Array(UInt64) COMMENT 'Executions per opcode' CODEC(ZSTD(1)),
    `opcode_gas_baseline` Array(UInt64) COMMENT 'Baseline gas per opcode' CODEC(ZSTD(1)),
    `opcode_gas_schedule` Array(UInt64) COMMENT 'Schedule gas per opcode' CODEC(ZSTD(1)),
    `state_gas_sum` Nullable(UInt64) COMMENT 'Sum of state gas spent in the bucket' CODEC(ZSTD(1)),
    `state_gas_spillover_sum` Nullable(UInt64) COMMENT 'Sum of state-gas spillover in the bucket' CODEC(ZSTD(1)),
    `multiplier_log2_hist` Array(Int32) COMMENT '12-bin log2 histogram of min-multiplier-to-succeed' CODEC(ZSTD(1)),
    `tx_count_creation` Nullable(UInt32) COMMENT 'Txs whose state op was a creation' CODEC(ZSTD(1)),
    `tx_count_authorization` Nullable(UInt32) COMMENT 'Txs whose state op was an authorization' CODEC(ZSTD(1)),
    `tx_count_runtime_state` Nullable(UInt32) COMMENT 'Txs with runtime state gas' CODEC(ZSTD(1)),
    `tx_count_no_state` Nullable(UInt32) COMMENT 'Txs with no state gas' CODEC(ZSTD(1))
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_number, block_hash, bucket)
COMMENT 'Per-(config, schedule, block, bucket) aggregate summary for the non-drill-in cohorts.';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_block_summary_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_block_summary_local', cityHash64(chain_id, analysis_config_hash, schedule_name, block_number, block_hash))
COMMENT 'Per-(config, schedule, block, bucket) aggregate summary for the non-drill-in cohorts.';

-- ── gas_analysis_divergence: one row per retained drill-in (per-tx forensic record) ──
CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_divergence_local ON CLUSTER '{cluster}'
(
    `updated_at` DateTime COMMENT 'Timestamp when the record was last produced (ReplacingMergeTree version)' CODEC(DoubleDelta, ZSTD(1)),
    `row_id` FixedString(66) COMMENT 'Deterministic divergence row identity' CODEC(ZSTD(1)),
    `analysis_config_hash` FixedString(66) COMMENT 'Dataset identity (keccak of the analysis manifest)' CODEC(ZSTD(1)),
    `chain_id` UInt64 COMMENT 'Chain ID the analysis ran against' CODEC(ZSTD(1)),
    `producer_schema_version` UInt32 COMMENT 'Producer SQLite schema version' CODEC(ZSTD(1)),
    `producer_git_commit` String COMMENT 'Full git SHA of the producer binary' CODEC(ZSTD(1)),
    `replay_semantics` LowCardinality(String) COMMENT 'Replay semantics tag',
    `schedule_name` LowCardinality(String) COMMENT 'Gas schedule name',
    `schedule_config_hash` FixedString(66) COMMENT 'Keccak of the schedule config fingerprint' CODEC(ZSTD(1)),
    `block_number` UInt64 COMMENT 'Block number' CODEC(DoubleDelta, ZSTD(1)),
    `block_hash` FixedString(66) COMMENT 'Canonical block hash' CODEC(ZSTD(1)),
    `block_timestamp` DateTime COMMENT 'Block timestamp' CODEC(DoubleDelta, ZSTD(1)),
    `tx_index` UInt32 COMMENT 'Transaction index within the block' CODEC(ZSTD(1)),
    `tx_hash` FixedString(66) COMMENT 'Transaction hash' CODEC(ZSTD(1)),
    `bucket` LowCardinality(String) COMMENT 'Drill-in bucket name',
    `sender` FixedString(42) COMMENT 'Transaction sender address' CODEC(ZSTD(1)),
    `recipient` Nullable(FixedString(42)) COMMENT 'Transaction recipient (null on contract creation)' CODEC(ZSTD(1)),
    `is_create` Bool COMMENT 'Whether the tx is a contract creation',
    `tx_gas_limit` UInt64 COMMENT 'Transaction gas limit' CODEC(ZSTD(1)),
    `baseline_success` Bool COMMENT 'Baseline execution succeeded',
    `schedule_success` Bool COMMENT 'Schedule execution succeeded',
    `status_changed` Bool COMMENT 'Success/failure flipped between baseline and schedule',
    `event_logs_changed` Bool COMMENT 'Emitted event logs differ',
    `output_changed` Bool COMMENT 'Return data differs',
    `logs_bloom_changed` Bool COMMENT 'Logs bloom differs',
    `baseline_gas_used` UInt64 COMMENT 'Gas used under baseline' CODEC(ZSTD(1)),
    `schedule_gas_used` UInt64 COMMENT 'Gas used under the schedule' CODEC(ZSTD(1)),
    `gas_delta` Int64 COMMENT 'schedule_gas_used - baseline_gas_used' CODEC(ZSTD(1)),
    `baseline_total_gas_spent` Nullable(UInt64) COMMENT 'Baseline total gas spent incl. refunds' CODEC(ZSTD(1)),
    `baseline_gas_refunded` Nullable(UInt64) COMMENT 'Baseline gas refunded' CODEC(ZSTD(1)),
    `schedule_total_gas_spent` Nullable(UInt64) COMMENT 'Schedule total gas spent incl. refunds' CODEC(ZSTD(1)),
    `schedule_gas_refunded` Nullable(UInt64) COMMENT 'Schedule gas refunded' CODEC(ZSTD(1)),
    `schedule_intrinsic_gas` Nullable(UInt64) COMMENT 'Schedule intrinsic gas' CODEC(ZSTD(1)),
    `schedule_floor_gas` Nullable(UInt64) COMMENT 'Schedule floor (calldata) gas' CODEC(ZSTD(1)),
    `would_fit_in_original_limit` Nullable(Bool) COMMENT 'Whether schedule gas fits the original limit',
    `min_multiplier_to_succeed` Nullable(Float64) COMMENT 'Continuous min gas-limit multiplier to succeed' CODEC(ZSTD(1)),
    `divergence_contract` Nullable(FixedString(42)) COMMENT 'Contract at the first divergence point' CODEC(ZSTD(1)),
    `divergence_pc` Nullable(UInt32) COMMENT 'Program counter at the first divergence' CODEC(ZSTD(1)),
    `divergence_call_depth` Nullable(Int32) COMMENT 'Call depth at the first divergence' CODEC(ZSTD(1)),
    `divergence_opcode` Nullable(UInt8) COMMENT 'Opcode at the first divergence' CODEC(ZSTD(1)),
    `oog_contract` Nullable(FixedString(42)) COMMENT 'Contract where the OOG halt occurred' CODEC(ZSTD(1)),
    `oog_pc` Nullable(UInt32) COMMENT 'Program counter at the OOG halt' CODEC(ZSTD(1)),
    `oog_call_depth` Nullable(Int32) COMMENT 'Call depth at the OOG halt' CODEC(ZSTD(1)),
    `oog_opcode` Nullable(UInt8) COMMENT 'Opcode at the OOG halt' CODEC(ZSTD(1)),
    `oog_pattern` Nullable(String) COMMENT 'OOG pattern classification' CODEC(ZSTD(1)),
    `oog_gas_remaining` Nullable(UInt64) COMMENT 'Gas remaining at the OOG halt' CODEC(ZSTD(1)),
    `oog_chain_proportional` Nullable(Bool) COMMENT 'Whether the OOG chain got proportional gas',
    `oog_bottleneck_depth` Nullable(Int32) COMMENT 'Depth of the proven OOG bottleneck' CODEC(ZSTD(1)),
    `oog_bottleneck_kind` Nullable(String) COMMENT 'Kind of the proven OOG bottleneck' CODEC(ZSTD(1)),
    `schedule_state_gas_spent` Nullable(UInt64) COMMENT 'State gas actually spent under the schedule' CODEC(ZSTD(1)),
    `schedule_state_gas_demanded` Nullable(UInt64) COMMENT 'State gas attempted (incl. a charge that OOGd)' CODEC(ZSTD(1)),
    `schedule_initial_state_gas` Nullable(UInt64) COMMENT 'Initial state gas budget' CODEC(ZSTD(1)),
    `schedule_initial_reservoir` Nullable(UInt64) COMMENT 'Initial reservoir budget' CODEC(ZSTD(1)),
    `runtime_state_gas` Nullable(UInt64) COMMENT 'Runtime state gas' CODEC(ZSTD(1)),
    `runtime_state_gas_spillover` Nullable(UInt64) COMMENT 'Runtime state-gas spillover' CODEC(ZSTD(1)),
    `state_gas_category` Nullable(String) COMMENT 'State gas category' CODEC(ZSTD(1)),
    `reservoir_exhausted` Nullable(Bool) COMMENT 'Whether the reservoir was exhausted',
    `replay_halt_oog` Nullable(Bool) COMMENT 'Final tier halted OOG (Some(true)) vs non-gas halt (Some(false))',
    `trace_payload` String COMMENT 'Forensic child components (call frames, opcode counts, event logs) as versioned JSON, NOT a full EVM trace' CODEC(ZSTD(3)),
    `trace_content_hash` FixedString(66) COMMENT 'Keccak of trace_payload' CODEC(ZSTD(1)),
    `trace_uncompressed_size_bytes` UInt64 COMMENT 'Byte length of trace_payload' CODEC(ZSTD(1)),
    `trace_format` LowCardinality(String) COMMENT 'trace_payload format tag',
    `trace_format_version` UInt16 COMMENT 'trace_payload format version' CODEC(ZSTD(1)),
    `call_frame_count` UInt32 COMMENT 'Call frames in trace_payload' CODEC(ZSTD(1)),
    `opcode_count_row_count` UInt32 COMMENT 'Opcode-count rows in trace_payload' CODEC(ZSTD(1)),
    `baseline_event_log_count` UInt32 COMMENT 'Baseline event logs in trace_payload' CODEC(ZSTD(1)),
    `schedule_event_log_count` UInt32 COMMENT 'Schedule event logs in trace_payload' CODEC(ZSTD(1)),
    `opcode_capture_complete` Nullable(Bool) COMMENT 'Whether per-opcode capture was complete (null in v1)'
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_hash, tx_index, tx_hash)
COMMENT 'Per-tx drill-in forensic record. trace_payload carries the child components, not a full step trace.';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_divergence ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_divergence_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_divergence_local', cityHash64(chain_id, analysis_config_hash, schedule_name, block_hash, tx_index, tx_hash))
COMMENT 'Per-tx drill-in forensic record. trace_payload carries the child components, not a full step trace.';
