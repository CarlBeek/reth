-- Reverse the producer-schema-v11 summary additions: drop the taxonomy /
-- relative-gas columns and restore the (misleading) v10 comment texts on
-- tx_count_creation / tx_count_authorization so the down state matches 002.
-- The column drops and the cosmetic comment restores are separate statements,
-- mirroring the up migration, so a COMMENT COLUMN rejection cannot block the
-- drops. Keep every COMMENT and comment line free of the statement separator
-- character.

ALTER TABLE gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `tx_count_type_legacy`,
    DROP COLUMN IF EXISTS `tx_count_type_access_list`,
    DROP COLUMN IF EXISTS `tx_count_type_dynamic_fee`,
    DROP COLUMN IF EXISTS `tx_count_type_blob`,
    DROP COLUMN IF EXISTS `tx_count_type_set_code`,
    DROP COLUMN IF EXISTS `tx_count_type_other`,
    DROP COLUMN IF EXISTS `tx_count_simple_transfer`,
    DROP COLUMN IF EXISTS `tx_count_contract_call`,
    DROP COLUMN IF EXISTS `gas_delta_pct_hist`,
    DROP COLUMN IF EXISTS `baseline_gas_used_sum`;
ALTER TABLE gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
    DROP COLUMN IF EXISTS `tx_count_type_legacy`,
    DROP COLUMN IF EXISTS `tx_count_type_access_list`,
    DROP COLUMN IF EXISTS `tx_count_type_dynamic_fee`,
    DROP COLUMN IF EXISTS `tx_count_type_blob`,
    DROP COLUMN IF EXISTS `tx_count_type_set_code`,
    DROP COLUMN IF EXISTS `tx_count_type_other`,
    DROP COLUMN IF EXISTS `tx_count_simple_transfer`,
    DROP COLUMN IF EXISTS `tx_count_contract_call`,
    DROP COLUMN IF EXISTS `gas_delta_pct_hist`,
    DROP COLUMN IF EXISTS `baseline_gas_used_sum`;

ALTER TABLE gas_analysis.gas_analysis_block_summary_local ON CLUSTER '{cluster}'
    COMMENT COLUMN IF EXISTS `tx_count_creation` 'Txs whose state op was a creation',
    COMMENT COLUMN IF EXISTS `tx_count_authorization` 'Txs whose state op was an authorization';
ALTER TABLE gas_analysis.gas_analysis_block_summary ON CLUSTER '{cluster}'
    COMMENT COLUMN IF EXISTS `tx_count_creation` 'Txs whose state op was a creation',
    COMMENT COLUMN IF EXISTS `tx_count_authorization` 'Txs whose state op was an authorization';
