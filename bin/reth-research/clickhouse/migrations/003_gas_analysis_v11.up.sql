-- Producer schema v11: per-class transaction taxonomy + relative-gas columns
-- on gas_analysis_block_summary.
--
-- Adds the dashboard-requested aggregate columns for the aggregate-only
-- cohorts (unchanged / gas_only), which have no per-tx rows to derive them
-- from: six EIP-2718 type counts (summing to tx_count, `other` = any type
-- outside 0..=4 incl. future ones), the envelope-shape split (creation +
-- simple_transfer + contract_call == tx_count, where tx_count_creation
-- already exists), a 13-bin percentage histogram of the per-tx gas delta,
-- and the class's baseline-gas denominator. Also corrects two misleading
-- v10 column comments: tx_count_creation / tx_count_authorization count
-- TX-LEVEL facts (envelope create-kind / EIP-7702 authorization list), not
-- state ops as the old comments claimed.
--
-- All changes are in-place ALTERs -- no table is dropped, existing data and
-- replication state are preserved. Both the _local table and the Distributed
-- wrapper must be altered (the wrapper does not inherit column changes).
-- Rows written before this migration read NULL / [] in the new columns --
-- consumers must pin analysis_config_hash (v11 producers mint a new hash).
--
-- The load-bearing ADD COLUMNs and the cosmetic COMMENT COLUMN corrections
-- are deliberately SEPARATE statements: if a cluster rejects the bare
-- COMMENT COLUMN action, only the cosmetic statement fails and the producer
-- still finds its required columns. Fallback for the comment statements is
-- MODIFY COLUMN restating the column type with a COMMENT clause.
--
-- golang-migrate runs with x-multi-statement=true and is not string/comment
-- aware, so no COMMENT or comment line may contain a statement separator.

ALTER TABLE gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `tx_count_type_legacy` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 0 (legacy)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_access_list` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 1 (EIP-2930 access list)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_dynamic_fee` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 2 (EIP-1559 dynamic fee)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_blob` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 3 (EIP-4844 blob)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_set_code` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 4 (EIP-7702 set code)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_other` Nullable(UInt32) COMMENT 'Aggregated txs with any other EIP-2718 type so the six type counts sum to tx_count' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_simple_transfer` Nullable(UInt32) COMMENT 'Non-create txs with empty calldata (destination may still be a contract) - tx_count_creation plus tx_count_simple_transfer plus tx_count_contract_call equals tx_count' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_contract_call` Nullable(UInt32) COMMENT 'Non-create txs with non-empty calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_delta_pct_hist` Array(Int32) COMMENT '13-bin closed-left histogram of 100*gas_delta/baseline_gas_used with edges -100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf and bin sum equal to tx_count (empty = written pre-v11)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_gas_used_sum` Nullable(UInt64) COMMENT 'Sum of baseline gas used over the aggregated txs of this class (ratio-of-sums denominator for gas_delta_sum)' CODEC(ZSTD(1));
ALTER TABLE gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
    ADD COLUMN IF NOT EXISTS `tx_count_type_legacy` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 0 (legacy)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_access_list` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 1 (EIP-2930 access list)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_dynamic_fee` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 2 (EIP-1559 dynamic fee)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_blob` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 3 (EIP-4844 blob)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_set_code` Nullable(UInt32) COMMENT 'Aggregated txs with EIP-2718 type 4 (EIP-7702 set code)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_type_other` Nullable(UInt32) COMMENT 'Aggregated txs with any other EIP-2718 type so the six type counts sum to tx_count' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_simple_transfer` Nullable(UInt32) COMMENT 'Non-create txs with empty calldata (destination may still be a contract) - tx_count_creation plus tx_count_simple_transfer plus tx_count_contract_call equals tx_count' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `tx_count_contract_call` Nullable(UInt32) COMMENT 'Non-create txs with non-empty calldata' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `gas_delta_pct_hist` Array(Int32) COMMENT '13-bin closed-left histogram of 100*gas_delta/baseline_gas_used with edges -100,-50,-25,-10,-1,0,1,10,25,50,100,200,500,+inf and bin sum equal to tx_count (empty = written pre-v11)' CODEC(ZSTD(1)),
    ADD COLUMN IF NOT EXISTS `baseline_gas_used_sum` Nullable(UInt64) COMMENT 'Sum of baseline gas used over the aggregated txs of this class (ratio-of-sums denominator for gas_delta_sum)' CODEC(ZSTD(1));

-- Cosmetic comment corrections, isolated so a rejection cannot block the
-- column additions above.
ALTER TABLE gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}'
    COMMENT COLUMN IF EXISTS `tx_count_creation` 'Txs in this class that are tx-level contract creations (to is null)',
    COMMENT COLUMN IF EXISTS `tx_count_authorization` 'Txs in this class carrying an EIP-7702 authorization list';
ALTER TABLE gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
    COMMENT COLUMN IF EXISTS `tx_count_creation` 'Txs in this class that are tx-level contract creations (to is null)',
    COMMENT COLUMN IF EXISTS `tx_count_authorization` 'Txs in this class carrying an EIP-7702 authorization list';
