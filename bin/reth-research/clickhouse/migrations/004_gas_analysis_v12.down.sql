-- Reverse the producer-schema-v12 additions: drop the per-tx gas spine table
-- (Distributed wrapper first, then the _local table it points at) and drop the
-- block_base_fee_per_gas column from both coverage tables, restoring the 003
-- state.
--
-- Dropping gas_analysis_tx_gas_result is destructive: it discards the per-tx
-- gas rows for every exported block, which can only be recovered by
-- re-draining the producers' SQLite outboxes or re-running the replay.
-- Keep every COMMENT and comment line free of the statement separator
-- character.

DROP TABLE IF EXISTS gas_analysis.gas_analysis_tx_gas_result ON CLUSTER '{cluster}' SYNC;
DROP TABLE IF EXISTS gas_analysis.gas_analysis_tx_gas_result_local ON CLUSTER '{cluster}' SYNC;

ALTER TABLE gas_analysis.gas_analysis_block_coverage_local ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `block_base_fee_per_gas`;
ALTER TABLE gas_analysis.gas_analysis_block_coverage ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `block_base_fee_per_gas`;
