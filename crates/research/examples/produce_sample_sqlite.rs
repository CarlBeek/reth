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
        DivergenceRow, DrillInRecord, OpcodeBucketTotal, OpcodeCountRow,
    },
    divergence::{Bucket, EventLog},
};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/sample.sqlite".into());
    // Wipe any prior file + WAL sidecars.
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path}-wal"));
    let _ = std::fs::remove_file(format!("{path}-shm"));

    let db = DivergenceDatabase::open(&path).expect("open");
    db.record_analysis_run_start("eip-8037", "config-v1", Some("sample"), Some("cross-repo test"))
        .unwrap();

    // One contract-broken tx with a per-frame opcode count + an event log,
    // matching the shape the consumer's dashboard expects.
    let drill_in = DrillInRecord {
        divergence: DivergenceRow {
            schedule_name: "eip-8037".into(),
            schedule_config_hash: "config-v1".into(),
            block_number: 22_000_000,
            tx_index: 0,
            tx_hash: B256::repeat_byte(0xdd),
            timestamp: 1_700_000_000,
            bucket: Bucket::ContractBroken,
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
            would_fit_in_original_limit: Some(false),
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
            schedule_state_gas_spent: None,
            schedule_state_gas_demanded: None,
            schedule_initial_state_gas: None,
            schedule_initial_reservoir: None,
            runtime_state_gas: None,
            runtime_state_gas_spillover: None,
            state_gas_category: None,
            reservoir_exhausted: None,
            replay_halt_oog: None,
            cold_account_code_count: None,
            cold_account_nocode_count: None,
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
            state_gas_running: None,
            deployed_bytecode_len: None,
        }],
        opcode_counts: vec![OpcodeCountRow {
            call_index: 0,
            opcode: 0x55,
            count: 3,
            gas_baseline: 60_000,
            gas_schedule: 75_000,
        }],
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
            schedule_name: "eip-8037".into(),
            schedule_config_hash: "config-v1".into(),
            block_number: 22_000_000,
            block_hash: B256::repeat_byte(0xb1),
            parent_hash: B256::repeat_byte(0xa0),
            timestamp: 1_700_000_000,
            tx_count: 1,
            tx_count_unchanged: 0,
            tx_count_trace_only: 0,
            tx_count_gas_only: 0,
            tx_count_event_logs_changed: 0,
            tx_count_schedule_rescued: 0,
            tx_count_wallet_fixable_shallow: 0,
            tx_count_wallet_fixable_deep_chain: 0,
            tx_count_inconclusive_needs_higher_sweep: 0,
            tx_count_contract_broken: 1,
            tx_count_aa_gas_reestimation: 0,
            block_gas_used: 15_000_000,
            block_gas_limit: 30_000_000,
        },
        summaries: vec![BlockSummaryRow {
            schedule_name: "eip-8037".into(),
            block_number: 22_000_000,
            bucket: Bucket::ContractBroken,
            tx_count: 1,
            gas_delta_sum: Some(20_000),
            gas_delta_sum_sq: Some(20_000i64 * 20_000),
            gas_delta_min: Some(20_000),
            gas_delta_max: Some(20_000),
            gas_delta_log2_hist: Some([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
            opcode_totals_7904: vec![OpcodeBucketTotal {
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
            cold_account_code_count: None,
            cold_account_nocode_count: None,
        }],
        drill_ins: vec![drill_in],
        bucket_recipients: vec![],
    })
    .unwrap();

    println!("wrote {path}");
}
