//! Types for representing execution divergences.

use alloy_primitives::{Address, Bytes, B256};
use serde::{Deserialize, Serialize};

/// A detected divergence between normal and experimental execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Divergence {
    /// Block number
    pub block_number: u64,

    /// Transaction index within the block
    pub tx_index: u64,

    /// Transaction hash
    pub tx_hash: B256,

    /// Block timestamp
    pub timestamp: u64,

    /// Types of divergence detected
    pub divergence_types: Vec<DivergenceType>,

    /// Gas analysis
    pub gas_analysis: GasAnalysis,

    /// Operation counts from normal execution
    pub normal_ops: OperationCounts,

    /// Operation counts from experimental execution
    pub experimental_ops: OperationCounts,

    /// Location where divergence first occurred
    pub divergence_location: Option<DivergenceLocation>,

    /// Out-of-gas information (if applicable)
    pub oog_info: Option<OutOfGasInfo>,

    /// Call trees (only if detailed tracing is enabled)
    pub call_trees: Option<CallTrees>,

    /// Event logs (only if detailed tracing is enabled)
    pub event_logs: Option<EventLogs>,
}

/// Type of divergence detected.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceType {
    /// Post-transaction state root differs
    StateRoot,

    /// Execution trace differs (different opcodes executed)
    ExecutionTrace,

    /// Success/revert status differs
    Status,

    /// Event logs differ
    EventLogs,

    /// Call tree differs (different external calls made)
    CallTree,

    /// Gas usage pattern significantly differs (structural difference)
    GasPattern,
}

impl std::fmt::Display for DivergenceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StateRoot => write!(f, "state_root"),
            Self::ExecutionTrace => write!(f, "execution_trace"),
            Self::Status => write!(f, "status"),
            Self::EventLogs => write!(f, "event_logs"),
            Self::CallTree => write!(f, "call_tree"),
            Self::GasPattern => write!(f, "gas_pattern"),
        }
    }
}

/// Gas usage analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasAnalysis {
    /// Gas consumed in normal execution
    pub normal_gas_used: u64,

    /// Gas consumed in experimental execution
    pub experimental_gas_used: u64,

    /// Gas efficiency ratio: (`experimental_gas` / multiplier) / `normal_gas`
    /// Values near 1.0 indicate same execution path, just more expensive
    /// Values != 1.0 indicate different execution path taken
    pub gas_efficiency_ratio: f64,
}

impl GasAnalysis {
    /// Calculate gas efficiency ratio.
    pub fn calculate_ratio(normal_gas: u64, experimental_gas: u64, gas_multiplier: u64) -> f64 {
        if normal_gas == 0 {
            return 1.0;
        }
        let normalized_exp_gas = experimental_gas as f64 / gas_multiplier as f64;
        normalized_exp_gas / normal_gas as f64
    }

    /// Check if the gas pattern indicates a structural divergence.
    /// Threshold of 5% difference
    pub fn is_structural_divergence(&self) -> bool {
        (self.gas_efficiency_ratio - 1.0).abs() > 0.05
    }
}

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

    // ── Repriced opcode counts and cumulative gas deltas ──
    /// Number of DIV (0x04) operations
    #[serde(default)]
    pub div_count: u64,
    /// Cumulative additional gas charged for DIV
    #[serde(default)]
    pub div_gas_delta: i64,

    /// Number of SDIV (0x05) operations
    #[serde(default)]
    pub sdiv_count: u64,
    /// Cumulative additional gas charged for SDIV
    #[serde(default)]
    pub sdiv_gas_delta: i64,

    /// Number of MOD (0x06) operations
    #[serde(default)]
    pub mod_count: u64,
    /// Cumulative additional gas charged for MOD
    #[serde(default)]
    pub mod_gas_delta: i64,

    /// Number of SMOD (0x07) operations
    #[serde(default)]
    pub smod_count: u64,
    /// Cumulative additional gas charged for SMOD
    #[serde(default)]
    pub smod_gas_delta: i64,

    /// Number of ADDMOD (0x08) operations
    #[serde(default)]
    pub addmod_count: u64,
    /// Cumulative additional gas charged for ADDMOD
    #[serde(default)]
    pub addmod_gas_delta: i64,

    /// Number of MULMOD (0x09) operations
    #[serde(default)]
    pub mulmod_count: u64,
    /// Cumulative additional gas charged for MULMOD
    #[serde(default)]
    pub mulmod_gas_delta: i64,

    /// Number of EXP (0x0A) operations
    #[serde(default)]
    pub exp_count: u64,
    /// Cumulative additional gas charged for EXP
    #[serde(default)]
    pub exp_gas_delta: i64,

    /// Number of KECCAK256 (0x20) operations
    #[serde(default)]
    pub keccak256_count: u64,
    /// Cumulative additional gas charged for KECCAK256
    #[serde(default)]
    pub keccak256_gas_delta: i64,

    /// Number of COLD account accesses whose target has code (`code_hash !=
    /// KECCAK_EMPTY`, incl. EIP-7702 delegated) — the EIP-8038 CODE cohort.
    ///
    /// Collected for **every** schedule (not just EIP-8038): the classification
    /// is a near-free native baseline, so 7904/8037 rows carry it too. Only
    /// EIP-8038 *prices* the code/no-code split.
    #[serde(default)]
    pub cold_account_code_count: u64,
    /// Number of COLD account accesses whose target has no code (pure EOA /
    /// empty / non-existent) — the EIP-8038 `NO_CODE` cohort.
    #[serde(default)]
    pub cold_account_nocode_count: u64,

    /// Running sum of the warm-base-correction repricing category (F12). The
    /// four `tax_*` sums reconcile to the tx's `additional_gas_charged`. See
    /// `schedule::GasTaxBreakdown`.
    #[serde(default)]
    pub tax_warm_base: i64,
    /// Running sum of the cold-account-CODE-surcharge category (F12).
    #[serde(default)]
    pub tax_cold_code: i64,
    /// Running sum of the EXTCODE* second-DB-read category (F12).
    #[serde(default)]
    pub tax_second_db_read: i64,
    /// Running sum of the unclassified category — multipliers, CSV deltas (F12).
    #[serde(default)]
    pub tax_other: i64,
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

/// Call trees from both executions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallTrees {
    /// Call tree from normal execution
    pub normal: Vec<CallFrame>,

    /// Call tree from experimental execution
    pub experimental: Vec<CallFrame>,
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

/// Event logs from both executions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventLogs {
    /// Logs from normal execution
    pub normal: Vec<EventLog>,

    /// Logs from experimental execution
    pub experimental: Vec<EventLog>,
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

/// Canonical ERC-4337 `EntryPoint` addresses (v0.6 / v0.7 / v0.8). Preserved as
/// institutional knowledge for the downstream `ClickHouse` classifier: a gas-class
/// failure on a tx whose recipient is one of these is a `UserOp` gas
/// re-estimation problem (the signed off-chain `callGasLimit` /
/// `verificationGasLimit`), not a contract bug. The producer no longer
/// classifies this — it stores the raw `recipient` and lets downstream decide.
pub const ERC4337_ENTRYPOINTS: [Address; 3] = [
    alloy_primitives::address!("5ff137d4b0fdcd49dca30c7cf57e578a026d2789"), // v0.6
    alloy_primitives::address!("0000000071727de22e5e9d8baf0edac6f37da032"), // v0.7
    alloy_primitives::address!("4337084d9e255ff0702461cf8895ce9e3b5ff108"), // v0.8
];

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
    fn test_gas_efficiency_ratio() {
        // Same execution path: experimental gas is exactly multiplier * normal gas
        let ratio = GasAnalysis::calculate_ratio(1000, 128_000, 128);
        assert!((ratio - 1.0).abs() < 0.01);
        assert!(!GasAnalysis {
            normal_gas_used: 1000,
            experimental_gas_used: 128_000,
            gas_efficiency_ratio: ratio,
        }
        .is_structural_divergence());

        // Different execution path: experimental uses less gas (shorter path)
        let ratio = GasAnalysis::calculate_ratio(1000, 100_000, 128);
        assert!(ratio < 0.9);
        assert!(GasAnalysis {
            normal_gas_used: 1000,
            experimental_gas_used: 100_000,
            gas_efficiency_ratio: ratio,
        }
        .is_structural_divergence());

        // Different execution path: experimental uses more gas (longer path)
        let ratio = GasAnalysis::calculate_ratio(1000, 150_000, 128);
        assert!(ratio > 1.1);
        assert!(GasAnalysis {
            normal_gas_used: 1000,
            experimental_gas_used: 150_000,
            gas_efficiency_ratio: ratio,
        }
        .is_structural_divergence());
    }

    #[test]
    fn test_divergence_type_display() {
        assert_eq!(DivergenceType::StateRoot.to_string(), "state_root");
        assert_eq!(DivergenceType::CallTree.to_string(), "call_tree");
    }

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
                                                                // A trace divergence with both succeeding → stored.
        let mut t = facts(true, true, 0);
        t.event_logs_changed = true;
        assert!(t.store_full_forensics());
    }
}
