//! Types for representing execution divergences.

use alloy_primitives::{Address, Bytes, B256};
use serde::{Deserialize, Serialize};

/// Counts of various operations executed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperationCounts {
    /// Number of SLOAD operations
    pub sload_count: u64,

    /// Number of SSTORE operations
    pub sstore_count: u64,

    /// Number of CALL/DELEGATECALL/STATICCALL operations
    pub call_count: u64,

    /// Number of LOG operations
    pub log_count: u64,

    /// Total opcodes executed
    pub total_ops: u64,

    /// Peak memory words allocated
    pub memory_words_allocated: u64,

    /// Number of CREATE/CREATE2 operations
    pub create_count: u64,

    /// Number of COLD account accesses (first access this tx) for the
    /// account-access opcodes (BALANCE / EXTCODE\* / CALL family / SELFDESTRUCT).
    ///
    /// Collected for **every** schedule (not just EIP-8038): the classification
    /// is a near-free native baseline, so 7904/8037 rows carry it too.
    #[serde(default)]
    pub cold_account_access_count: u64,

    /// Number of WARM account accesses (already-touched this tx) for the same
    /// account-access opcodes as [`Self::cold_account_access_count`]. Together
    /// with the cold count this gives the full account-access split that drives
    /// EIP-8038's account-touch repricing. Collected for every schedule. (F2)
    #[serde(default)]
    pub warm_account_access_count: u64,

    /// Number of CALL/CALLCODE (`0xF1` / `0xF2`) operations that transferred a
    /// non-zero value. DELEGATECALL / STATICCALL carry no value and are
    /// excluded. Collected for every schedule. (F2)
    #[serde(default)]
    pub value_transfer_count: u64,

    /// Running sum of the EXTCODE* second-DB-read repricing category (F12). The
    /// `tax_*` sums reconcile to the tx's `additional_gas_charged`. See
    /// `schedule::GasTaxBreakdown`.
    #[serde(default)]
    pub tax_second_db_read: i64,
    /// Running sum of the unclassified category — multipliers, CSV deltas (F12).
    #[serde(default)]
    pub tax_other: i64,

    // EIP-8038 storage-reprice drivers (F8) — read-only per-opcode counts that
    // attribute the native storage surcharges (`COLD_STORAGE_ACCESS`,
    // `STORAGE_WRITE`, `REFUND_STORAGE_CLEAR`), which never touch
    // `additional_gas_charged`. Collected for every schedule.
    /// Cold SLOAD accesses (drive `COLD_STORAGE_ACCESS` on reads).
    #[serde(default)]
    pub sload_cold_count: u64,
    /// Warm SLOAD accesses.
    #[serde(default)]
    pub sload_warm_count: u64,
    /// SSTOREs touching a cold slot (cold first-touch → `COLD_STORAGE_ACCESS`).
    #[serde(default)]
    pub sstore_cold_count: u64,
    /// SSTORE fresh-set transitions (0 → nonzero on a clean slot) → `STORAGE_WRITE`.
    #[serde(default)]
    pub sstore_set_count: u64,
    /// SSTORE reset transitions (nonzero → other nonzero on a clean slot) → `STORAGE_WRITE`.
    #[serde(default)]
    pub sstore_reset_count: u64,
    /// SSTORE clear transitions (nonzero → 0 on a clean slot) → `STORAGE_WRITE` +
    /// `REFUND_STORAGE_CLEAR`.
    #[serde(default)]
    pub sstore_clear_count: u64,
    /// SSTORE no-ops (`current == new`) — warm read cost only.
    #[serde(default)]
    pub sstore_noop_count: u64,
    /// SSTORE re-writes of a slot already changed this tx (`original != current`) — warm rate.
    #[serde(default)]
    pub sstore_dirty_count: u64,
}

/// The EIP-8038 storage-reprice drivers (F8) bundled for the in-memory plumbing
/// (`PerScheduleResult` / `TxObservation` / the per-class aggregate) — the eight
/// counts map 1:1 to the explicit `divergences` / `block_summaries` columns at
/// insert time. See [`OperationCounts`] for the per-field meaning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct StorageDrivers {
    pub sload_cold: u64,
    pub sload_warm: u64,
    pub sstore_cold: u64,
    pub sstore_set: u64,
    pub sstore_reset: u64,
    pub sstore_clear: u64,
    pub sstore_noop: u64,
    pub sstore_dirty: u64,
}

impl StorageDrivers {
    /// Snapshot the storage-driver counts out of an [`OperationCounts`].
    pub const fn from_counts(oc: &OperationCounts) -> Self {
        Self {
            sload_cold: oc.sload_cold_count,
            sload_warm: oc.sload_warm_count,
            sstore_cold: oc.sstore_cold_count,
            sstore_set: oc.sstore_set_count,
            sstore_reset: oc.sstore_reset_count,
            sstore_clear: oc.sstore_clear_count,
            sstore_noop: oc.sstore_noop_count,
            sstore_dirty: oc.sstore_dirty_count,
        }
    }

    /// Saturating element-wise accumulation (per-class aggregate).
    pub const fn add(&mut self, o: &Self) {
        self.sload_cold = self.sload_cold.saturating_add(o.sload_cold);
        self.sload_warm = self.sload_warm.saturating_add(o.sload_warm);
        self.sstore_cold = self.sstore_cold.saturating_add(o.sstore_cold);
        self.sstore_set = self.sstore_set.saturating_add(o.sstore_set);
        self.sstore_reset = self.sstore_reset.saturating_add(o.sstore_reset);
        self.sstore_clear = self.sstore_clear.saturating_add(o.sstore_clear);
        self.sstore_noop = self.sstore_noop.saturating_add(o.sstore_noop);
        self.sstore_dirty = self.sstore_dirty.saturating_add(o.sstore_dirty);
    }

    /// Whether any driver fired — gates emitting the aggregate columns.
    pub const fn any(&self) -> bool {
        self.sload_cold != 0 ||
            self.sload_warm != 0 ||
            self.sstore_cold != 0 ||
            self.sstore_set != 0 ||
            self.sstore_reset != 0 ||
            self.sstore_clear != 0 ||
            self.sstore_noop != 0 ||
            self.sstore_dirty != 0
    }
}

/// Account-side gas-driver counts (F2/F3) bundled for the in-memory plumbing
/// (`PerScheduleResult` / `TxObservation` / the per-class aggregate), mirroring
/// [`StorageDrivers`]: the five counts map 1:1 to explicit `divergences` /
/// `block_summaries` columns at insert time. The first three are runtime
/// inspector counts; the two access-list counts come from the tx envelope (the
/// same value for every replay tier).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub struct AccountDrivers {
    /// Warm account accesses (`OperationCounts::warm_account_access_count`).
    pub warm_account_access: u64,
    /// CALL/CALLCODE with non-zero value (`OperationCounts::value_transfer_count`).
    pub value_transfer: u64,
    /// CREATE + CREATE2 opcodes (`OperationCounts::create_count`).
    pub create_opcode: u64,
    /// EIP-2930 access-list address entries declared by the tx.
    pub access_list_address: u64,
    /// EIP-2930 access-list storage-key entries declared by the tx.
    pub access_list_storage_key: u64,
}

impl AccountDrivers {
    /// Snapshot the inspector-derived counts out of an [`OperationCounts`] and
    /// pair them with the tx-envelope access-list counts.
    pub const fn from_parts(
        oc: &OperationCounts,
        access_list_address: u64,
        access_list_storage_key: u64,
    ) -> Self {
        Self {
            warm_account_access: oc.warm_account_access_count,
            value_transfer: oc.value_transfer_count,
            create_opcode: oc.create_count,
            access_list_address,
            access_list_storage_key,
        }
    }

    /// Saturating element-wise accumulation (per-class aggregate).
    pub const fn add(&mut self, o: &Self) {
        self.warm_account_access = self.warm_account_access.saturating_add(o.warm_account_access);
        self.value_transfer = self.value_transfer.saturating_add(o.value_transfer);
        self.create_opcode = self.create_opcode.saturating_add(o.create_opcode);
        self.access_list_address = self.access_list_address.saturating_add(o.access_list_address);
        self.access_list_storage_key =
            self.access_list_storage_key.saturating_add(o.access_list_storage_key);
    }

    /// Whether any driver fired — gates emitting the aggregate columns.
    pub const fn any(&self) -> bool {
        self.warm_account_access != 0 ||
            self.value_transfer != 0 ||
            self.create_opcode != 0 ||
            self.access_list_address != 0 ||
            self.access_list_storage_key != 0
    }
}

/// F1: 1×-failure forensics for a divergence row.
///
/// When tier-1 (the mainnet-equivalent run) FAILS but the accepted attempt is a
/// bumped tier (rescue) or the highest tier (fail-under-both), the accepted
/// attempt's `oog_*` / frame fields describe that *other* tier — these preserve
/// where/why tier-1 broke. Built from tier-1's own per-schedule result +
/// inspector with NO re-execution; the field types mirror the matching
/// `DivergenceRow` columns so the insert mapping is shared.
#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct Tier1Diagnostics {
    pub failure_reason: Option<String>,
    pub oog_opcode: Option<u8>,
    pub oog_contract: Option<Address>,
    pub oog_pc: Option<u32>,
    pub oog_depth: Option<i32>,
    pub oog_gas_remaining: Option<u64>,
    pub failing_selector: Option<[u8; 4]>,
    pub failing_gas_provided: Option<u64>,
    pub failing_gas_requested: Option<u64>,
}

impl Tier1Diagnostics {
    /// Assemble tier-1 forensics from the structured pieces of tier-1's result:
    /// the failure reason, the captured OOG info, and the innermost failing
    /// call frame (the deepest `!success` frame — the bottleneck).
    pub fn from_parts(
        failure_reason: Option<String>,
        oog: Option<&OutOfGasInfo>,
        failing: Option<&CallFrame>,
    ) -> Self {
        Self {
            failure_reason,
            oog_opcode: oog.map(|o| o.opcode),
            oog_contract: oog.map(|o| o.contract),
            oog_pc: oog.map(|o| o.pc as u32),
            oog_depth: oog.map(|o| o.call_depth as i32),
            oog_gas_remaining: oog.map(|o| o.gas_remaining),
            failing_selector: failing
                .and_then(|f| f.input.as_ref())
                .filter(|b| b.len() >= 4)
                .map(|b| [b[0], b[1], b[2], b[3]]),
            failing_gas_provided: failing.map(|f| f.gas_provided),
            failing_gas_requested: failing.and_then(|f| f.gas_requested_on_stack),
        }
    }
}

/// Location where divergence first occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivergenceLocation {
    /// Contract address where divergence occurred
    pub contract: Address,

    /// Function selector call stack (first 4 bytes of calldata for each call in the stack)
    /// Ordered from root (index 0) to deepest call (last index)
    /// None entries represent calls without selectors (e.g., value transfers, creates)
    pub function_selectors: Vec<Option<[u8; 4]>>,

    /// Program counter at divergence
    pub pc: usize,

    /// Call depth when divergence occurred
    pub call_depth: usize,

    /// Opcode at divergence point
    pub opcode: u8,

    /// Opcode name (for readability)
    pub opcode_name: String,
}

/// Information about out-of-gas occurrence in experimental execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutOfGasInfo {
    /// Opcode that caused OOG
    pub opcode: u8,

    /// Opcode name
    pub opcode_name: String,

    /// Program counter where OOG occurred
    pub pc: usize,

    /// Contract address
    pub contract: Address,

    /// Call depth
    pub call_depth: usize,

    /// Gas remaining when OOG occurred
    pub gas_remaining: u64,

    /// Detected pattern (heuristic)
    pub pattern: OogPattern,

    /// Cumulative repricing surcharge (`additional_gas_charged`) the schedule
    /// had applied at the instant OOG was first recorded (F13) — the absolute
    /// gas deficit the repricing introduced up to the failure. `#[serde(default)]`
    /// so older serialized infos deserialize to 0.
    #[serde(default)]
    pub additional_gas_at_oog: i64,
}

impl OutOfGasInfo {
    /// Synthesize a 1-based root-frame OOG sentinel (no real opcode captured):
    /// `pattern = Unknown`, `call_depth = 1`, zeroed pc / contract / gas /
    /// surcharge. `opcode_name` distinguishes the synthesis site (e.g.
    /// `"root_halt"`, `"evm_reject_intrinsic"`). Used when a tx halts/rejects
    /// before the inspector captured a per-frame OOG, so downstream
    /// classification still sees a gas-class root halt.
    pub fn synthetic_root(opcode_name: impl Into<String>) -> Self {
        Self {
            opcode: 0,
            opcode_name: opcode_name.into(),
            pc: 0,
            contract: Address::ZERO,
            call_depth: 1,
            gas_remaining: 0,
            pattern: OogPattern::Unknown,
            additional_gas_at_oog: 0,
        }
    }
}

/// Pattern that caused out-of-gas.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OogPattern {
    /// Loop iteration
    Loop,

    /// Memory expansion
    MemoryExpansion,

    /// Chain of external calls
    CallChain,

    /// Heavy storage operations
    StorageHeavy,

    /// Unknown/other pattern
    Unknown,
}

impl std::fmt::Display for OogPattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loop => write!(f, "loop"),
            Self::MemoryExpansion => write!(f, "memory_expansion"),
            Self::CallChain => write!(f, "call_chain"),
            Self::StorageHeavy => write!(f, "storage_heavy"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// A single call frame in the call tree.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallFrame {
    /// Index in the call tree
    pub call_index: usize,

    /// Call depth
    pub depth: usize,

    /// Caller address
    pub from: Address,

    /// Callee address (None for CREATE)
    pub to: Option<Address>,

    /// Type of call
    pub call_type: CallType,

    /// Gas provided to the call
    pub gas_provided: u64,

    /// Gas used by the call
    pub gas_used: u64,

    /// Whether the call succeeded
    pub success: bool,

    /// Input data
    pub input: Option<Bytes>,

    /// Output/return data
    pub output: Option<Bytes>,

    /// Cumulative repricing gas delta applied within this call frame.
    /// Positive means the schedule made this frame's opcodes more expensive.
    #[serde(default)]
    pub repricing_gas_delta: i64,

    /// Gas argument the caller pushed onto the stack at the CALL opcode (top of
    /// stack at the moment of CALL/CALLCODE/DELEGATECALL/STATICCALL execution).
    ///
    /// `None` for CREATE/CREATE2 (which don't take gas as a stack argument), the
    /// root frame, and frames built by the baseline `TrackingInspector` (which
    /// doesn't capture stack values).
    ///
    /// Used together with `parent_gas_at_call` to detect whether the parent
    /// passed the EIP-150 63/64 cap (proportional forwarding) or a smaller
    /// hardcoded amount (`.transfer()` 2300 stipend, fixed constants, or
    /// fractional patterns like `gas() / 2`).
    #[serde(default)]
    pub gas_requested_on_stack: Option<u64>,

    /// Gas remaining in the parent frame at the moment the CALL opcode
    /// executed (before the EIP-150 cap was applied to derive the child's
    /// `gas_provided`).
    ///
    /// `None` for the root frame and for baseline `TrackingInspector` frames.
    #[serde(default)]
    pub parent_gas_at_call: Option<u64>,

    /// Value transferred (in wei) when the frame was opened. `None` for
    /// DELEGATECALL / STATICCALL (which can't transfer value), CREATE
    /// frames where the value is part of `init_code`, and frames from the
    /// baseline `TrackingInspector`. Carried as the lower 128 bits of a
    /// U256 — values that exceed `u128::MAX` are saturated; this stays
    /// well above any realistic ETH amount.
    #[serde(default)]
    pub value_wei: Option<u128>,

    /// Program counter of the CALL/CREATE opcode in the caller's bytecode that
    /// opened this frame (F9). `None` for the root frame and baseline frames.
    /// Excluded from the structural call-tree comparison.
    #[serde(default)]
    pub caller_pc: Option<usize>,

    /// Whether this frame was served by a precompile (F9). Captured only on the
    /// schedule side; baseline frames are `false`.
    #[serde(default)]
    pub was_precompile: bool,

    /// Precompile address when `was_precompile` is set (F9), else `None`.
    #[serde(default)]
    pub precompile_address: Option<Address>,

    /// Gas remaining in this frame at the moment it failed (F9). `Some` only on
    /// the schedule side for a `!success` frame; `None` otherwise. Excluded from
    /// the structural call-tree comparison.
    #[serde(default)]
    pub gas_remaining_at_fail: Option<u64>,

    /// Storage/call target (revm `target_address`) when it differs from `to`
    /// (which carries the code holder / `bytecode_address`) — i.e. the proxy
    /// under a DELEGATECALL (F3). `None` for CREATE and when uncaptured; equals
    /// `to` for ordinary calls. Excluded from the structural call-tree
    /// comparison.
    #[serde(default)]
    pub storage_target: Option<Address>,
}

/// Type of call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CallType {
    /// Regular call
    Call,
    /// Delegate call
    DelegateCall,
    /// Static call
    StaticCall,
    /// Call code
    CallCode,
    /// Contract creation
    Create,
    /// Contract creation with CREATE2
    Create2,
}

impl std::fmt::Display for CallType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Call => write!(f, "CALL"),
            Self::DelegateCall => write!(f, "DELEGATECALL"),
            Self::StaticCall => write!(f, "STATICCALL"),
            Self::CallCode => write!(f, "CALLCODE"),
            Self::Create => write!(f, "CREATE"),
            Self::Create2 => write!(f, "CREATE2"),
        }
    }
}

/// A single event log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventLog {
    /// Log index
    pub log_index: usize,

    /// Contract address that emitted the log
    pub address: Address,

    /// Topics (indexed parameters)
    pub topics: Vec<B256>,

    /// Data (non-indexed parameters)
    pub data: Bytes,
}

/// Maximum number of frames whose opcode counts are retained per
/// transaction. Beyond this, the inspector flips `PerFrameCapture::truncated`
/// to `true` and discards further frames' counts (execution continues
/// normally — only the bookkeeping is dropped).
///
/// 1024 frames × ~6 KB per frame = ~6 MB heap upper bound per in-flight tx.
/// A typical tx has < 20 frames; this only kicks in for pathological
/// recursive or fan-out patterns.
pub const MAX_TRACKED_FRAMES: u32 = 1024;

/// Per-frame opcode counters captured by the inspector.
///
/// Counts and gas are tracked per-opcode (indexed by opcode byte) within a
/// single call frame. The `call_index` identifies the frame in
/// frame-open order, with the root frame at index 0.
///
/// Arrays are boxed so a `Vec<FrameOpcodeCounts>` keeps a small inline
/// footprint; the 6 KB of opcode tables sits on the heap.
///
/// `gas_baseline` is the natural EVM gas cost the opcode would have charged
/// without any schedule adjustment. `gas_schedule` is the cost actually
/// charged under the schedule (`gas_baseline + per-opcode delta`). For the
/// baseline `TrackingInspector` the two columns are equal.
#[derive(Debug, Clone)]
pub struct FrameOpcodeCounts {
    /// Frame-open index in this transaction. Root = 0.
    pub call_index: u32,
    /// `counts[opcode]` — number of times the opcode executed in this frame.
    pub counts: Box<[u64; 256]>,
    /// `gas_baseline[opcode]` — total natural EVM gas charged by this opcode
    /// in this frame.
    pub gas_baseline: Box<[u64; 256]>,
    /// `gas_schedule[opcode]` — total gas the schedule charged for this
    /// opcode in this frame (natural + per-opcode delta).
    pub gas_schedule: Box<[u64; 256]>,
}

impl FrameOpcodeCounts {
    /// Allocate empty counters for a new frame at `call_index`.
    pub fn new(call_index: u32) -> Self {
        Self {
            call_index,
            counts: Box::new([0u64; 256]),
            gas_baseline: Box::new([0u64; 256]),
            gas_schedule: Box::new([0u64; 256]),
        }
    }

    /// Iterator yielding `(opcode, count, gas_baseline, gas_schedule)` for
    /// every opcode the frame actually used. Skips opcodes with `count == 0`
    /// so the caller can serialize a sparse representation directly.
    pub fn nonzero(&self) -> impl Iterator<Item = (u8, u64, u64, u64)> + '_ {
        (0u16..=255u16).filter_map(move |op| {
            let i = op as usize;
            if self.counts[i] == 0 {
                None
            } else {
                Some((op as u8, self.counts[i], self.gas_baseline[i], self.gas_schedule[i]))
            }
        })
    }
}

/// Container for the inspector's per-frame opcode capture across a single
/// transaction.
///
/// `frames` is in frame-open order: `frames[0]` is the root frame, the
/// next entries are sub-calls in the order they were dispatched.
/// `truncated` flips to `true` if more than [`MAX_TRACKED_FRAMES`] frames
/// were opened in the tx; later frames' counts are silently dropped.
#[derive(Debug, Clone, Default)]
pub struct PerFrameCapture {
    /// Per-frame opcode counts, indexed by frame-open order.
    pub frames: Vec<FrameOpcodeCounts>,
    /// Set to `true` once a tx opens more than [`MAX_TRACKED_FRAMES`] frames.
    pub truncated: bool,
}

impl PerFrameCapture {
    /// Empty capture (zero frames, not truncated).
    pub fn new() -> Self {
        Self::default()
    }
}

/// Execution-fact class for a tx that did NOT meet the per-tx forensic-storage
/// criterion ([`DivergenceFacts::store_full_forensics`]) — both baseline and
/// schedule succeeded with identical traces; only gas may differ. These are the
/// sole txs rolled into per-block aggregates; every failure / rescue / trace
/// divergence gets a full per-tx record instead. The former editorial bucket
/// taxonomy (wallet-fixable / contract-broken / aa-reestimation / …) is
/// re-derived downstream from the stored raw facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateClass {
    /// `gas_delta == 0` and no trace change — byte-identical to baseline.
    Unchanged,
    /// `gas_delta != 0` with no other difference — the silent-majority repricing
    /// tax (the dominant cohort whose opcode-level distribution still matters).
    GasOnly,
}

impl AggregateClass {
    /// Stable lowercase identifier for storage and the dashboard layer.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::GasOnly => "gas_only",
        }
    }
}

impl std::fmt::Display for AggregateClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Raw execution facts a tx exposes after the baseline-vs-schedule comparison,
/// used to decide whether to store a full per-tx forensic record and, if not,
/// which aggregate class it rolls into. No editorial judgment — pure facts.
#[derive(Debug, Clone, Copy)]
pub struct DivergenceFacts {
    /// Baseline (native) execution succeeded.
    pub baseline_success: bool,
    /// Schedule replay succeeded at the original gas limit (tier 1).
    pub schedule_success: bool,
    /// `schedule_gas_used - baseline_gas_used`. Zero means no gas difference.
    pub gas_delta: i64,
    /// Schedule and baseline emitted different event logs.
    pub event_logs_changed: bool,
    /// Schedule and baseline produced structurally different call trees.
    pub call_tree_changed: bool,
    /// Schedule and baseline returned different bytes / lengths.
    pub output_changed: bool,
    /// CREATE/CREATE2 produced different addresses (or one created where the
    /// other didn't).
    pub created_address_changed: bool,
    /// Different log-bloom (caught even when the per-log diff didn't, e.g.
    /// when the inspector skipped detailed log capture).
    pub logs_bloom_changed: bool,
}

impl DivergenceFacts {
    /// Any baseline-vs-schedule trace flag differs (logs / output / created
    /// address / bloom / call tree).
    pub const fn trace_diverged(&self) -> bool {
        self.event_logs_changed ||
            self.output_changed ||
            self.logs_bloom_changed ||
            self.call_tree_changed ||
            self.created_address_changed
    }

    /// Store a full per-tx forensic record iff anything beyond a pure gas change
    /// happened: either execution failed (break, rescue, or fail-under-both) or a
    /// trace diverged. The complement — both succeeded with identical traces — is
    /// the aggregate-only `{unchanged, gas_only}` set. The former editorial
    /// drill-in buckets (wallet-fixable / contract-broken / aa-reestimation / …)
    /// are all subsumed here and re-derived downstream from the stored facts.
    pub const fn store_full_forensics(&self) -> bool {
        !self.schedule_success || !self.baseline_success || self.trace_diverged()
    }

    /// Aggregate class for a non-stored tx (`!store_full_forensics`): `GasOnly`
    /// when the schedule charged different gas, else `Unchanged`.
    pub const fn aggregate_class(&self) -> AggregateClass {
        if self.gas_delta == 0 {
            AggregateClass::Unchanged
        } else {
            AggregateClass::GasOnly
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_operation_counts_default() {
        let ops = OperationCounts::default();
        assert_eq!(ops.total_ops, 0);
        assert_eq!(ops.sload_count, 0);
    }

    fn facts(baseline_ok: bool, schedule_ok: bool, gas_delta: i64) -> DivergenceFacts {
        DivergenceFacts {
            baseline_success: baseline_ok,
            schedule_success: schedule_ok,
            gas_delta,
            event_logs_changed: false,
            call_tree_changed: false,
            output_changed: false,
            created_address_changed: false,
            logs_bloom_changed: false,
        }
    }

    #[test]
    fn store_criterion_and_aggregate_class() {
        // Both succeeded, identical trace → aggregate-only, classed by gas.
        assert!(!facts(true, true, 0).store_full_forensics());
        assert_eq!(facts(true, true, 0).aggregate_class(), AggregateClass::Unchanged);
        assert!(!facts(true, true, 5).store_full_forensics());
        assert_eq!(facts(true, true, 5).aggregate_class(), AggregateClass::GasOnly);
        // Break, rescue, fail-under-both → stored.
        assert!(facts(true, false, 0).store_full_forensics()); // schedule broke it
        assert!(facts(false, true, 0).store_full_forensics()); // schedule rescued it
        assert!(facts(false, false, 0).store_full_forensics()); // failed under both
                                                                // A trace divergence with both
                                                                // succeeding → stored.
        let mut t = facts(true, true, 0);
        t.event_logs_changed = true;
        assert!(t.store_full_forensics());
    }
}
