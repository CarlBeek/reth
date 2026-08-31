-- Producer schema v12: unconditional per-tx gas spine.
--
-- Adds gas_analysis_tx_gas_result, one row per (schedule, block, tx) written
-- for EVERY transaction rather than only the store_full_forensics minority
-- that earns a gas_analysis_divergence row. The divergence table explains the
-- divergent tail; a repricing simulator instead needs the repriced gas of
-- every tx, including the byte-identical majority that only ever reaches a
-- gas_analysis_block_summary class aggregate.
--
-- This table is deliberately slim: no trace_payload, no call frames, no
-- opcode counts. It is also uncapped, unlike drill-ins which the producer's
-- max-divergences-per-block setting truncates. Expect roughly tx_count rows
-- per (schedule, block) -- a materially larger row count than the divergence
-- table, hence the same partitioning and a ZSTD(1) column codec throughout.
--
-- Gas column semantics that are NOT interchangeable:
--   schedule_gas_used        -- sender-facing: post-refund, floor-applied.
--   schedule_total_gas_spent -- PRE-refund. EIP-7778 makes block-level gas
--                               accounting ignore refunds, so this is the
--                               figure that feeds block-fill analysis.
--   schedule_gas_refunded    -- the difference, kept explicit.
--
-- Also adds block_base_fee_per_gas to gas_analysis_block_coverage. With the
-- per-tx fee caps in the new table this completes the fee-market picture, so
-- the simulator no longer needs an external per-tx fee source.
--
-- Migration 003 is applied and frozen -- this is the first v12 delta.
-- Both the _local table and the Distributed wrapper are created / altered
-- (the wrapper does not inherit column changes).
-- Rows written before this migration read NULL in block_base_fee_per_gas --
-- consumers must pin analysis_config_hash (v12 producers mint a new hash).
--
-- golang-migrate runs with x-multi-statement=true and is not string/comment
-- aware, so no COMMENT or comment line may contain a statement separator.

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_tx_gas_result_local ON CLUSTER '{cluster}'
(
    `updated_at` DateTime COMMENT 'Timestamp when the record was last produced (ReplacingMergeTree version)' CODEC(DoubleDelta, ZSTD(1)),
    `row_id` FixedString(66) COMMENT 'Deterministic per-tx gas row identity' CODEC(ZSTD(1)),
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
    `tx_type` UInt8 COMMENT 'EIP-2718 transaction envelope type byte' CODEC(ZSTD(1)),
    `tx_gas_limit` UInt64 COMMENT 'Transaction gas limit as declared by the sender' CODEC(ZSTD(1)),
    `max_fee_per_gas` String COMMENT 'Sender max fee per gas as a U256 decimal string (never narrowed to an integer column)' CODEC(ZSTD(1)),
    `max_priority_fee_per_gas` Nullable(String) COMMENT 'Sender max priority fee per gas as a U256 decimal string - null for envelope types with no priority cap' CODEC(ZSTD(1)),
    `baseline_success` Bool COMMENT 'Baseline (native-schedule) execution succeeded',
    `baseline_gas_used` UInt64 COMMENT 'Gas used under baseline - the repricing denominator' CODEC(ZSTD(1)),
    `baseline_total_gas_spent` UInt64 COMMENT 'Baseline pre-refund gas spent' CODEC(ZSTD(1)),
    `schedule_success` Bool COMMENT 'Schedule execution succeeded within the original tx gas limit',
    `schedule_gas_used` UInt64 COMMENT 'Sender-facing gas under the schedule - post-refund and floor-applied' CODEC(ZSTD(1)),
    `schedule_total_gas_spent` UInt64 COMMENT 'PRE-refund gas under the schedule - the EIP-7778 block-accounting figure, not interchangeable with schedule_gas_used' CODEC(ZSTD(1)),
    `schedule_gas_refunded` UInt64 COMMENT 'Gas refunded under the schedule - the difference between the pre-refund and sender-facing figures' CODEC(ZSTD(1)),
    `schedule_floor_gas` UInt64 COMMENT 'Schedule calldata floor gas (EIP-7623 lineage)' CODEC(ZSTD(1)),
    `schedule_state_gas_spent` UInt64 COMMENT 'EIP-8037 state gas spent under the schedule - zero for schedules that do not engage state gas' CODEC(ZSTD(1)),
    `schedule_intrinsic_gas` Nullable(UInt64) COMMENT 'Schedule intrinsic gas - null for execution-only schedules which have no intrinsic opinion' CODEC(ZSTD(1)),
    `min_multiplier_to_succeed` Nullable(Float64) COMMENT 'Continuous min gas-limit multiplier the schedule needed - null when the replay halted regardless of gas' CODEC(ZSTD(1))
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_hash, tx_index, tx_hash)
COMMENT 'Unconditional per-tx repriced-gas spine - one row per transaction, not just divergent ones. Slim and uncapped.';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_tx_gas_result ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_tx_gas_result_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_tx_gas_result_local', cityHash64(chain_id, analysis_config_hash, schedule_name, block_hash, tx_index, tx_hash))
COMMENT 'Unconditional per-tx repriced-gas spine - one row per transaction, not just divergent ones. Slim and uncapped.';

ALTER TABLE gas_analysis.gas_analysis_block_coverage_local ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `block_base_fee_per_gas` Nullable(UInt64) COMMENT 'EIP-1559 base fee from the block header - null for pre-London blocks or rows written before v12' CODEC(ZSTD(1));
ALTER TABLE gas_analysis.gas_analysis_block_coverage ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `block_base_fee_per_gas` Nullable(UInt64) COMMENT 'EIP-1559 base fee from the block header - null for pre-London blocks or rows written before v12' CODEC(ZSTD(1));
