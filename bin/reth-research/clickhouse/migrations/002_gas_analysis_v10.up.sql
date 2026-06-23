-- EIP-8038 / F-series schema reshape (producer schema v10).
--
-- Brings the gas_analysis warehouse tables into line with the rebased
-- reth-research producer column contract (crates/research/src/export/clickhouse.rs
-- `required_columns` + bin/reth-research/clickhouse/PENDING_SCHEMA_UPDATE.md).
--
-- block_coverage and block_divergence are altered IN PLACE -- no table is
-- dropped, so any existing data and the replication state are preserved.
-- block_summary is the sole exception: its `bucket` -> `class` rename touches the
-- sorting key, which ClickHouse cannot ALTER in place (and `class` must stay in
-- the key or ReplacingMergeTree would collapse the per-class rows), so that one
-- table is recreated.
--
-- golang-migrate runs with x-multi-statement=true and is not string/comment
-- aware, so no COMMENT or comment line may contain a statement separator.

-- == gas_analysis_block_coverage (in-place) ==
ALTER TABLE gas_analysis.gas_analysis_block_coverage_local ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `tx_count_stored` UInt32 COMMENT 'Txs that produced a stored drill-in record' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `block_gas_used` UInt64 COMMENT 'Total gas used by the block' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `block_gas_limit` UInt64 COMMENT 'Block gas limit' CODEC(ZSTD(1)),
    DROP COLUMN IF EXISTS `tx_count_trace_only`,
    DROP COLUMN IF EXISTS `tx_count_event_logs_changed`,
    DROP COLUMN IF EXISTS `tx_count_schedule_rescued`,
    DROP COLUMN IF EXISTS `tx_count_wallet_fixable_shallow`,
    DROP COLUMN IF EXISTS `tx_count_wallet_fixable_deep_chain`,
    DROP COLUMN IF EXISTS `tx_count_inconclusive_needs_higher_sweep`,
    DROP COLUMN IF EXISTS `tx_count_contract_broken`,
    DROP COLUMN IF EXISTS `tx_count_aa_gas_reestimation`;
ALTER TABLE gas_analysis.gas_analysis_block_coverage ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `tx_count_stored` UInt32 COMMENT 'Txs that produced a stored drill-in record' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `block_gas_used` UInt64 COMMENT 'Total gas used by the block' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `block_gas_limit` UInt64 COMMENT 'Block gas limit' CODEC(ZSTD(1)),
    DROP COLUMN IF EXISTS `tx_count_trace_only`,
    DROP COLUMN IF EXISTS `tx_count_event_logs_changed`,
    DROP COLUMN IF EXISTS `tx_count_schedule_rescued`,
    DROP COLUMN IF EXISTS `tx_count_wallet_fixable_shallow`,
    DROP COLUMN IF EXISTS `tx_count_wallet_fixable_deep_chain`,
    DROP COLUMN IF EXISTS `tx_count_inconclusive_needs_higher_sweep`,
    DROP COLUMN IF EXISTS `tx_count_contract_broken`,
    DROP COLUMN IF EXISTS `tx_count_aa_gas_reestimation`;

-- == gas_analysis_divergence (in-place) ==
ALTER TABLE gas_analysis.gas_analysis_divergence_local ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `bucket`,
    DROP COLUMN IF EXISTS `would_fit_in_original_limit`,
    ADD COLUMN IF NOT EXISTS `outer_limit_only_failure` Nullable(UInt8) COMMENT 'F-series witness: failure attributable only to the outer gas limit' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `cold_account_access_count` Nullable(UInt64) COMMENT 'F2 cold account accesses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `additional_gas_charged` Nullable(Int64) COMMENT 'F4 additional gas charged by the schedule' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `failure_selector_path` Nullable(String) COMMENT 'F6 selector path to the failing frame' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_type` Nullable(UInt8) COMMENT 'F5 transaction type' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_nonce` Nullable(UInt64) COMMENT 'F5 transaction nonce' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `entry_selector` Nullable(String) COMMENT 'F5 entry function selector (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `input_zero_bytes` Nullable(UInt64) COMMENT 'F5 zero bytes in calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `input_nonzero_bytes` Nullable(UInt64) COMMENT 'F5 nonzero bytes in calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `has_authorization` Nullable(UInt8) COMMENT 'F5 whether the tx carries an authorization list' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `failure_reason` Nullable(String) COMMENT 'F1/F2 failure reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `revert_data` Nullable(String) COMMENT 'F1/F2 raw revert data (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `revert_decoded` Nullable(String) COMMENT 'F1/F2 decoded revert reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_output` Nullable(String) COMMENT 'F1/F2 transaction output (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_success` Nullable(UInt8) COMMENT 'F7 baseline root-frame success' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_gas_used` Nullable(UInt64) COMMENT 'F7 baseline root-frame gas used' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_gas_provided` Nullable(UInt64) COMMENT 'F7 baseline root-frame gas provided' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `surcharge_at_oog` Nullable(Int64) COMMENT 'F13 surcharge applied at the OOG point' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_contract` Nullable(String) COMMENT 'F10 contract at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_pc` Nullable(UInt32) COMMENT 'F10 program counter at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_call_depth` Nullable(Int32) COMMENT 'F10 call depth at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_opcode` Nullable(UInt8) COMMENT 'F10 opcode at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_second_db_read` Nullable(Int64) COMMENT 'F12 tax category second DB read' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_other` Nullable(Int64) COMMENT 'F12 tax category other' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_intrinsic` Nullable(Int64) COMMENT 'F12 tax category intrinsic' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sload_cold_count` Nullable(UInt64) COMMENT 'F8 cold SLOADs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sload_warm_count` Nullable(UInt64) COMMENT 'F8 warm SLOADs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_cold_count` Nullable(UInt64) COMMENT 'F8 cold SSTOREs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_set_count` Nullable(UInt64) COMMENT 'F8 SSTORE set transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_reset_count` Nullable(UInt64) COMMENT 'F8 SSTORE reset transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_clear_count` Nullable(UInt64) COMMENT 'F8 SSTORE clear transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_noop_count` Nullable(UInt64) COMMENT 'F8 SSTORE no-op transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_dirty_count` Nullable(UInt64) COMMENT 'F8 SSTORE dirty transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `warm_account_access_count` Nullable(UInt64) COMMENT 'F2/F3 warm account accesses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `value_transfer_count` Nullable(UInt64) COMMENT 'F3 value transfers' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `create_opcode_count` Nullable(UInt64) COMMENT 'F3 CREATE or CREATE2 opcodes' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `access_list_address_count` Nullable(UInt64) COMMENT 'F3 access-list addresses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `access_list_storage_key_count` Nullable(UInt64) COMMENT 'F3 access-list storage keys' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failure_reason` Nullable(String) COMMENT 'F1 tier-1 (1x) failure reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_opcode` Nullable(UInt8) COMMENT 'F1 tier-1 OOG opcode' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_contract` Nullable(String) COMMENT 'F1 tier-1 OOG contract' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_pc` Nullable(UInt32) COMMENT 'F1 tier-1 OOG program counter' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_depth` Nullable(Int32) COMMENT 'F1 tier-1 OOG call depth' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_gas_remaining` Nullable(UInt64) COMMENT 'F1 tier-1 gas remaining at OOG' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_selector` Nullable(String) COMMENT 'F1 tier-1 failing selector (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_gas_provided` Nullable(UInt64) COMMENT 'F1 tier-1 gas provided to the failing frame' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_gas_requested` Nullable(UInt64) COMMENT 'F1 tier-1 gas requested by the failing frame' CODEC(ZSTD(1));
ALTER TABLE gas_analysis.gas_analysis_divergence ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `bucket`,
    DROP COLUMN IF EXISTS `would_fit_in_original_limit`,
    ADD COLUMN IF NOT EXISTS `outer_limit_only_failure` Nullable(UInt8) COMMENT 'F-series witness: failure attributable only to the outer gas limit' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `cold_account_access_count` Nullable(UInt64) COMMENT 'F2 cold account accesses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `additional_gas_charged` Nullable(Int64) COMMENT 'F4 additional gas charged by the schedule' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `failure_selector_path` Nullable(String) COMMENT 'F6 selector path to the failing frame' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_type` Nullable(UInt8) COMMENT 'F5 transaction type' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_nonce` Nullable(UInt64) COMMENT 'F5 transaction nonce' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `entry_selector` Nullable(String) COMMENT 'F5 entry function selector (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `input_zero_bytes` Nullable(UInt64) COMMENT 'F5 zero bytes in calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `input_nonzero_bytes` Nullable(UInt64) COMMENT 'F5 nonzero bytes in calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `has_authorization` Nullable(UInt8) COMMENT 'F5 whether the tx carries an authorization list' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `failure_reason` Nullable(String) COMMENT 'F1/F2 failure reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `revert_data` Nullable(String) COMMENT 'F1/F2 raw revert data (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `revert_decoded` Nullable(String) COMMENT 'F1/F2 decoded revert reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_output` Nullable(String) COMMENT 'F1/F2 transaction output (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_success` Nullable(UInt8) COMMENT 'F7 baseline root-frame success' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_gas_used` Nullable(UInt64) COMMENT 'F7 baseline root-frame gas used' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_frame_gas_provided` Nullable(UInt64) COMMENT 'F7 baseline root-frame gas provided' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `surcharge_at_oog` Nullable(Int64) COMMENT 'F13 surcharge applied at the OOG point' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_contract` Nullable(String) COMMENT 'F10 contract at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_pc` Nullable(UInt32) COMMENT 'F10 program counter at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_call_depth` Nullable(Int32) COMMENT 'F10 call depth at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_div_opcode` Nullable(UInt8) COMMENT 'F10 opcode at the first gas divergence' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_second_db_read` Nullable(Int64) COMMENT 'F12 tax category second DB read' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_other` Nullable(Int64) COMMENT 'F12 tax category other' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tax_intrinsic` Nullable(Int64) COMMENT 'F12 tax category intrinsic' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sload_cold_count` Nullable(UInt64) COMMENT 'F8 cold SLOADs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sload_warm_count` Nullable(UInt64) COMMENT 'F8 warm SLOADs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_cold_count` Nullable(UInt64) COMMENT 'F8 cold SSTOREs' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_set_count` Nullable(UInt64) COMMENT 'F8 SSTORE set transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_reset_count` Nullable(UInt64) COMMENT 'F8 SSTORE reset transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_clear_count` Nullable(UInt64) COMMENT 'F8 SSTORE clear transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_noop_count` Nullable(UInt64) COMMENT 'F8 SSTORE no-op transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `sstore_dirty_count` Nullable(UInt64) COMMENT 'F8 SSTORE dirty transitions' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `warm_account_access_count` Nullable(UInt64) COMMENT 'F2/F3 warm account accesses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `value_transfer_count` Nullable(UInt64) COMMENT 'F3 value transfers' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `create_opcode_count` Nullable(UInt64) COMMENT 'F3 CREATE or CREATE2 opcodes' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `access_list_address_count` Nullable(UInt64) COMMENT 'F3 access-list addresses' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `access_list_storage_key_count` Nullable(UInt64) COMMENT 'F3 access-list storage keys' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failure_reason` Nullable(String) COMMENT 'F1 tier-1 (1x) failure reason' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_opcode` Nullable(UInt8) COMMENT 'F1 tier-1 OOG opcode' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_contract` Nullable(String) COMMENT 'F1 tier-1 OOG contract' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_pc` Nullable(UInt32) COMMENT 'F1 tier-1 OOG program counter' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_depth` Nullable(Int32) COMMENT 'F1 tier-1 OOG call depth' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_oog_gas_remaining` Nullable(UInt64) COMMENT 'F1 tier-1 gas remaining at OOG' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_selector` Nullable(String) COMMENT 'F1 tier-1 failing selector (0x-hex)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_gas_provided` Nullable(UInt64) COMMENT 'F1 tier-1 gas provided to the failing frame' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tier1_failing_gas_requested` Nullable(UInt64) COMMENT 'F1 tier-1 gas requested by the failing frame' CODEC(ZSTD(1));

-- == gas_analysis_block_summary (recreate -- sort-key rename forces it) ==
DROP TABLE IF EXISTS gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}' SYNC;
DROP TABLE IF EXISTS gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}' SYNC;

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
    `class` LowCardinality(String) COMMENT 'Fact class name (gas_only, unchanged, stored, ...)',
    `tx_count` UInt32 COMMENT 'Transactions in this class for this block' CODEC(ZSTD(1)),
    `gas_delta_sum` Nullable(Int64) COMMENT 'Sum of schedule-minus-baseline gas over the class' CODEC(ZSTD(1)),
    `gas_delta_sum_sq` Nullable(Float64) COMMENT 'Sum of squared gas deltas (approximate variance source)' CODEC(ZSTD(1)),
    `gas_delta_min` Nullable(Int64) COMMENT 'Minimum gas delta in the class' CODEC(ZSTD(1)),
    `gas_delta_max` Nullable(Int64) COMMENT 'Maximum gas delta in the class' CODEC(ZSTD(1)),
    `gas_delta_log2_hist` Array(Int32) COMMENT '12-bin log2 histogram of abs(gas_delta)' CODEC(ZSTD(1)),
    `opcode` Array(UInt8) COMMENT 'Sparse opcode list (parallel to opcode_* arrays)' CODEC(ZSTD(1)),
    `opcode_count` Array(UInt64) COMMENT 'Executions per opcode' CODEC(ZSTD(1)),
    `opcode_gas_baseline` Array(UInt64) COMMENT 'Baseline gas per opcode' CODEC(ZSTD(1)),
    `opcode_gas_schedule` Array(UInt64) COMMENT 'Schedule gas per opcode' CODEC(ZSTD(1)),
    `state_gas_sum` Nullable(UInt64) COMMENT 'Sum of state gas spent in the class' CODEC(ZSTD(1)),
    `state_gas_spillover_sum` Nullable(UInt64) COMMENT 'Sum of state-gas spillover in the class' CODEC(ZSTD(1)),
    `multiplier_log2_hist` Array(Int32) COMMENT '12-bin log2 histogram of min-multiplier-to-succeed' CODEC(ZSTD(1)),
    `tx_count_creation` Nullable(UInt32) COMMENT 'Txs whose state op was a creation' CODEC(ZSTD(1)),
    `tx_count_authorization` Nullable(UInt32) COMMENT 'Txs whose state op was an authorization' CODEC(ZSTD(1)),
    `tx_count_runtime_state` Nullable(UInt32) COMMENT 'Txs with runtime state gas' CODEC(ZSTD(1)),
    `tx_count_no_state` Nullable(UInt32) COMMENT 'Txs with no state gas' CODEC(ZSTD(1)),
    `cold_account_access_count` Nullable(UInt64) COMMENT 'F2 cold account accesses' CODEC(ZSTD(1)),
    `sload_cold_count` Nullable(UInt64) COMMENT 'F8 cold SLOADs' CODEC(ZSTD(1)),
    `sload_warm_count` Nullable(UInt64) COMMENT 'F8 warm SLOADs' CODEC(ZSTD(1)),
    `sstore_cold_count` Nullable(UInt64) COMMENT 'F8 cold SSTOREs' CODEC(ZSTD(1)),
    `sstore_set_count` Nullable(UInt64) COMMENT 'F8 SSTORE set transitions' CODEC(ZSTD(1)),
    `sstore_reset_count` Nullable(UInt64) COMMENT 'F8 SSTORE reset transitions' CODEC(ZSTD(1)),
    `sstore_clear_count` Nullable(UInt64) COMMENT 'F8 SSTORE clear transitions' CODEC(ZSTD(1)),
    `sstore_noop_count` Nullable(UInt64) COMMENT 'F8 SSTORE no-op transitions' CODEC(ZSTD(1)),
    `sstore_dirty_count` Nullable(UInt64) COMMENT 'F8 SSTORE dirty transitions' CODEC(ZSTD(1)),
    `warm_account_access_count` Nullable(UInt64) COMMENT 'F2/F3 warm account accesses' CODEC(ZSTD(1)),
    `value_transfer_count` Nullable(UInt64) COMMENT 'F3 value transfers' CODEC(ZSTD(1)),
    `create_opcode_count` Nullable(UInt64) COMMENT 'F3 CREATE or CREATE2 opcodes' CODEC(ZSTD(1)),
    `access_list_address_count` Nullable(UInt64) COMMENT 'F3 access-list addresses' CODEC(ZSTD(1)),
    `access_list_storage_key_count` Nullable(UInt64) COMMENT 'F3 access-list storage keys' CODEC(ZSTD(1))
)
ENGINE = ReplicatedReplacingMergeTree('/clickhouse/{installation}/{cluster}/tables/{shard}/{database}/{table}', '{replica}', updated_at)
PARTITION BY (chain_id, intDiv(block_number, 1000000))
ORDER BY (chain_id, analysis_config_hash, schedule_name, block_number, block_hash, class)
COMMENT 'Per-(config, schedule, block, class) aggregate summary for the non-drill-in cohorts.';

CREATE TABLE IF NOT EXISTS gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
AS gas_analysis.gas_analysis_block_summary_local
ENGINE = Distributed('{cluster}', 'gas_analysis', 'gas_analysis_block_summary_local', cityHash64(chain_id, analysis_config_hash, schedule_name, block_number, block_hash))
COMMENT 'Per-(config, schedule, block, class) aggregate summary for the non-drill-in cohorts.';