//! Per-block fact-class aggregator for the `SQLite` write path.
//!
//! `BlockAggregator` buffers every tx's execution facts across a single
//! (schedule, block) and flushes the result as a [`BlockOutput`] at
//! `finish_block()`. Every tx that failed or whose trace diverged from baseline
//! (`store_full_forensics`) keeps its full per-tx [`DrillInRecord`]; the
//! remaining txs — byte-identical (`Unchanged`) or gas-only — roll up into one
//! `block_summaries` row per [`AggregateClass`] so we don't pay per-tx storage
//! for the silent majority.
//!
//! See `crates/research/docs/storage-redesign.md` for the storage rules and the
//! schema.

use crate::{
    database::{
        BlockCoverageRow, BlockOutput, BlockSummaryRow, DrillInRecord, OpcodeBucketTotal,
        RecipientRow,
    },
    divergence::{AggregateClass, FrameOpcodeCounts, StorageDrivers},
};
use alloy_primitives::{Address, B256};
use std::collections::{BTreeMap, HashMap};

/// Max distinct recipients kept per (block, class) in the `block_recipients`
/// rollup. The truncated tail folds into one `__other__` row so the
/// unattributed mass stays quantified. Generous enough that most blocks (a few
/// hundred distinct destinations) aren't truncated at all; bounds the worst case
/// so a pathological block can't balloon the table.
const RECIPIENT_TOP_K: usize = 128;

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
    /// Native block header `gas_used` — actual gas the block consumed.
    pub gas_used: u64,
    /// Native block header `gas_limit` — the protocol cap.
    pub gas_limit: u64,
}

/// Accumulates per-tx execution facts and metrics for a single block.
///
/// Construct with [`BlockAggregator::start_block`], feed each tx via
/// [`BlockAggregator::observe_tx`], and call [`BlockAggregator::finish_block`]
/// to materialise the [`BlockOutput`] for the `SQLite` writer.
#[derive(Debug)]
pub struct BlockAggregator {
    meta: BlockMeta,
    /// Total tx count seen — included in `block_coverage.tx_count`.
    tx_count: u32,
    /// Count of txs that got a per-tx forensic row (failures + trace
    /// divergences) rather than feeding a class aggregate.
    tx_count_stored: u32,
    /// Per-class counters in deterministic order so the emitted summary
    /// rows are stable across runs. Only `Unchanged` / `GasOnly` ever key
    /// this map — stored txs are kept per-tx in `drill_ins` instead.
    classes: BTreeMap<AggregateClass, ClassAccumulator>,
    /// Drill-in records collected for txs where we keep per-tx state
    /// (`store_full_forensics`). The order matches `observe_tx` invocations.
    drill_ins: Vec<DrillInRecord>,
}

#[derive(Debug)]
struct ClassAccumulator {
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
    // state gas; emitted as None on the row when the class saw zero
    // state-gas activity so the column reads cleanly as "n/a".
    state_gas_sum: u64,
    state_gas_spillover_sum: u64,

    /// 12-bin histogram of `min_multiplier_to_succeed` values. See
    /// [`multiplier_bin`] for boundaries. `bin 0` collects txs where the
    /// replay didn't resolve a multiplier (None) — failures or
    /// not-applicable. (Degenerate bin-1 for these always-succeeding classes,
    /// kept for symmetry with the per-tx multiplier data.)
    multiplier_log2_hist: [i32; 12],
    /// Number of observations that contributed to `multiplier_log2_hist`
    /// (i.e. had a meaningful multiplier or were explicitly counted as
    /// None). Used to decide whether to emit the histogram at all.
    multiplier_observations: u32,

    // 7904 / state-gas category counters. A single tx may increment more
    // than one of these (e.g. a contract creation that also burned
    // runtime state gas) — they're independent indicators rather than
    // mutually-exclusive classes.
    tx_count_creation: u32,
    tx_count_authorization: u32,
    tx_count_runtime_state: u32,
    tx_count_no_state: u32,

    // Cold-account code/no-code split (EIP-8038). Sum over the class's txs
    // of cold account accesses whose target carried code
    // (`code_hash != KECCAK_EMPTY`, incl. EIP-7702 delegated) vs not (EOA /
    // empty / non-existent). Zero for schedules that don't price the split;
    // emitted as None when the class saw no cold account accesses.
    cold_account_code_sum: u64,
    cold_account_nocode_sum: u64,

    // EIP-8038 storage-reprice drivers (F8) summed over the class's txs.
    storage_drivers: StorageDrivers,

    // Per-opcode totals — counts + baseline / schedule gas — summed
    // across every frame of every tx in this class for this block.
    // Stored dense (256 wide) for cache-friendly accumulation; emitted
    // sparse (nonzero only) in `finish_block` so the JSON column stays
    // compact.
    //
    // Default isn't auto-derived for arrays larger than 32 elements;
    // we provide a manual impl below.
    opcode_counts: [u64; 256],
    opcode_gas_baseline: [u64; 256],
    opcode_gas_schedule: [u64; 256],

    // Per-recipient attribution for this class, keyed by (to-address as
    // lowercase `{:#x}` hex or the `__create__` sentinel, 4-byte selector).
    // Folded into top-K `block_recipients` rows at `finish_block`.
    // Skipped for `AggregateClass::Unchanged` (nothing diverged → nothing to
    // attribute).
    recipients: HashMap<(String, [u8; 4]), RecipientAcc>,
}

/// One recipient's running totals within a class. `tx_count` is meaningful for
/// every cohort; `gas_delta_sum_succeeding` accumulates `gas_delta` only over
/// txs that succeeded within their original gas limit, so OOG halt-gas never
/// pollutes it (see [`RecipientRow`]).
#[derive(Debug, Default, Clone, Copy)]
struct RecipientAcc {
    tx_count: u32,
    gas_delta_sum_succeeding: i64,
}

impl Default for ClassAccumulator {
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
            cold_account_code_sum: 0,
            cold_account_nocode_sum: 0,
            storage_drivers: StorageDrivers::default(),
            opcode_counts: [0; 256],
            opcode_gas_baseline: [0; 256],
            opcode_gas_schedule: [0; 256],
            recipients: HashMap::new(),
        }
    }
}

impl ClassAccumulator {
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
/// bin 11 = `|delta| >= 1024`. See [`ClassAccumulator::gas_delta_log2_hist`].
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
/// Bundles the per-tx storage decision with every metric the aggregator rolls
/// into `block_summaries`. New fields can be added here without changing the
/// call signature.
#[derive(Debug, Clone)]
pub struct TxObservation {
    /// Execution-fact class for the aggregate rollup. Only consulted when
    /// `store_full_forensics` is false (i.e. `Unchanged` / `GasOnly`).
    pub class: AggregateClass,
    /// Whether this tx gets a full per-tx forensic row (it failed, or its
    /// trace diverged from baseline) instead of feeding a class aggregate.
    pub store_full_forensics: bool,
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
    /// Cold account accesses this tx made whose target carried code
    /// (`code_hash != KECCAK_EMPTY`, incl. EIP-7702 delegated). `None` when the
    /// replay was rejected before classification completed (vs `Some(0)` = ran,
    /// no cold-code access), so unmeasured txs don't dilute the class sum.
    pub cold_account_code_count: Option<u64>,
    /// Cold account accesses this tx made whose target had no code
    /// (EOA / empty / non-existent). `None` when unmeasured (see above).
    pub cold_account_nocode_count: Option<u64>,
    /// EIP-8038 storage-reprice drivers (F8) for this tx. `None` when the replay
    /// was rejected (unmeasured), so it doesn't dilute the class sum.
    pub storage_drivers: Option<StorageDrivers>,
    /// Per-tx drill-in record — populated only when `store_full_forensics`
    /// is set; ignored for the aggregate classes.
    pub drill_in_record: Option<DrillInRecord>,
    /// Tx destination (to-address). `None` for contract creations — recorded
    /// under the `__create__` sentinel in the recipient rollup.
    pub recipient: Option<Address>,
    /// 4-byte function selector (first 4 calldata bytes). `None` for creations
    /// and for calls with fewer than 4 calldata bytes.
    pub selector: Option<[u8; 4]>,
    /// Whether the schedule replay succeeded within the tx's *original* gas
    /// limit. Gates whether `gas_delta` feeds `gas_delta_sum_succeeding` —
    /// OOG-at-higher-tier txs carry halt-gas `gas_delta` and are excluded.
    pub succeeded_within_limit: bool,
}

impl BlockAggregator {
    /// Begin a new block. `tx_count_hint` is the expected number of txs
    /// so the drill-in vec can pre-allocate; the actual count is tracked
    /// by `observe_tx`.
    pub fn start_block(meta: BlockMeta, tx_count_hint: usize) -> Self {
        Self {
            meta,
            tx_count: 0,
            tx_count_stored: 0,
            classes: BTreeMap::new(),
            drill_ins: Vec::with_capacity(tx_count_hint),
        }
    }

    /// Record a single tx's execution facts + per-tx metrics + per-frame
    /// opcode counts. Txs with `store_full_forensics` keep their per-tx
    /// drill-in record and are excluded from the class aggregate; the
    /// remaining `Unchanged` / `GasOnly` txs roll into their class summary.
    ///
    /// `opcode_frames` is the inspector's per-frame opcode counter; we
    /// fold every frame's nonzero opcodes into the class's running
    /// totals so `block_summaries.opcode_totals` ends up with the
    /// full opcode-level gas profile of each class. Pass an empty
    /// slice when there's no per-frame data (e.g. a baseline-only
    /// observation or a test).
    pub fn observe_tx(&mut self, obs: TxObservation, opcode_frames: &[FrameOpcodeCounts]) {
        self.tx_count += 1;

        // Stored txs (failures + trace divergences) get a per-tx forensic row
        // and do NOT feed any class aggregate — their opcode/state/cold/gas
        // data lives in the drill-in record (call frames + per-frame opcode
        // counts + the divergence row).
        if obs.store_full_forensics {
            self.tx_count_stored += 1;
            if let Some(record) = obs.drill_in_record {
                self.drill_ins.push(record);
            }
            return;
        }

        let acc = self.classes.entry(obs.class).or_default();
        acc.tx_count += 1;
        acc.observe_gas_delta(obs.gas_delta);

        // 8037 state-gas aggregates.
        acc.state_gas_sum = acc.state_gas_sum.saturating_add(obs.state_gas_spent);
        acc.state_gas_spillover_sum =
            acc.state_gas_spillover_sum.saturating_add(obs.state_gas_spillover);

        // 8038 cold-account code/no-code split aggregates. Fold only *measured*
        // counts (`Some`); a reject-path tx (`None`) contributes nothing rather
        // than a phantom zero that would bias the class sum low.
        if let Some(c) = obs.cold_account_code_count {
            acc.cold_account_code_sum = acc.cold_account_code_sum.saturating_add(c);
        }
        if let Some(c) = obs.cold_account_nocode_count {
            acc.cold_account_nocode_sum = acc.cold_account_nocode_sum.saturating_add(c);
        }

        // 8038 storage-reprice drivers (F8). Same measured-only fold.
        if let Some(sd) = obs.storage_drivers {
            acc.storage_drivers.add(&sd);
        }

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

        // Per-opcode totals. Sum across every frame; the class's dense
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

        // Per-recipient attribution. Skip `Unchanged` — those txs diverged in
        // neither gas nor outcome, so there's nothing to attribute and it
        // would bloat the rollup with every passing tx's destination.
        if obs.class != AggregateClass::Unchanged {
            let recipient_key = match obs.recipient {
                Some(addr) => format!("{addr:#x}"),
                None => "__create__".to_string(),
            };
            let selector_key = obs.selector.unwrap_or([0u8; 4]);
            let r = acc.recipients.entry((recipient_key, selector_key)).or_default();
            r.tx_count = r.tx_count.saturating_add(1);
            // Only the succeeding-within-original-limit cohort contributes gas;
            // OOG-at-higher-tier txs carry halt-gas deltas (see RecipientAcc).
            if obs.succeeded_within_limit {
                r.gas_delta_sum_succeeding =
                    r.gas_delta_sum_succeeding.saturating_add(obs.gas_delta);
            }
        }
    }

    /// Finalise the block. Builds the `block_coverage` row, one
    /// `block_summaries` row per non-empty class, and bundles the
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
            tx_count_gas_only: 0,
            tx_count_stored: self.tx_count_stored,
            block_gas_used: self.meta.gas_used,
            block_gas_limit: self.meta.gas_limit,
        };
        let mut summaries = Vec::with_capacity(self.classes.len());
        let mut recipients_out = Vec::new();

        for (class, acc) in self.classes {
            match class {
                AggregateClass::Unchanged => coverage.tx_count_unchanged = acc.tx_count,
                AggregateClass::GasOnly => coverage.tx_count_gas_only = acc.tx_count,
            }

            // Only emit the 8037 / multiplier columns when the class
            // actually saw the relevant signal — `None` reads cleanly as
            // "n/a for this class" in the dashboard.
            let state_gas_sum = (acc.state_gas_sum > 0).then_some(acc.state_gas_sum);
            let state_gas_spillover_sum =
                (acc.state_gas_spillover_sum > 0).then_some(acc.state_gas_spillover_sum);
            let multiplier_log2_hist =
                (acc.multiplier_observations > 0).then_some(acc.multiplier_log2_hist);

            // Emit the cold-account split as a pair: if the class saw any
            // cold account access at all, both columns carry a real count
            // (so a genuine zero on one side reads as Some(0), not "n/a").
            // Schedules that don't price the split leave both sums at 0 and
            // both columns read None.
            let cold_split_tracked =
                acc.cold_account_code_sum > 0 || acc.cold_account_nocode_sum > 0;
            let cold_account_code_count = cold_split_tracked.then_some(acc.cold_account_code_sum);
            let cold_account_nocode_count =
                cold_split_tracked.then_some(acc.cold_account_nocode_sum);

            // F8 storage drivers: emit only when the class saw SLOAD/SSTORE
            // activity (else the eight columns read NULL).
            let storage_drivers = acc.storage_drivers.any().then_some(acc.storage_drivers);

            // Collapse the dense 256-wide opcode arrays into a sparse
            // list of `OpcodeBucketTotal`. Skip entries where every
            // counter is zero so the JSON column stays compact.
            let mut opcode_totals = Vec::new();
            for i in 0..256 {
                let count = acc.opcode_counts[i];
                let gas_baseline = acc.opcode_gas_baseline[i];
                let gas_schedule = acc.opcode_gas_schedule[i];
                if count != 0 || gas_baseline != 0 || gas_schedule != 0 {
                    opcode_totals.push(OpcodeBucketTotal {
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
                class,
                tx_count: acc.tx_count,
                gas_delta_sum: Some(acc.gas_delta_sum),
                gas_delta_sum_sq: Some(acc.gas_delta_sum_sq),
                gas_delta_min: acc.gas_delta_min,
                gas_delta_max: acc.gas_delta_max,
                gas_delta_log2_hist: Some(acc.gas_delta_log2_hist),
                opcode_totals,
                state_gas_sum,
                state_gas_spillover_sum,
                multiplier_log2_hist,
                tx_count_creation: Some(acc.tx_count_creation),
                tx_count_authorization: Some(acc.tx_count_authorization),
                tx_count_runtime_state: Some(acc.tx_count_runtime_state),
                tx_count_no_state: Some(acc.tx_count_no_state),
                cold_account_code_count,
                cold_account_nocode_count,
                storage_drivers,
            });

            // Fold this class's per-recipient map into top-K rollup rows.
            // Rank by `tx_count` (gas_delta is a halt-gas artefact for the
            // failed cohort, so it can't drive the ranking); the truncated
            // tail collapses into one `__other__` row that keeps the
            // unattributed count/gas visible rather than silently dropping it.
            if !acc.recipients.is_empty() {
                let mut entries: Vec<((String, [u8; 4]), RecipientAcc)> =
                    acc.recipients.into_iter().collect();
                // Deterministic order so re-touching a block upserts identical
                // rows: tx_count desc, then (recipient, selector) ascending.
                entries.sort_by(|a, b| b.1.tx_count.cmp(&a.1.tx_count).then_with(|| a.0.cmp(&b.0)));

                let mut tail = RecipientAcc::default();
                for (rank, ((recipient, top_selector), racc)) in entries.into_iter().enumerate() {
                    if rank < RECIPIENT_TOP_K {
                        recipients_out.push(RecipientRow {
                            schedule_name: self.meta.schedule_name.clone(),
                            block_number: self.meta.block_number,
                            class,
                            recipient,
                            top_selector,
                            tx_count: racc.tx_count,
                            gas_delta_sum_succeeding: racc.gas_delta_sum_succeeding,
                        });
                    } else {
                        tail.tx_count = tail.tx_count.saturating_add(racc.tx_count);
                        tail.gas_delta_sum_succeeding = tail
                            .gas_delta_sum_succeeding
                            .saturating_add(racc.gas_delta_sum_succeeding);
                    }
                }
                if tail.tx_count > 0 {
                    recipients_out.push(RecipientRow {
                        schedule_name: self.meta.schedule_name.clone(),
                        block_number: self.meta.block_number,
                        class,
                        recipient: "__other__".to_string(),
                        top_selector: [0u8; 4],
                        tx_count: tail.tx_count,
                        gas_delta_sum_succeeding: tail.gas_delta_sum_succeeding,
                    });
                }
            }
        }

        BlockOutput { coverage, summaries, drill_ins: self.drill_ins, recipients: recipients_out }
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
            gas_used: 15_000_000,
            gas_limit: 30_000_000,
        }
    }

    /// Build a minimal aggregate-class observation with everything-zero
    /// defaults.
    fn obs(class: AggregateClass, gas_delta: i64) -> TxObservation {
        TxObservation {
            class,
            store_full_forensics: false,
            gas_delta,
            state_gas_spent: 0,
            state_gas_spillover: 0,
            min_multiplier_to_succeed: None,
            is_creation: false,
            has_authorization: false,
            has_runtime_state: false,
            cold_account_code_count: None,
            cold_account_nocode_count: None,
            storage_drivers: None,
            drill_in_record: None,
            recipient: Some(Address::repeat_byte(0xab)),
            selector: Some([0x12, 0x34, 0x56, 0x78]),
            succeeded_within_limit: true,
        }
    }

    /// Build a stored (per-tx forensic) observation carrying a drill-in record.
    fn obs_stored(gas_delta: i64) -> TxObservation {
        TxObservation {
            store_full_forensics: true,
            drill_in_record: Some(dummy_drill_in()),
            ..obs(AggregateClass::GasOnly, gas_delta)
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
    fn coverage_counts_split_by_class() {
        let mut agg = BlockAggregator::start_block(meta(), 5);
        agg.observe_tx(obs(AggregateClass::Unchanged, 0), &[]);
        agg.observe_tx(obs(AggregateClass::GasOnly, 100), &[]);
        agg.observe_tx(obs(AggregateClass::GasOnly, 5_000), &[]);
        agg.observe_tx(obs_stored(20_000), &[]);
        agg.observe_tx(obs_stored(50_000), &[]);

        let out = agg.finish_block();
        assert_eq!(out.coverage.tx_count, 5);
        assert_eq!(out.coverage.tx_count_unchanged, 1);
        assert_eq!(out.coverage.tx_count_gas_only, 2);
        assert_eq!(out.coverage.tx_count_stored, 2);
        // One summary per touched aggregate class (Unchanged + GasOnly).
        assert_eq!(out.summaries.len(), 2);
        // Stored txs are kept per-tx, not in a class aggregate.
        assert_eq!(out.drill_ins.len(), 2);
    }

    #[test]
    fn stored_txs_collected_and_excluded_from_aggregate() {
        let mut agg = BlockAggregator::start_block(meta(), 3);

        // Aggregate-class tx — drill_in_record is ignored even if passed.
        let mut o = obs(AggregateClass::GasOnly, 100);
        o.drill_in_record = Some(dummy_drill_in());
        agg.observe_tx(o, &[]);
        // Two stored txs — kept per-tx.
        agg.observe_tx(obs_stored(-50), &[]);
        agg.observe_tx(obs_stored(500), &[]);

        let out = agg.finish_block();
        assert_eq!(out.drill_ins.len(), 2, "only stored txs should retain records");
        // The lone GasOnly tx is the only one in a class aggregate.
        let gas_only = out.summaries.iter().find(|s| s.class == AggregateClass::GasOnly).unwrap();
        assert_eq!(gas_only.tx_count, 1);
    }

    #[test]
    fn gas_delta_aggregates_per_class() {
        let mut agg = BlockAggregator::start_block(meta(), 3);
        agg.observe_tx(obs(AggregateClass::GasOnly, 100), &[]);
        agg.observe_tx(obs(AggregateClass::GasOnly, -50), &[]);
        agg.observe_tx(obs(AggregateClass::GasOnly, 200), &[]);

        let out = agg.finish_block();
        let summary = out.summaries.iter().find(|s| s.class == AggregateClass::GasOnly).unwrap();
        assert_eq!(summary.tx_count, 3);
        assert_eq!(summary.gas_delta_sum, Some(100 - 50 + 200));
        assert_eq!(summary.gas_delta_sum_sq, Some(100i64 * 100 + 50i64 * 50 + 200i64 * 200));
        assert_eq!(summary.gas_delta_min, Some(-50));
        assert_eq!(summary.gas_delta_max, Some(200));
    }

    /// Observation with an explicit recipient / success flag for the
    /// recipient-rollup tests. A `None` recipient models a contract creation.
    fn obs_recipient(
        class: AggregateClass,
        gas_delta: i64,
        recipient: Option<Address>,
        succeeded_within_limit: bool,
    ) -> TxObservation {
        TxObservation {
            class,
            store_full_forensics: false,
            gas_delta,
            state_gas_spent: 0,
            state_gas_spillover: 0,
            min_multiplier_to_succeed: None,
            is_creation: recipient.is_none(),
            has_authorization: false,
            has_runtime_state: false,
            cold_account_code_count: None,
            cold_account_nocode_count: None,
            storage_drivers: None,
            drill_in_record: None,
            recipient,
            selector: Some([0xaa, 0xbb, 0xcc, 0xdd]),
            succeeded_within_limit,
        }
    }

    #[test]
    fn coverage_carries_block_gas_fields() {
        let out = BlockAggregator::start_block(meta(), 0).finish_block();
        assert_eq!(out.coverage.block_gas_used, 15_000_000);
        assert_eq!(out.coverage.block_gas_limit, 30_000_000);
    }

    #[test]
    fn recipients_skip_unchanged_and_attribute_others() {
        let mut agg = BlockAggregator::start_block(meta(), 3);
        let a = Address::repeat_byte(0x10);
        // Unchanged tx — diverged in neither gas nor outcome, so it must NOT
        // appear in the attribution rollup.
        agg.observe_tx(obs_recipient(AggregateClass::Unchanged, 0, Some(a), true), &[]);
        agg.observe_tx(obs_recipient(AggregateClass::GasOnly, 100, Some(a), true), &[]);
        agg.observe_tx(obs_recipient(AggregateClass::GasOnly, 200, Some(a), true), &[]);

        let out = agg.finish_block();
        assert!(
            out.recipients.iter().all(|r| r.class != AggregateClass::Unchanged),
            "Unchanged class must not be attributed"
        );
        let row = out
            .recipients
            .iter()
            .find(|r| r.class == AggregateClass::GasOnly)
            .expect("gas_only recipient row");
        assert_eq!(row.recipient, format!("{a:#x}"));
        assert_eq!(row.tx_count, 2);
        assert_eq!(row.gas_delta_sum_succeeding, 300);
    }

    #[test]
    fn recipients_gas_sum_excludes_non_succeeding() {
        let mut agg = BlockAggregator::start_block(meta(), 2);
        let a = Address::repeat_byte(0x20);
        // Succeeding tx: gas counted. OOG-at-tier tx: counted in tx_count only —
        // its halt-gas gas_delta must NOT pollute the sum.
        agg.observe_tx(obs_recipient(AggregateClass::GasOnly, 1_000, Some(a), true), &[]);
        agg.observe_tx(obs_recipient(AggregateClass::GasOnly, 9_999_999, Some(a), false), &[]);

        let out = agg.finish_block();
        let row = out.recipients.iter().find(|r| r.class == AggregateClass::GasOnly).unwrap();
        assert_eq!(row.tx_count, 2, "both txs counted");
        assert_eq!(row.gas_delta_sum_succeeding, 1_000, "halt-gas delta excluded");
    }

    #[test]
    fn recipients_create_uses_sentinel() {
        let mut agg = BlockAggregator::start_block(meta(), 1);
        agg.observe_tx(obs_recipient(AggregateClass::GasOnly, 5, None, true), &[]);
        let out = agg.finish_block();
        let row = out.recipients.iter().find(|r| r.class == AggregateClass::GasOnly).unwrap();
        assert_eq!(row.recipient, "__create__");
    }

    #[test]
    fn recipients_top_k_folds_tail_into_other() {
        let mut agg = BlockAggregator::start_block(meta(), 0);
        // K+3 distinct recipients, one tx each, so 3 fall outside the top-K.
        let n = RECIPIENT_TOP_K + 3;
        for i in 0..n {
            agg.observe_tx(
                obs_recipient(
                    AggregateClass::GasOnly,
                    1,
                    Some(Address::repeat_byte(i as u8)),
                    true,
                ),
                &[],
            );
        }
        let out = agg.finish_block();
        let rows: Vec<_> =
            out.recipients.iter().filter(|r| r.class == AggregateClass::GasOnly).collect();
        assert_eq!(rows.len(), RECIPIENT_TOP_K + 1, "top-K rows plus one __other__");
        let other = rows.iter().find(|r| r.recipient == "__other__").expect("__other__ row");
        assert_eq!(other.tx_count, 3, "the 3 truncated recipients fold into __other__");
    }

    #[test]
    fn state_gas_aggregates_only_emit_when_non_zero() {
        let mut agg = BlockAggregator::start_block(meta(), 2);
        // GasOnly class: state-gas-active tx.
        let mut o = obs(AggregateClass::GasOnly, 0);
        o.state_gas_spent = 5_000;
        o.state_gas_spillover = 1_500;
        agg.observe_tx(o, &[]);
        // Unchanged class: no state gas activity → state_gas_sum should be None.
        agg.observe_tx(obs(AggregateClass::Unchanged, 0), &[]);

        let out = agg.finish_block();
        let gas_only = out.summaries.iter().find(|s| s.class == AggregateClass::GasOnly).unwrap();
        assert_eq!(gas_only.state_gas_sum, Some(5_000));
        assert_eq!(gas_only.state_gas_spillover_sum, Some(1_500));
        let unchanged =
            out.summaries.iter().find(|s| s.class == AggregateClass::Unchanged).unwrap();
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
                class: AggregateClass::GasOnly,
                store_full_forensics: false,
                gas_delta: 0,
                state_gas_spent: 1_000,
                state_gas_spillover: 0,
                min_multiplier_to_succeed: None,
                is_creation: true,
                has_authorization: true,
                has_runtime_state: true,
                cold_account_code_count: None,
                cold_account_nocode_count: None,
                storage_drivers: None,
                drill_in_record: None,
                recipient: None,
                selector: None,
                succeeded_within_limit: false,
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
        agg.observe_tx(obs(AggregateClass::Unchanged, 0), &[]);
        let out = agg.finish_block();
        let summary = &out.summaries[0];
        assert_eq!(summary.tx_count_no_state, Some(1));
        assert_eq!(summary.tx_count_runtime_state, Some(0));
    }

    #[test]
    fn cold_account_split_sums_per_class_and_gates_as_a_pair() {
        let mut agg = BlockAggregator::start_block(meta(), 4);

        // Two GasOnly txs accumulate into one class aggregate.
        let mut a = obs(AggregateClass::GasOnly, 0);
        a.cold_account_code_count = Some(3);
        a.cold_account_nocode_count = Some(2);
        agg.observe_tx(a, &[]);
        let mut b = obs(AggregateClass::GasOnly, 0);
        b.cold_account_code_count = Some(1);
        b.cold_account_nocode_count = Some(4);
        agg.observe_tx(b, &[]);
        // A reject-path tx (unmeasured) in the same class must NOT dilute the
        // sums — `None` is skipped, not folded as a phantom zero.
        let mut unmeasured = obs(AggregateClass::GasOnly, 0);
        unmeasured.cold_account_code_count = None;
        unmeasured.cold_account_nocode_count = None;
        agg.observe_tx(unmeasured, &[]);

        // A class that made no cold account accesses at all.
        agg.observe_tx(obs(AggregateClass::Unchanged, 0), &[]);

        let out = agg.finish_block();
        let by = |class: AggregateClass| out.summaries.iter().find(|s| s.class == class).unwrap();

        let gas_only = by(AggregateClass::GasOnly);
        assert_eq!(gas_only.cold_account_code_count, Some(4));
        assert_eq!(gas_only.cold_account_nocode_count, Some(6));
        // code + nocode == total cold account accesses in the class.
        assert_eq!(
            gas_only.cold_account_code_count.unwrap() + gas_only.cold_account_nocode_count.unwrap(),
            10
        );

        // No cold accesses → both columns read None (n/a), not Some(0).
        let unchanged = by(AggregateClass::Unchanged);
        assert_eq!(unchanged.cold_account_code_count, None);
        assert_eq!(unchanged.cold_account_nocode_count, None);
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
        let mut o = obs(AggregateClass::GasOnly, 0);
        o.min_multiplier_to_succeed = Some(1.0); // bin 1
        agg.observe_tx(o, &[]);
        let mut o = obs(AggregateClass::GasOnly, 0);
        o.min_multiplier_to_succeed = Some(1.4); // bin 3 (1.25 < 1.4 ≤ 1.5)
        agg.observe_tx(o, &[]);
        let mut o = obs(AggregateClass::GasOnly, 0);
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
        DrillInRecord {
            divergence: DivergenceRow {
                schedule_name: "test".to_string(),
                schedule_config_hash: "cfg".to_string(),
                block_number: 42,
                baseline_success: true,
                schedule_success: false,
                status_changed: true,
                ..Default::default()
            },
            call_frames: vec![],
            opcode_counts: vec![],
            baseline_call_frames: vec![],
            baseline_opcode_counts: vec![],
            baseline_event_logs: vec![],
            schedule_event_logs: vec![],
        }
    }

    /// The per-frame opcode counts produced by the inspector should
    /// fold cleanly into the class's aggregate `opcode_totals`,
    /// summed across every frame of every tx in the same class. Zero-
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

        agg.observe_tx(obs(AggregateClass::GasOnly, 100), &[frame_a, frame_b]);

        // tx 2 (same class): one frame with SSTORE (0x55).
        let mut frame_c = FrameOpcodeCounts::new(0);
        frame_c.counts[0x55] = 2;
        frame_c.gas_baseline[0x55] = 40_000;
        frame_c.gas_schedule[0x55] = 50_000;
        agg.observe_tx(obs(AggregateClass::GasOnly, 200), &[frame_c]);

        // tx 3 (different class): should NOT mix into GasOnly.
        let mut frame_d = FrameOpcodeCounts::new(0);
        frame_d.counts[0x20] = 99; // unrelated KECCAK count
        agg.observe_tx(obs(AggregateClass::Unchanged, 0), &[frame_d]);

        let out = agg.finish_block();
        let gas_only = out
            .summaries
            .iter()
            .find(|s| s.class == AggregateClass::GasOnly)
            .expect("GasOnly summary emitted");

        // Sparse list — KECCAK256 (8 = 3+5), SLOAD (1), SSTORE (2). No 0-count rows.
        assert_eq!(gas_only.opcode_totals.len(), 3);
        let by_op: std::collections::BTreeMap<u8, _> = gas_only
            .opcode_totals
            .iter()
            .map(|t| (t.opcode, (t.count, t.gas_baseline, t.gas_schedule)))
            .collect();
        assert_eq!(by_op[&0x20], (8, 240, 360));
        assert_eq!(by_op[&0x54], (1, 800, 800));
        assert_eq!(by_op[&0x55], (2, 40_000, 50_000));

        // Unchanged class's KECCAK total is isolated.
        let unchanged = out
            .summaries
            .iter()
            .find(|s| s.class == AggregateClass::Unchanged)
            .expect("Unchanged summary emitted");
        assert_eq!(unchanged.opcode_totals.len(), 1);
        assert_eq!(unchanged.opcode_totals[0].opcode, 0x20);
        assert_eq!(unchanged.opcode_totals[0].count, 99);
    }
}
