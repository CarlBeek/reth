//! Per-block bucket aggregator for the DuckDB write path.
//!
//! `BlockAggregator` buffers every tx's classification across a single
//! (schedule, block) and flushes the result as a [`BlockOutput`] at
//! `finish_block()`. Drill-in buckets (`EventLogsChanged`,
//! `ContractBroken`) keep their full per-tx record; aggregate buckets
//! roll up into `block_summaries` rows so we don't pay per-tx storage
//! for them.
//!
//! See `crates/research/docs/storage-redesign.md` for the bucket /
//! storage rules and the schema.

use crate::{
    database_duckdb::{BlockCoverageRow, BlockOutput, BlockSummaryRow, DrillInRecord},
    divergence::Bucket,
};
use alloy_primitives::B256;
use std::collections::BTreeMap;

/// Inputs the aggregator needs the moment the block starts.
#[derive(Debug, Clone)]
pub struct BlockMeta {
    /// Schedule that's producing this output (one aggregator per schedule).
    pub schedule_name: String,
    /// Schedule's `config_fingerprint` hashed — same value the dedupe
    /// query keys on.
    pub schedule_config_hash: String,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    /// Block timestamp (Unix seconds).
    pub timestamp: u64,
}

/// Accumulates per-tx classifications and metrics for a single block.
///
/// Construct with [`BlockAggregator::start_block`], feed each tx via
/// [`BlockAggregator::observe_tx`], and call [`BlockAggregator::finish_block`]
/// to materialise the [`BlockOutput`] for the DuckDB writer.
#[derive(Debug)]
pub struct BlockAggregator {
    meta: BlockMeta,
    /// Total tx count seen — included in `block_coverage.tx_count`.
    tx_count: u32,
    /// Per-bucket counters in deterministic order so the emitted summary
    /// rows are stable across runs.
    buckets: BTreeMap<Bucket, BucketAccumulator>,
    /// Drill-in records collected for buckets where we keep per-tx
    /// state. The order matches `observe_tx` invocations.
    drill_ins: Vec<DrillInRecord>,
}

#[derive(Debug, Default)]
struct BucketAccumulator {
    tx_count: u32,
    gas_delta_sum: i64,
    gas_delta_sum_sq: i64,
    gas_delta_min: Option<i64>,
    gas_delta_max: Option<i64>,
    /// 12-bin log2 histogram of `abs(gas_delta)`:
    ///   bin 0 → gas_delta == 0
    ///   bin i (1..=10) → 2^(i-1) <= |gas_delta| < 2^i, except bin 1 also
    ///                    counts 1 ≤ |gas_delta| < 2.
    ///   bin 11 → |gas_delta| >= 2^10 (1024)
    gas_delta_log2_hist: [i32; 12],
}

impl BucketAccumulator {
    fn observe_gas_delta(&mut self, gas_delta: i64) {
        self.gas_delta_sum = self.gas_delta_sum.saturating_add(gas_delta);
        // i64 mul guard: |gas_delta| up to ~30M per tx → squared ≤ 9 × 10^14;
        // 200 txs per block → 1.8 × 10^17, well under i64::MAX. Saturating
        // to be safe under pathological deltas.
        let sq = gas_delta.saturating_mul(gas_delta);
        self.gas_delta_sum_sq = self.gas_delta_sum_sq.saturating_add(sq);
        self.gas_delta_min = Some(self.gas_delta_min.map_or(gas_delta, |m| m.min(gas_delta)));
        self.gas_delta_max = Some(self.gas_delta_max.map_or(gas_delta, |m| m.max(gas_delta)));
        let bin = log2_bin(gas_delta);
        self.gas_delta_log2_hist[bin] = self.gas_delta_log2_hist[bin].saturating_add(1);
    }
}

/// Pick the bin index for a signed gas delta. Bin 0 = exact zero;
/// bin 11 = `|delta| >= 1024`. See [`BucketAccumulator::gas_delta_log2_hist`].
fn log2_bin(gas_delta: i64) -> usize {
    let abs = gas_delta.unsigned_abs();
    if abs == 0 {
        return 0;
    }
    // bits = number of bits needed = 1 + floor(log2(abs)).
    let bits = 64 - abs.leading_zeros() as usize;
    // bits 1 → bin 1, bits 2 → bin 2, ..., bits 10 → bin 10, bits 11+ → bin 11.
    bits.min(11)
}

impl BlockAggregator {
    /// Begin a new block. `tx_count_hint` is the expected number of txs
    /// so the drill-in vec can pre-allocate; the actual count is tracked
    /// by `observe_tx`.
    pub fn start_block(meta: BlockMeta, tx_count_hint: usize) -> Self {
        Self {
            meta,
            tx_count: 0,
            buckets: BTreeMap::new(),
            drill_ins: Vec::with_capacity(tx_count_hint),
        }
    }

    /// Record a single tx's classification + gas delta. The
    /// `drill_in_record` payload is only honoured when `bucket.is_drill_in()`
    /// — for aggregate-only buckets the per-tx data has already been
    /// summed into running totals.
    pub fn observe_tx(
        &mut self,
        bucket: Bucket,
        gas_delta: i64,
        drill_in_record: Option<DrillInRecord>,
    ) {
        self.tx_count += 1;
        let acc = self.buckets.entry(bucket).or_default();
        acc.tx_count += 1;
        acc.observe_gas_delta(gas_delta);
        if bucket.is_drill_in() {
            if let Some(record) = drill_in_record {
                self.drill_ins.push(record);
            }
        }
    }

    /// Finalise the block. Builds the `block_coverage` row, one
    /// `block_summaries` row per non-empty bucket, and bundles the
    /// drill-in records. Consumes `self` so the aggregator can't be
    /// reused for another block — callers create a fresh one per block.
    pub fn finish_block(self) -> BlockOutput {
        let mut coverage = BlockCoverageRow {
            schedule_name: self.meta.schedule_name.clone(),
            schedule_config_hash: self.meta.schedule_config_hash.clone(),
            block_number: self.meta.block_number,
            block_hash: self.meta.block_hash,
            parent_hash: self.meta.parent_hash,
            timestamp: self.meta.timestamp,
            tx_count: self.tx_count,
            tx_count_unchanged: 0,
            tx_count_trace_only: 0,
            tx_count_gas_only: 0,
            tx_count_event_logs_changed: 0,
            tx_count_wallet_fixable_shallow: 0,
            tx_count_wallet_fixable_deep_chain: 0,
            tx_count_contract_broken: 0,
        };
        let mut summaries = Vec::with_capacity(self.buckets.len());

        for (bucket, acc) in self.buckets {
            match bucket {
                Bucket::Unchanged => coverage.tx_count_unchanged = acc.tx_count,
                Bucket::TraceOnly => coverage.tx_count_trace_only = acc.tx_count,
                Bucket::GasOnly => coverage.tx_count_gas_only = acc.tx_count,
                Bucket::EventLogsChanged => coverage.tx_count_event_logs_changed = acc.tx_count,
                Bucket::WalletFixableShallow => {
                    coverage.tx_count_wallet_fixable_shallow = acc.tx_count
                }
                Bucket::WalletFixableDeepChain => {
                    coverage.tx_count_wallet_fixable_deep_chain = acc.tx_count
                }
                Bucket::ContractBroken => coverage.tx_count_contract_broken = acc.tx_count,
            }

            summaries.push(BlockSummaryRow {
                schedule_name: self.meta.schedule_name.clone(),
                block_number: self.meta.block_number,
                bucket,
                tx_count: acc.tx_count,
                gas_delta_sum: Some(acc.gas_delta_sum),
                gas_delta_sum_sq: Some(acc.gas_delta_sum_sq),
                gas_delta_min: acc.gas_delta_min,
                gas_delta_max: acc.gas_delta_max,
                gas_delta_log2_hist: Some(acc.gas_delta_log2_hist),
                // 8037 fields and 7904 opcode totals are deferred to a
                // follow-up — the aggregator doesn't see per-frame data
                // yet. Schema columns accept NULL.
                state_gas_sum: None,
                state_gas_spillover_sum: None,
                multiplier_log2_hist: None,
                tx_count_creation: None,
                tx_count_authorization: None,
                tx_count_runtime_state: None,
                tx_count_no_state: None,
            });
        }

        BlockOutput { coverage, summaries, drill_ins: self.drill_ins }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> BlockMeta {
        BlockMeta {
            schedule_name: "test".to_string(),
            schedule_config_hash: "cfg".to_string(),
            block_number: 42,
            block_hash: B256::repeat_byte(0x42),
            parent_hash: B256::repeat_byte(0x41),
            timestamp: 1_700_000_000,
        }
    }

    #[test]
    fn empty_block_emits_zero_count_coverage() {
        let agg = BlockAggregator::start_block(meta(), 0);
        let out = agg.finish_block();
        assert_eq!(out.coverage.tx_count, 0);
        assert!(out.summaries.is_empty());
        assert!(out.drill_ins.is_empty());
    }

    #[test]
    fn coverage_counts_split_by_bucket() {
        let mut agg = BlockAggregator::start_block(meta(), 4);
        agg.observe_tx(Bucket::Unchanged, 0, None);
        agg.observe_tx(Bucket::GasOnly, 100, None);
        agg.observe_tx(Bucket::WalletFixableShallow, 5_000, None);
        // ContractBroken is drill-in, but we don't have a record fixture
        // here — passing None just means the per-tx record isn't kept.
        agg.observe_tx(Bucket::ContractBroken, 50_000, None);

        let out = agg.finish_block();
        assert_eq!(out.coverage.tx_count, 4);
        assert_eq!(out.coverage.tx_count_unchanged, 1);
        assert_eq!(out.coverage.tx_count_gas_only, 1);
        assert_eq!(out.coverage.tx_count_wallet_fixable_shallow, 1);
        assert_eq!(out.coverage.tx_count_contract_broken, 1);
        assert_eq!(out.coverage.tx_count_event_logs_changed, 0);
        // One summary per touched bucket.
        assert_eq!(out.summaries.len(), 4);
    }

    #[test]
    fn drill_in_records_collected_only_for_drill_in_buckets() {
        let mut agg = BlockAggregator::start_block(meta(), 2);

        // Aggregate bucket — drill_in_record is ignored even if passed.
        agg.observe_tx(Bucket::GasOnly, 100, Some(dummy_drill_in()));
        // Drill-in bucket with a record — kept.
        agg.observe_tx(Bucket::ContractBroken, -50, Some(dummy_drill_in()));

        let out = agg.finish_block();
        assert_eq!(out.drill_ins.len(), 1, "only ContractBroken should retain its record");
    }

    #[test]
    fn gas_delta_aggregates_per_bucket() {
        let mut agg = BlockAggregator::start_block(meta(), 3);
        agg.observe_tx(Bucket::GasOnly, 100, None);
        agg.observe_tx(Bucket::GasOnly, -50, None);
        agg.observe_tx(Bucket::GasOnly, 200, None);

        let out = agg.finish_block();
        let summary = out.summaries.iter().find(|s| s.bucket == Bucket::GasOnly).unwrap();
        assert_eq!(summary.tx_count, 3);
        assert_eq!(summary.gas_delta_sum, Some(100 - 50 + 200));
        assert_eq!(summary.gas_delta_sum_sq, Some(100i64 * 100 + 50i64 * 50 + 200i64 * 200));
        assert_eq!(summary.gas_delta_min, Some(-50));
        assert_eq!(summary.gas_delta_max, Some(200));
    }

    #[test]
    fn log2_bin_boundaries() {
        // Bin 0: exact zero.
        assert_eq!(log2_bin(0), 0);
        // Bin 1: |delta| ∈ {1}. (bits=1)
        assert_eq!(log2_bin(1), 1);
        assert_eq!(log2_bin(-1), 1);
        // Bin 2: |delta| ∈ {2, 3}. (bits=2)
        assert_eq!(log2_bin(2), 2);
        assert_eq!(log2_bin(3), 2);
        // Bin 10: |delta| ∈ [512, 1023]. (bits=10)
        assert_eq!(log2_bin(512), 10);
        assert_eq!(log2_bin(1023), 10);
        // Bin 11 catch-all: |delta| ≥ 1024.
        assert_eq!(log2_bin(1024), 11);
        assert_eq!(log2_bin(1_000_000), 11);
    }

    fn dummy_drill_in() -> DrillInRecord {
        use crate::database_duckdb::DivergenceRow;
        use alloy_primitives::Address;
        DrillInRecord {
            divergence: DivergenceRow {
                schedule_name: "test".to_string(),
                schedule_config_hash: "cfg".to_string(),
                block_number: 42,
                tx_index: 0,
                tx_hash: B256::ZERO,
                timestamp: 0,
                bucket: Bucket::ContractBroken,
                sender: Address::ZERO,
                recipient: None,
                is_create: false,
                tx_gas_limit: 0,
                baseline_success: true,
                schedule_success: false,
                status_changed: true,
                event_logs_changed: false,
                output_changed: false,
                logs_bloom_changed: false,
                baseline_gas_used: 0,
                schedule_gas_used: 0,
                gas_delta: 0,
                baseline_total_gas_spent: None,
                baseline_gas_refunded: None,
                schedule_total_gas_spent: None,
                schedule_gas_refunded: None,
                schedule_intrinsic_gas: None,
                schedule_floor_gas: None,
                would_fit_in_original_limit: None,
                min_multiplier_to_succeed: None,
                divergence_contract: None,
                divergence_pc: None,
                divergence_call_depth: None,
                divergence_opcode: None,
                oog_contract: None,
                oog_pc: None,
                oog_call_depth: None,
                oog_opcode: None,
                oog_pattern: None,
                oog_gas_remaining: None,
                oog_chain_proportional: None,
                oog_bottleneck_depth: None,
                oog_bottleneck_kind: None,
                schedule_state_gas_spent: None,
                schedule_initial_state_gas: None,
                schedule_initial_reservoir: None,
                runtime_state_gas: None,
                runtime_state_gas_spillover: None,
                state_gas_category: None,
                reservoir_exhausted: None,
            },
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        }
    }
}
