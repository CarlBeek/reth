//! Per-block bucket aggregator for the `DuckDB` write path.
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
    database::{BlockCoverageRow, BlockOutput, BlockSummaryRow, DrillInRecord, OpcodeBucketTotal},
    divergence::{Bucket, FrameOpcodeCounts},
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
    /// Block number being aggregated.
    pub block_number: u64,
    /// Block hash being aggregated.
    pub block_hash: B256,
    /// Parent block hash (used to detect reorgs at write time).
    pub parent_hash: B256,
    /// Block timestamp (Unix seconds).
    pub timestamp: u64,
}

/// Accumulates per-tx classifications and metrics for a single block.
///
/// Construct with [`BlockAggregator::start_block`], feed each tx via
/// [`BlockAggregator::observe_tx`], and call [`BlockAggregator::finish_block`]
/// to materialise the [`BlockOutput`] for the `DuckDB` writer.
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

#[derive(Debug)]
struct BucketAccumulator {
    tx_count: u32,
    gas_delta_sum: i64,
    gas_delta_sum_sq: i64,
    gas_delta_min: Option<i64>,
    gas_delta_max: Option<i64>,
    /// 12-bin log2 histogram of `abs(gas_delta)`:
    ///   bin 0 → `gas_delta` == 0
    ///   bin i (1..=10) → 2^(i-1) <= |`gas_delta`| < 2^i, except bin 1 also
    ///                    counts 1 ≤ |`gas_delta`| < 2.
    ///   bin 11 → |`gas_delta`| >= 2^10 (1024)
    gas_delta_log2_hist: [i32; 12],

    // EIP-8037 state-gas aggregates. Zero for schedules that don't track
    // state gas; emitted as None on the row when the bucket saw zero
    // state-gas activity so the column reads cleanly as "n/a".
    state_gas_sum: u64,
    state_gas_spillover_sum: u64,

    /// 12-bin histogram of `min_multiplier_to_succeed` values. See
    /// [`multiplier_bin`] for boundaries. `bin 0` collects txs where the
    /// replay didn't resolve a multiplier (None) — failures or
    /// not-applicable.
    multiplier_log2_hist: [i32; 12],
    /// Number of observations that contributed to `multiplier_log2_hist`
    /// (i.e. had a meaningful multiplier or were explicitly counted as
    /// None). Used to decide whether to emit the histogram at all.
    multiplier_observations: u32,

    // 7904 / state-gas category counters. A single tx may increment more
    // than one of these (e.g. a contract creation that also burned
    // runtime state gas) — they're independent indicators rather than
    // mutually-exclusive buckets.
    tx_count_creation: u32,
    tx_count_authorization: u32,
    tx_count_runtime_state: u32,
    tx_count_no_state: u32,

    // Per-opcode totals — counts + baseline / schedule gas — summed
    // across every frame of every tx in this bucket for this block.
    // Stored dense (256 wide) for cache-friendly accumulation; emitted
    // sparse (nonzero only) in `finish_block` so the JSON column stays
    // compact.
    //
    // Default isn't auto-derived for arrays larger than 32 elements;
    // we provide a manual impl below.
    opcode_counts: [u64; 256],
    opcode_gas_baseline: [u64; 256],
    opcode_gas_schedule: [u64; 256],
}

impl Default for BucketAccumulator {
    fn default() -> Self {
        Self {
            tx_count: 0,
            gas_delta_sum: 0,
            gas_delta_sum_sq: 0,
            gas_delta_min: None,
            gas_delta_max: None,
            gas_delta_log2_hist: [0; 12],
            state_gas_sum: 0,
            state_gas_spillover_sum: 0,
            multiplier_log2_hist: [0; 12],
            multiplier_observations: 0,
            tx_count_creation: 0,
            tx_count_authorization: 0,
            tx_count_runtime_state: 0,
            tx_count_no_state: 0,
            opcode_counts: [0; 256],
            opcode_gas_baseline: [0; 256],
            opcode_gas_schedule: [0; 256],
        }
    }
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

/// 12-bin histogram of `min_multiplier_to_succeed` (replay's required
/// gas-limit inflation). Mirrors the bin labels the dashboard expects:
/// `None` (no multiplier), ≤1.0, ≤1.25, ≤1.5, ≤2, ≤3, ≤4, ≤5, ≤6, ≤7,
/// ≤8, >8.
fn multiplier_bin(value: Option<f64>) -> usize {
    let Some(v) = value else {
        return 0;
    };
    if v <= 1.0 {
        1
    } else if v <= 1.25 {
        2
    } else if v <= 1.5 {
        3
    } else if v <= 2.0 {
        4
    } else if v <= 3.0 {
        5
    } else if v <= 4.0 {
        6
    } else if v <= 5.0 {
        7
    } else if v <= 6.0 {
        8
    } else if v <= 7.0 {
        9
    } else if v <= 8.0 {
        10
    } else {
        11
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

/// Observation passed to [`BlockAggregator::observe_tx`].
///
/// Bundles the bucket assignment with every per-tx metric the aggregator
/// rolls into `block_summaries`. New fields can be added here without
/// changing the call signature.
#[derive(Debug, Clone)]
pub struct TxObservation {
    /// Bucket the classifier assigned to this (tx, schedule).
    pub bucket: Bucket,
    /// `schedule_gas_used - baseline_gas_used`.
    pub gas_delta: i64,
    /// Net EIP-8037 state gas the schedule actually charged. Zero for
    /// schedules that don't engage state gas.
    pub state_gas_spent: u64,
    /// Portion of `state_gas_spent` that spilled into regular gas
    /// because the reservoir wasn't large enough.
    pub state_gas_spillover: u64,
    /// Smallest gas-limit multiplier the schedule required for the
    /// replay to succeed. `None` when the replay halted regardless of
    /// gas (revert / non-OOG halt) or at the inflated replay cap.
    pub min_multiplier_to_succeed: Option<f64>,
    /// Whether the tx is a contract creation.
    pub is_creation: bool,
    /// Whether the tx carries an EIP-7702 authorization list.
    pub has_authorization: bool,
    /// Whether any state gas was charged during execution (i.e.
    /// post-intrinsic). False when the tx only paid the initial state-gas
    /// allotment with no spillover.
    pub has_runtime_state: bool,
    /// Per-tx drill-in record — populated only for drill-in buckets
    /// (`EventLogsChanged` / `ContractBroken`); ignored for aggregate
    /// buckets.
    pub drill_in_record: Option<DrillInRecord>,
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

    /// Record a single tx's classification + per-tx metrics + per-frame
    /// opcode counts. Drill-in records are kept only for
    /// `bucket.is_drill_in()`; aggregate buckets discard them after
    /// rolling the metric counters.
    ///
    /// `opcode_frames` is the inspector's per-frame opcode counter; we
    /// fold every frame's nonzero opcodes into the bucket's running
    /// totals so `block_summaries.opcode_totals_7904` ends up with the
    /// full opcode-level gas profile of each bucket. Pass an empty
    /// slice when there's no per-frame data (e.g. a baseline-only
    /// observation or a test).
    pub fn observe_tx(&mut self, obs: TxObservation, opcode_frames: &[FrameOpcodeCounts]) {
        self.tx_count += 1;
        let acc = self.buckets.entry(obs.bucket).or_default();
        acc.tx_count += 1;
        acc.observe_gas_delta(obs.gas_delta);

        // 8037 state-gas aggregates.
        acc.state_gas_sum = acc.state_gas_sum.saturating_add(obs.state_gas_spent);
        acc.state_gas_spillover_sum =
            acc.state_gas_spillover_sum.saturating_add(obs.state_gas_spillover);

        // Multiplier histogram. We bin every tx (including those without
        // a meaningful multiplier — they land in bin 0) so the histogram's
        // bin sum equals `tx_count` and consumers can normalize.
        let mbin = multiplier_bin(obs.min_multiplier_to_succeed);
        acc.multiplier_log2_hist[mbin] = acc.multiplier_log2_hist[mbin].saturating_add(1);
        acc.multiplier_observations = acc.multiplier_observations.saturating_add(1);

        // 7904 category counters — independent indicators, so a single
        // tx may bump multiple.
        if obs.is_creation {
            acc.tx_count_creation += 1;
        }
        if obs.has_authorization {
            acc.tx_count_authorization += 1;
        }
        if obs.has_runtime_state {
            acc.tx_count_runtime_state += 1;
        } else {
            acc.tx_count_no_state += 1;
        }

        // Per-opcode totals. Sum across every frame; the bucket's dense
        // 256-wide arrays absorb everything. saturating_add guards the
        // pathological case of a single opcode running ~2^64 times in
        // one block, which can't happen but is cheap insurance.
        for frame in opcode_frames {
            for (opcode_byte, count, gas_baseline, gas_schedule) in frame.nonzero() {
                let i = opcode_byte as usize;
                acc.opcode_counts[i] = acc.opcode_counts[i].saturating_add(count);
                acc.opcode_gas_baseline[i] =
                    acc.opcode_gas_baseline[i].saturating_add(gas_baseline);
                acc.opcode_gas_schedule[i] =
                    acc.opcode_gas_schedule[i].saturating_add(gas_schedule);
            }
        }

        if obs.bucket.is_drill_in() &&
            let Some(record) = obs.drill_in_record
        {
            self.drill_ins.push(record);
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

            // Only emit the 8037 / multiplier columns when the bucket
            // actually saw the relevant signal — `None` reads cleanly as
            // "n/a for this bucket" in the dashboard.
            let state_gas_sum = (acc.state_gas_sum > 0).then_some(acc.state_gas_sum);
            let state_gas_spillover_sum =
                (acc.state_gas_spillover_sum > 0).then_some(acc.state_gas_spillover_sum);
            let multiplier_log2_hist =
                (acc.multiplier_observations > 0).then_some(acc.multiplier_log2_hist);

            // Collapse the dense 256-wide opcode arrays into a sparse
            // list of `OpcodeBucketTotal`. Skip entries where every
            // counter is zero so the JSON column stays compact.
            let mut opcode_totals_7904 = Vec::new();
            for i in 0..256 {
                let count = acc.opcode_counts[i];
                let gas_baseline = acc.opcode_gas_baseline[i];
                let gas_schedule = acc.opcode_gas_schedule[i];
                if count != 0 || gas_baseline != 0 || gas_schedule != 0 {
                    opcode_totals_7904.push(OpcodeBucketTotal {
                        opcode: i as u8,
                        count,
                        gas_baseline,
                        gas_schedule,
                    });
                }
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
                opcode_totals_7904,
                state_gas_sum,
                state_gas_spillover_sum,
                multiplier_log2_hist,
                tx_count_creation: Some(acc.tx_count_creation),
                tx_count_authorization: Some(acc.tx_count_authorization),
                tx_count_runtime_state: Some(acc.tx_count_runtime_state),
                tx_count_no_state: Some(acc.tx_count_no_state),
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

    /// Build a minimal observation with everything-zero defaults.
    fn obs(bucket: Bucket, gas_delta: i64) -> TxObservation {
        TxObservation {
            bucket,
            gas_delta,
            state_gas_spent: 0,
            state_gas_spillover: 0,
            min_multiplier_to_succeed: None,
            is_creation: false,
            has_authorization: false,
            has_runtime_state: false,
            drill_in_record: None,
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
        agg.observe_tx(obs(Bucket::Unchanged, 0), &[]);
        agg.observe_tx(obs(Bucket::GasOnly, 100), &[]);
        agg.observe_tx(obs(Bucket::WalletFixableShallow, 5_000), &[]);
        agg.observe_tx(obs(Bucket::ContractBroken, 50_000), &[]);

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
        let mut o = obs(Bucket::GasOnly, 100);
        o.drill_in_record = Some(dummy_drill_in());
        agg.observe_tx(o, &[]);
        // Drill-in bucket with a record — kept.
        let mut o = obs(Bucket::ContractBroken, -50);
        o.drill_in_record = Some(dummy_drill_in());
        agg.observe_tx(o, &[]);

        let out = agg.finish_block();
        assert_eq!(out.drill_ins.len(), 1, "only ContractBroken should retain its record");
    }

    #[test]
    fn gas_delta_aggregates_per_bucket() {
        let mut agg = BlockAggregator::start_block(meta(), 3);
        agg.observe_tx(obs(Bucket::GasOnly, 100), &[]);
        agg.observe_tx(obs(Bucket::GasOnly, -50), &[]);
        agg.observe_tx(obs(Bucket::GasOnly, 200), &[]);

        let out = agg.finish_block();
        let summary = out.summaries.iter().find(|s| s.bucket == Bucket::GasOnly).unwrap();
        assert_eq!(summary.tx_count, 3);
        assert_eq!(summary.gas_delta_sum, Some(100 - 50 + 200));
        assert_eq!(summary.gas_delta_sum_sq, Some(100i64 * 100 + 50i64 * 50 + 200i64 * 200));
        assert_eq!(summary.gas_delta_min, Some(-50));
        assert_eq!(summary.gas_delta_max, Some(200));
    }

    #[test]
    fn state_gas_aggregates_only_emit_when_non_zero() {
        let mut agg = BlockAggregator::start_block(meta(), 2);
        // Bucket A: state-gas-active tx.
        let mut o = obs(Bucket::GasOnly, 0);
        o.state_gas_spent = 5_000;
        o.state_gas_spillover = 1_500;
        agg.observe_tx(o, &[]);
        // Bucket B: no state gas activity → state_gas_sum should be None.
        agg.observe_tx(obs(Bucket::Unchanged, 0), &[]);

        let out = agg.finish_block();
        let gas_only = out.summaries.iter().find(|s| s.bucket == Bucket::GasOnly).unwrap();
        assert_eq!(gas_only.state_gas_sum, Some(5_000));
        assert_eq!(gas_only.state_gas_spillover_sum, Some(1_500));
        let unchanged = out.summaries.iter().find(|s| s.bucket == Bucket::Unchanged).unwrap();
        assert_eq!(unchanged.state_gas_sum, None);
        assert_eq!(unchanged.state_gas_spillover_sum, None);
    }

    #[test]
    fn category_counters_increment_independently() {
        // A tx can be a contract creation AND carry an authorization AND
        // burn runtime state gas. All three counters should fire.
        let mut agg = BlockAggregator::start_block(meta(), 1);
        agg.observe_tx(
            TxObservation {
                bucket: Bucket::GasOnly,
                gas_delta: 0,
                state_gas_spent: 1_000,
                state_gas_spillover: 0,
                min_multiplier_to_succeed: None,
                is_creation: true,
                has_authorization: true,
                has_runtime_state: true,
                drill_in_record: None,
            },
            &[],
        );
        let out = agg.finish_block();
        let summary = &out.summaries[0];
        assert_eq!(summary.tx_count_creation, Some(1));
        assert_eq!(summary.tx_count_authorization, Some(1));
        assert_eq!(summary.tx_count_runtime_state, Some(1));
        // `has_runtime_state` was true, so the no-state counter stays 0.
        assert_eq!(summary.tx_count_no_state, Some(0));
    }

    #[test]
    fn no_state_counter_fires_when_no_runtime_state_gas() {
        let mut agg = BlockAggregator::start_block(meta(), 1);
        agg.observe_tx(obs(Bucket::Unchanged, 0), &[]);
        let out = agg.finish_block();
        let summary = &out.summaries[0];
        assert_eq!(summary.tx_count_no_state, Some(1));
        assert_eq!(summary.tx_count_runtime_state, Some(0));
    }

    #[test]
    fn multiplier_bin_boundaries() {
        // Every "≤ N" interval lands in the expected bin.
        assert_eq!(multiplier_bin(None), 0);
        assert_eq!(multiplier_bin(Some(0.5)), 1);
        assert_eq!(multiplier_bin(Some(1.0)), 1);
        assert_eq!(multiplier_bin(Some(1.1)), 2);
        assert_eq!(multiplier_bin(Some(1.25)), 2);
        assert_eq!(multiplier_bin(Some(1.5)), 3);
        assert_eq!(multiplier_bin(Some(2.0)), 4);
        assert_eq!(multiplier_bin(Some(3.0)), 5);
        assert_eq!(multiplier_bin(Some(4.0)), 6);
        assert_eq!(multiplier_bin(Some(5.0)), 7);
        assert_eq!(multiplier_bin(Some(6.0)), 8);
        assert_eq!(multiplier_bin(Some(7.0)), 9);
        assert_eq!(multiplier_bin(Some(8.0)), 10);
        assert_eq!(multiplier_bin(Some(8.001)), 11);
        assert_eq!(multiplier_bin(Some(100.0)), 11);
    }

    #[test]
    fn multiplier_log2_hist_bins_observations() {
        let mut agg = BlockAggregator::start_block(meta(), 3);
        let mut o = obs(Bucket::GasOnly, 0);
        o.min_multiplier_to_succeed = Some(1.0); // bin 1
        agg.observe_tx(o, &[]);
        let mut o = obs(Bucket::GasOnly, 0);
        o.min_multiplier_to_succeed = Some(1.4); // bin 3 (1.25 < 1.4 ≤ 1.5)
        agg.observe_tx(o, &[]);
        let mut o = obs(Bucket::GasOnly, 0);
        o.min_multiplier_to_succeed = None; // bin 0
        agg.observe_tx(o, &[]);

        let out = agg.finish_block();
        let summary = &out.summaries[0];
        let hist = summary.multiplier_log2_hist.expect("histogram emitted");
        assert_eq!(hist[0], 1, "None landed in bin 0");
        assert_eq!(hist[1], 1, "1.0 landed in bin 1");
        assert_eq!(hist[3], 1, "1.4 landed in bin 3");
        // Sum equals tx_count.
        let total: i32 = hist.iter().sum();
        assert_eq!(total as u32, summary.tx_count);
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
        use crate::database::DivergenceRow;
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
                replay_halt_oog: None,
            },
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        }
    }

    /// The per-frame opcode counts produced by the inspector should
    /// fold cleanly into the bucket's aggregate `opcode_totals_7904`,
    /// summed across every frame of every tx in the same bucket. Zero-
    /// count opcodes shouldn't appear in the emitted sparse list.
    #[test]
    fn opcode_totals_aggregate_across_frames_and_txs() {
        let mut agg = BlockAggregator::start_block(meta(), 2);

        // tx 1: two frames each running KECCAK256 (0x20) and SLOAD (0x54).
        let mut frame_a = FrameOpcodeCounts::new(0);
        frame_a.counts[0x20] = 3;
        frame_a.gas_baseline[0x20] = 90;
        frame_a.gas_schedule[0x20] = 135;
        frame_a.counts[0x54] = 1;
        frame_a.gas_baseline[0x54] = 800;
        frame_a.gas_schedule[0x54] = 800;

        let mut frame_b = FrameOpcodeCounts::new(1);
        frame_b.counts[0x20] = 5;
        frame_b.gas_baseline[0x20] = 150;
        frame_b.gas_schedule[0x20] = 225;

        agg.observe_tx(obs(Bucket::ContractBroken, 100), &[frame_a, frame_b]);

        // tx 2 (same bucket): one frame with SSTORE (0x55).
        let mut frame_c = FrameOpcodeCounts::new(0);
        frame_c.counts[0x55] = 2;
        frame_c.gas_baseline[0x55] = 40_000;
        frame_c.gas_schedule[0x55] = 50_000;
        agg.observe_tx(obs(Bucket::ContractBroken, 200), &[frame_c]);

        // tx 3 (different bucket): should NOT mix into ContractBroken.
        let mut frame_d = FrameOpcodeCounts::new(0);
        frame_d.counts[0x20] = 99; // unrelated KECCAK count
        agg.observe_tx(obs(Bucket::GasOnly, 5), &[frame_d]);

        let out = agg.finish_block();
        let broken = out
            .summaries
            .iter()
            .find(|s| s.bucket == Bucket::ContractBroken)
            .expect("ContractBroken summary emitted");

        // Sparse list — KECCAK256 (8 = 3+5), SLOAD (1), SSTORE (2). No 0-count rows.
        assert_eq!(broken.opcode_totals_7904.len(), 3);
        let by_op: std::collections::BTreeMap<u8, _> = broken
            .opcode_totals_7904
            .iter()
            .map(|t| (t.opcode, (t.count, t.gas_baseline, t.gas_schedule)))
            .collect();
        assert_eq!(by_op[&0x20], (8, 240, 360));
        assert_eq!(by_op[&0x54], (1, 800, 800));
        assert_eq!(by_op[&0x55], (2, 40_000, 50_000));

        // GasOnly bucket's KECCAK total is isolated.
        let gas_only = out
            .summaries
            .iter()
            .find(|s| s.bucket == Bucket::GasOnly)
            .expect("GasOnly summary emitted");
        assert_eq!(gas_only.opcode_totals_7904.len(), 1);
        assert_eq!(gas_only.opcode_totals_7904[0].opcode, 0x20);
        assert_eq!(gas_only.opcode_totals_7904[0].count, 99);
    }
}
