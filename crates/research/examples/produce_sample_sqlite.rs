//! Emit a small SQLite file in the producer's format, for cross-repo
//! wire validation against the consumer's `sqlite_scanner` attach path.
//!
//! Used while developing the storage redesign: the consumer (Python /
//! DuckDB in `repricing-forensics`) attaches the file this writes and
//! runs its full dashboard endpoint suite against it. Throwaway once the
//! producer is wired into the live reth path.
//!
//! Usage:
//!   cargo run -p reth-research --example produce_sample_sqlite -- /tmp/sample.sqlite

use alloy_primitives::{Address, Bytes, B256};
use reth_research::{
    database::{
        BlockCoverageRow, BlockOutput, BlockSummaryRow, CallFrameRow, DivergenceDatabase,
        DivergenceRow, DrillInRecord, OpcodeBucketTotal, OpcodeCountRow, RecipientRow,
        TxGasResultRow,
    },
    divergence::{AggregateClass, EventLog},
};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/sample.sqlite".into());
    // Wipe any prior file + WAL sidecars.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));

    let db = DivergenceDatabase::open(&path).expect("open");
    db.record_analysis_run_start("amsterdam", "config-v1", Some("sample"), Some("cross-repo test"))
        .unwrap();

    // One contract-broken tx with a per-frame opcode count + an event log,
    // matching the shape the consumer's dashboard expects.
    let drill_in = DrillInRecord {
        divergence: DivergenceRow {
            schedule_name: "amsterdam".into(),
            schedule_config_hash: "config-v1".into(),
            block_number: 22_000_000,
            tx_index: 0,
            tx_hash: B256::repeat_byte(0xdd),
            timestamp: 1_700_000_000,
            // OOG break (stipend bottleneck), not an outer-limit-only failure.
            outer_limit_only_failure: Some(false),
            sender: Address::repeat_byte(0x11),
            recipient: Some(Address::repeat_byte(0x22)),
            is_create: false,
            tx_gas_limit: 500_000,
            baseline_success: true,
            schedule_success: false,
            status_changed: true,
            event_logs_changed: false,
            output_changed: false,
            logs_bloom_changed: false,
            baseline_gas_used: 80_000,
            schedule_gas_used: 100_000,
            gas_delta: 20_000,
            baseline_total_gas_spent: Some(80_000),
            baseline_gas_refunded: Some(0),
            schedule_total_gas_spent: Some(100_000),
            schedule_gas_refunded: Some(0),
            schedule_intrinsic_gas: Some(21_000),
            schedule_floor_gas: Some(21_000),
            min_multiplier_to_succeed: Some(1.25),
            divergence_contract: Some(Address::repeat_byte(0x22)),
            divergence_pc: Some(123),
            divergence_call_depth: Some(1),
            divergence_opcode: Some(0x55),
            oog_contract: Some(Address::repeat_byte(0x22)),
            oog_pc: Some(456),
            oog_call_depth: Some(1),
            oog_opcode: Some(0x55),
            oog_pattern: Some("stipend".into()),
            oog_gas_remaining: Some(0),
            oog_chain_proportional: Some(false),
            oog_bottleneck_depth: Some(1),
            oog_bottleneck_kind: Some("Stipend2300".into()),
            additional_gas_charged: Some(20_000),
            failure_selector_path: Some("[\"0x12345678\"]".into()),
            ..Default::default()
        },
        call_frames: vec![CallFrameRow {
            call_index: 0,
            parent_call_index: None,
            depth: 0,
            from_address: Address::repeat_byte(0x11),
            to_address: Address::repeat_byte(0x22),
            code_address: Some(Address::repeat_byte(0x22)),
            codehash: Some(B256::repeat_byte(0xcc)),
            call_type: "CALL".into(),
            selector: Some([0x12, 0x34, 0x56, 0x78]),
            value_wei: Some("0".into()),
            gas_provided: 100_000,
            gas_used: 100_000,
            gas_margin: Some(0),
            success: false,
            parent_gas_at_call: None,
            gas_requested_on_stack: None,
            eip150_cap_binding: None,
            deployed_bytecode_len: None,
            repricing_gas_delta: 0,
            ..Default::default()
        }],
        opcode_counts: vec![OpcodeCountRow {
            call_index: 0,
            opcode: 0x55,
            count: 3,
            gas_baseline: 60_000,
            gas_schedule: 75_000,
        }],
        baseline_call_frames: vec![],
        baseline_opcode_counts: vec![],
        baseline_event_logs: vec![],
        schedule_event_logs: vec![EventLog {
            log_index: 0,
            address: Address::repeat_byte(0x22),
            topics: vec![B256::repeat_byte(0xab)],
            data: Bytes::from(vec![1u8, 2, 3]),
        }],
    };

    db.record_block_output(&BlockOutput {
        coverage: BlockCoverageRow {
            schedule_name: "amsterdam".into(),
            schedule_config_hash: "config-v1".into(),
            block_number: 22_000_000,
            block_hash: B256::repeat_byte(0xb1),
            parent_hash: B256::repeat_byte(0xa0),
            timestamp: 1_700_000_000,
            // Two txs: the stored (per-tx forensic) break below, plus one
            // gas-only tx that rolls into the aggregate class summary.
            tx_count: 2,
            tx_count_unchanged: 0,
            tx_count_gas_only: 1,
            tx_count_stored: 1,
            block_gas_used: 15_000_000,
            block_gas_limit: 30_000_000,
            block_base_fee_per_gas: Some(12_000_000_000),
        },
        // One aggregate-class summary keyed by `class` (the gas-only cohort).
        summaries: vec![BlockSummaryRow {
            schedule_name: "amsterdam".into(),
            block_number: 22_000_000,
            class: AggregateClass::GasOnly,
            tx_count: 1,
            gas_delta_sum: Some(20_000),
            gas_delta_sum_sq: Some(20_000i64 * 20_000),
            gas_delta_min: Some(20_000),
            gas_delta_max: Some(20_000),
            gas_delta_log2_hist: Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            opcode_totals: vec![OpcodeBucketTotal {
                opcode: 0x55,
                count: 3,
                gas_baseline: 60_000,
                gas_schedule: 75_000,
            }],
            state_gas_sum: None,
            state_gas_spillover_sum: None,
            multiplier_log2_hist: None,
            tx_count_creation: None,
            tx_count_authorization: None,
            tx_count_runtime_state: None,
            tx_count_no_state: None,
            cold_account_access_count: None,
            storage_drivers: None,
            account_drivers: None,
            tx_count_type_legacy: Some(0),
            tx_count_type_access_list: Some(0),
            tx_count_type_dynamic_fee: Some(1),
            tx_count_type_blob: Some(0),
            tx_count_type_set_code: Some(0),
            tx_count_type_other: Some(0),
            tx_count_simple_transfer: Some(0),
            tx_count_contract_call: Some(1),
            // 20_000 gas delta on a 100_000-gas baseline = +20% → bin [10, 25).
            gas_delta_pct_hist: Some([0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0]),
            baseline_gas_used_sum: Some(100_000),
        }],
        drill_ins: vec![drill_in],
        // One recipient rollup row keyed by the same aggregate `class`.
        recipients: vec![RecipientRow {
            schedule_name: "amsterdam".into(),
            block_number: 22_000_000,
            class: AggregateClass::GasOnly,
            recipient: format!("{:#x}", Address::repeat_byte(0x22)),
            top_selector: [0x12, 0x34, 0x56, 0x78],
            tx_count: 1,
            gas_delta_sum_succeeding: 20_000,
        }],
        // v12: one row per tx in the block when the producer collects the
        // spine — the forensic tx plus the gas-only tx that only appears in the
        // class summary above.
        tx_gas_results: vec![
            TxGasResultRow {
                schedule_name: "amsterdam".into(),
                schedule_config_hash: "config-v1".into(),
                block_number: 22_000_000,
                tx_index: 0,
                tx_hash: B256::repeat_byte(0x11),
                tx_type: 2,
                tx_gas_limit: 200_000,
                max_fee_per_gas: "20000000000".into(),
                max_priority_fee_per_gas: Some("1000000000".into()),
                baseline_success: true,
                baseline_gas_used: 100_000,
                baseline_total_gas_spent: 100_000,
                schedule_success: true,
                schedule_gas_used: 120_000,
                schedule_total_gas_spent: 120_000,
                schedule_gas_refunded: 0,
                schedule_floor_gas: 0,
                schedule_state_gas_spent: 97_920,
                schedule_intrinsic_gas: Some(12_000),
                min_multiplier_to_succeed: Some(0.6),
            },
            TxGasResultRow {
                schedule_name: "amsterdam".into(),
                schedule_config_hash: "config-v1".into(),
                block_number: 22_000_000,
                tx_index: 1,
                tx_hash: B256::repeat_byte(0x22),
                tx_type: 2,
                tx_gas_limit: 150_000,
                max_fee_per_gas: "20000000000".into(),
                max_priority_fee_per_gas: Some("1000000000".into()),
                baseline_success: true,
                baseline_gas_used: 100_000,
                baseline_total_gas_spent: 100_000,
                schedule_success: true,
                schedule_gas_used: 120_000,
                schedule_total_gas_spent: 120_000,
                schedule_gas_refunded: 0,
                schedule_floor_gas: 0,
                schedule_state_gas_spent: 0,
                schedule_intrinsic_gas: Some(12_000),
                min_multiplier_to_succeed: Some(0.8),
            },
        ],
    })
    .unwrap();

    println!("wrote {path}");
}
