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

/// Per-tx classification used to decide whether the divergence is written
/// per-tx (drill-in bucket) or rolled into per-block aggregates.
///
/// See `crates/research/docs/storage-redesign.md` for the bucket semantics.
/// The classifier in [`classify_bucket`] applies a fixed priority order:
/// outcome flips toward failure (`baseline_to_schedule_break`) fall into
/// one of the wallet-fixable variants, `InconclusiveNeedsHigherSweep`, or
/// `ContractBroken`; outcome flips the other way
/// (`baseline_to_schedule_rescue`) are surfaced as `ScheduleRescued` (an
/// interesting finding, not a failure); non-flipped divergences are
/// classified by the next available signal (event logs, gas, other-trace) or
/// `Unchanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    /// Outcome + traces match. Counted in `block_coverage.tx_count` only.
    Unchanged,
    /// Trace differs (call tree / output / created address / logs bloom) but
    /// the outcome is identical and gas hasn't changed.
    TraceOnly,
    /// `gas_delta != 0` with no other differences.
    GasOnly,
    /// Emitted event logs differ but the outcome (success/revert) is the
    /// same. Drill-in bucket — full per-tx record retained.
    EventLogsChanged,
    /// Outcome flipped from baseline-failed to schedule-succeeded.
    /// The schedule rescued a tx the baseline couldn't run. Not a failure
    /// — surfaced separately so it doesn't pollute the contract-broken
    /// cohort. Aggregate-only: no per-tx record retained.
    ScheduleRescued,
    /// Outcome flipped from baseline-succeeded to schedule-failed, and the
    /// OOG-class halt was attributed to the root frame (1-based depth = 1).
    /// Wallet-fixable: raising the outer gas limit (or the wallet's gas
    /// estimate) resolves it. Aggregate-only.
    WalletFixableShallow,
    /// Outcome flipped toward failure *and* the OOG-chain walk confirmed
    /// every parent→child hop got proportional gas via the EIP-150 63/64
    /// rule. Wallet-fixable even though the tx involves subcalls.
    /// Aggregate-only.
    WalletFixableDeepChain,
    /// Outcome flipped toward failure, the highest configured gas-limit
    /// multiplier still halted OOG, and no fixed/fractional/stipend
    /// bottleneck was proven. Drill-in bucket — rerun with a higher sweep
    /// ceiling before deciding wallet-fixable vs contract-broken.
    InconclusiveNeedsHigherSweep,
    /// Outcome flipped from baseline-succeeded to schedule-failed and
    /// neither wallet-fixable rule applies. Drill-in bucket — full per-tx
    /// record retained.
    ContractBroken,
    /// Outcome flipped toward failure on a transaction sent to an ERC-4337
    /// `EntryPoint`, via an OOG-class halt. The chain-walk sees the
    /// `EntryPoint` forward a *fixed* `callGasLimit`/`verificationGasLimit` to
    /// the account and would otherwise call it a `FixedGas` contract bottleneck
    /// — but those limits are signed `UserOp` fields the bundler computes
    /// off-chain, so the real fix is re-estimating them and re-signing, not a
    /// contract change. Distinct from `WalletFixable*` because raising the
    /// *outer* tx gas doesn't help (the `EntryPoint` still forwards the fixed
    /// inner budget). Drill-in bucket — full per-tx record retained.
    AaGasReestimation,
}

impl Bucket {
    /// Stable lowercase identifier for storage and the dashboard layer.
    /// Order kept identical to the consumer-side doc's enum.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::TraceOnly => "trace_only",
            Self::GasOnly => "gas_only",
            Self::EventLogsChanged => "event_logs_changed",
            Self::ScheduleRescued => "schedule_rescued",
            Self::WalletFixableShallow => "wallet_fixable_shallow",
            Self::WalletFixableDeepChain => "wallet_fixable_deep_chain",
            Self::InconclusiveNeedsHigherSweep => "inconclusive_needs_higher_sweep",
            Self::ContractBroken => "contract_broken",
            Self::AaGasReestimation => "aa_gas_reestimation",
        }
    }

    /// Whether divergences in this bucket are written per-tx (drill-in) or
    /// rolled into per-block aggregates.
    pub const fn is_drill_in(&self) -> bool {
        matches!(
            self,
            Self::EventLogsChanged |
                Self::InconclusiveNeedsHigherSweep |
                Self::ContractBroken |
                Self::AaGasReestimation
        )
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Canonical ERC-4337 `EntryPoint` addresses (v0.6 / v0.7 / v0.8). A tx whose
/// top-level recipient is one of these is an account-abstraction bundle: its
/// per-`UserOp` gas budgets (`callGasLimit`, `verificationGasLimit`, …) are
/// signed off-chain fields, so a gas-class failure is a re-estimation problem,
/// not a contract bug. See [`Bucket::AaGasReestimation`].
pub const ERC4337_ENTRYPOINTS: [Address; 3] = [
    alloy_primitives::address!("5ff137d4b0fdcd49dca30c7cf57e578a026d2789"), // v0.6
    alloy_primitives::address!("0000000071727de22e5e9d8baf0edac6f37da032"), // v0.7
    alloy_primitives::address!("4337084d9e255ff0702461cf8895ce9e3b5ff108"), // v0.8
];

/// Whether `addr` is a known ERC-4337 `EntryPoint`.
pub fn is_erc4337_entrypoint(addr: Address) -> bool {
    ERC4337_ENTRYPOINTS.contains(&addr)
}

/// Inputs to [`classify_bucket`].
///
/// Bundled so the classifier signature stays readable at call sites and so
/// future fields (e.g. EOF-era markers) can be threaded through without
/// touching every caller.
#[derive(Debug, Clone, Copy)]
pub struct BucketInput {
    /// The transaction's top-level recipient is a known ERC-4337 `EntryPoint`.
    /// Gates [`Bucket::AaGasReestimation`]: an OOG-class break on such a tx is
    /// a `UserOp` gas-limit shortfall (re-estimate + re-sign), not a contract
    /// bug, even though the chain-walk sees a `FixedGas` forward.
    pub recipient_is_entrypoint: bool,
    /// Outcome flipped from baseline-succeeded to schedule-failed.
    /// I.e. `normal_success && !schedule_success`. This is the "schedule
    /// broke this tx" direction — the gate for `WalletFixableShallow`,
    /// `WalletFixableDeepChain`, `InconclusiveNeedsHigherSweep`, and
    /// `ContractBroken`.
    pub baseline_to_schedule_break: bool,
    /// Outcome flipped from baseline-failed to schedule-succeeded.
    /// I.e. `!normal_success && schedule_success`. The schedule rescued
    /// a tx the baseline couldn't run — interesting but not a failure,
    /// surfaced as `Bucket::ScheduleRescued`.
    ///
    /// Mutually exclusive with `baseline_to_schedule_break`; both can be
    /// false (no status flip).
    pub baseline_to_schedule_rescue: bool,
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
    /// The schedule replay succeeded with a larger outer gas limit, but the
    /// resulting `schedule_gas_used` exceeded the transaction's original gas
    /// limit while still matching baseline observable behavior. This is a
    /// direct witness that the original-limit failure is wallet-fixable even
    /// when the stored successful replay no longer has an OOG site to classify.
    pub outer_limit_only_failure: bool,
    /// Outcome of the schedule replay's final attempted gas-limit multiplier
    /// tier. `Some(true)` means the highest configured tier still halted OOG,
    /// so the tx needs a higher sweep before wallet-fixable can be proven.
    /// `Some(false)` means the highest tier failed for a non-OOG reason, and
    /// `None` means at least one tier succeeded.
    pub replay_halt_oog: Option<bool>,
    /// 1-based depth at which the schedule's OOG-class halt was attributed,
    /// from `OutOfGasInfo.call_depth`. `None` when the schedule didn't OOG
    /// (e.g. plain revert) and the producer's Err-branch synthesis didn't
    /// fire. Used by the shallow-OOG heuristic to short-circuit root-frame
    /// failures (including EVM-rejection at the intrinsic-gas check, which
    /// the producer synthesizes as a `call_depth = 1` halt).
    pub oog_call_depth: Option<usize>,
    /// Result of the OOG-chain classifier (`oog_chain::classify_oog_chain`):
    /// `Some(true)` if every hop from root to OOG was proportional;
    /// `Some(false)` if some hop throttled; `None` when the divergence isn't
    /// an OOG or call frames were missing.
    pub oog_chain_proportional: Option<bool>,
}

/// Classify a per-tx divergence into a [`Bucket`] using the priority ladder
/// from `crates/research/docs/storage-redesign.md`:
///
/// 1. `baseline_to_schedule_break` (schedule made the tx fail) →
///    - `WalletFixableShallow` if a successful higher-gas replay proves the original tx failed only
///      because the outer limit was too low;
///    - `ContractBroken` if the OOG-chain classifier found a fixed/fractional/stipend bottleneck;
///    - `InconclusiveNeedsHigherSweep` if the highest configured replay tier still halted OOG and
///      no bottleneck was proven;
///    - `WalletFixableShallow` if the OOG was in the root frame (`oog_call_depth <= 1`, 1-based),
///      which includes EVM-rejection at the intrinsic-gas check (synthesized as `call_depth = 1` by
///      the producer);
///    - else `WalletFixableDeepChain` if the OOG-chain classifier confirmed every hop was
///      proportional;
///    - else `ContractBroken`.
/// 2. `baseline_to_schedule_rescue` (schedule made the tx succeed) → `ScheduleRescued`.
/// 3. `event_logs_changed` → `EventLogsChanged`.
/// 4. `gas_delta != 0` → `GasOnly`.
/// 5. Any other trace flag set → `TraceOnly`.
/// 6. Otherwise → `Unchanged`.
///
/// The break-direction outranks the rescue-direction: only one of those
/// flags can be true (they're mutually exclusive), but a status-flipped tx
/// whose logs also differ is one of the break buckets (wallet-fixable,
/// inconclusive, or contract-broken), not `EventLogsChanged`, because the
/// outcome flip is the actionable signal. Similarly, a non-flipped tx with
/// both event-log and gas differences is classified as `EventLogsChanged`
/// — log changes are harder to dismiss than a gas delta alone.
pub fn classify_bucket(input: &BucketInput) -> Bucket {
    if input.baseline_to_schedule_break {
        if input.outer_limit_only_failure {
            Bucket::WalletFixableShallow
        } else if input.recipient_is_entrypoint && is_oog_class(input) {
            // ERC-4337 EntryPoint bundle that OOG'd: the EntryPoint meters
            // each UserOp via signed gas limits, so the chain-walk's
            // `FixedGas` verdict is misleading — the fix is off-chain
            // re-estimation, not a contract change. Takes priority over the
            // ContractBroken / InconclusiveNeedsHigherSweep branches below,
            // which is where these otherwise (mis)landed.
            Bucket::AaGasReestimation
        } else if input.oog_chain_proportional == Some(false) {
            Bucket::ContractBroken
        } else if input.replay_halt_oog == Some(true) {
            Bucket::InconclusiveNeedsHigherSweep
        } else if is_shallow_oog(input.oog_call_depth) {
            Bucket::WalletFixableShallow
        } else if input.oog_chain_proportional == Some(true) {
            Bucket::WalletFixableDeepChain
        } else {
            Bucket::ContractBroken
        }
    } else if input.baseline_to_schedule_rescue {
        Bucket::ScheduleRescued
    } else if input.event_logs_changed {
        Bucket::EventLogsChanged
    } else if input.gas_delta != 0 {
        Bucket::GasOnly
    } else if input.call_tree_changed ||
        input.output_changed ||
        input.created_address_changed ||
        input.logs_bloom_changed
    {
        Bucket::TraceOnly
    } else {
        Bucket::Unchanged
    }
}

/// Whether the schedule break was gas-class (an OOG site was recorded, or the
/// highest replay tier still halted OOG). Distinguishes `EntryPoint` OOGs —
/// which are AA gas re-estimation — from `EntryPoint` reverts for non-gas
/// reasons (e.g. a paymaster rejecting), which are not.
const fn is_oog_class(input: &BucketInput) -> bool {
    input.oog_call_depth.is_some() || matches!(input.replay_halt_oog, Some(true))
}

/// Shallow heuristic: the OOG-class halt was attributed to the root frame
/// (1-based depth = 1).
///
/// Captures three real-world cases:
/// - Simple tx (no sub-calls) whose root frame ran out of gas — the canonical "wallet just needs
///   more gas" failure.
/// - Tx that made some sub-calls which returned cleanly, then OOG'd back in the root frame on a
///   later opcode (the old `call_count == 0` guard wrongly excluded this).
/// - EVM-rejection at the intrinsic-gas check: the producer synthesizes `OutOfGasInfo { call_depth:
///   1, .. }` so these EIP-8037 intrinsic blow-ups classify as wallet-fixable rather than
///   contract-broken.
const fn is_shallow_oog(oog_call_depth: Option<usize>) -> bool {
    matches!(oog_call_depth, Some(d) if d <= 1)
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

    /// Build a `BucketInput` with every flag in its "no divergence" position.
    /// Tests override only the fields they care about.
    fn neutral_input() -> BucketInput {
        BucketInput {
            recipient_is_entrypoint: false,
            baseline_to_schedule_break: false,
            baseline_to_schedule_rescue: false,
            gas_delta: 0,
            event_logs_changed: false,
            call_tree_changed: false,
            output_changed: false,
            created_address_changed: false,
            logs_bloom_changed: false,
            outer_limit_only_failure: false,
            replay_halt_oog: None,
            oog_call_depth: None,
            oog_chain_proportional: None,
        }
    }

    #[test]
    fn entrypoint_oog_break_is_aa_gas_reestimation() {
        // Same shape that lands a non-EntryPoint tx in ContractBroken
        // (FixedGas bottleneck), but to an EntryPoint → AA re-estimation.
        let base = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(3),
            oog_chain_proportional: Some(false),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&base), Bucket::ContractBroken);
        let aa = BucketInput { recipient_is_entrypoint: true, ..base };
        assert_eq!(classify_bucket(&aa), Bucket::AaGasReestimation);
    }

    #[test]
    fn entrypoint_non_oog_revert_is_not_aa_gas_reestimation() {
        // EntryPoint break with no OOG signal (e.g. paymaster revert) must
        // not be swept into AA re-estimation — it's not a gas-limit fix.
        let input = BucketInput {
            recipient_is_entrypoint: true,
            baseline_to_schedule_break: true,
            oog_call_depth: None,
            replay_halt_oog: Some(false),
            oog_chain_proportional: None,
            ..neutral_input()
        };
        assert_ne!(classify_bucket(&input), Bucket::AaGasReestimation);
    }

    #[test]
    fn erc4337_entrypoint_recognized() {
        assert!(is_erc4337_entrypoint(alloy_primitives::address!(
            "0000000071727de22e5e9d8baf0edac6f37da032"
        )));
        assert!(!is_erc4337_entrypoint(Address::ZERO));
    }

    #[test]
    fn bucket_unchanged_when_nothing_diverges() {
        assert_eq!(classify_bucket(&neutral_input()), Bucket::Unchanged);
    }

    #[test]
    fn bucket_trace_only_for_call_tree_diff_without_gas() {
        let input = BucketInput { call_tree_changed: true, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::TraceOnly);
    }

    #[test]
    fn bucket_trace_only_for_output_or_address_or_bloom_diff() {
        for input in [
            BucketInput { output_changed: true, ..neutral_input() },
            BucketInput { created_address_changed: true, ..neutral_input() },
            BucketInput { logs_bloom_changed: true, ..neutral_input() },
        ] {
            assert_eq!(classify_bucket(&input), Bucket::TraceOnly);
        }
    }

    #[test]
    fn bucket_gas_only_outranks_trace_only() {
        // Both gas and a trace flag set; gas wins (TraceOnly is reserved for
        // tx differences that have neither a status flip nor a gas delta).
        let input = BucketInput { gas_delta: 1234, call_tree_changed: true, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::GasOnly);
    }

    #[test]
    fn bucket_gas_only_for_pure_gas_delta() {
        let input = BucketInput { gas_delta: -42, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::GasOnly);
    }

    #[test]
    fn bucket_event_logs_changed_outranks_gas_only() {
        let input = BucketInput { event_logs_changed: true, gas_delta: 999, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::EventLogsChanged);
    }

    #[test]
    fn bucket_event_logs_changed_for_log_diff_only() {
        let input = BucketInput { event_logs_changed: true, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::EventLogsChanged);
    }

    #[test]
    fn bucket_break_outranks_event_logs() {
        // A status flip with diverging logs is still a wallet-fixable /
        // contract-broken case; the outcome flip is the actionable signal.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            event_logs_changed: true,
            // Root-frame OOG so the variant is deterministic without
            // depending on oog_chain_proportional.
            oog_call_depth: Some(1),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_for_root_frame_oog() {
        // OOG attributed to the root frame (1-based depth = 1) is the
        // canonical wallet-fixable case.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(1),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_even_when_tx_had_subcalls() {
        // Tx made internal calls that returned cleanly, then OOG'd at the
        // root frame on a later opcode. The old `call_count == 0` guard
        // wrongly excluded this; the new predicate accepts it because the
        // OOG itself is at depth 1.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(1),
            oog_chain_proportional: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_for_synthesized_evm_reject() {
        // EVM-rejection at the intrinsic-gas check: producer synthesizes
        // `OutOfGasInfo { call_depth: 1, .. }` so these classify the same
        // as a natural root-frame OOG.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(1),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_for_outer_limit_only_failure() {
        // A tier-sweep replay can succeed with more gas, then still be marked
        // `schedule_success=false` because the computed schedule gas does not
        // fit the tx's original limit. There is no OOG site in the stored
        // successful replay, but the successful replay itself proves a wallet
        // gas-limit increase clears the failure.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            outer_limit_only_failure: true,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_deep_chain_when_proportional_with_subcalls() {
        // OOG at depth > 1 with the chain-walk classifier confirming every
        // hop got proportional gas → the deep-chain wallet-fixable rescue.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(3),
            oog_chain_proportional: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableDeepChain);
    }

    #[test]
    fn bucket_contract_broken_when_chain_throttled() {
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(2),
            oog_chain_proportional: Some(false),
            replay_halt_oog: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ContractBroken);
    }

    #[test]
    fn bucket_inconclusive_when_highest_sweep_still_oog_without_bottleneck() {
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(3),
            oog_chain_proportional: Some(true),
            replay_halt_oog: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::InconclusiveNeedsHigherSweep);
    }

    #[test]
    fn bucket_inconclusive_for_root_oog_when_sweep_exhausted() {
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(1),
            replay_halt_oog: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::InconclusiveNeedsHigherSweep);
    }

    #[test]
    fn bucket_contract_broken_when_oog_depth_missing() {
        // Status flipped without an OOG signal (plain revert, or producer
        // didn't capture depth) and the chain-walk classifier didn't run
        // either — be conservative and call it broken.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: None,
            oog_chain_proportional: None,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ContractBroken);
    }

    #[test]
    fn bucket_shallow_outranks_deep_chain_when_both_apply() {
        // Depth 1 OOG, but also proportional chain (single-hop chain that
        // trivially passed). The shallow variant wins because it's the more
        // specific case.
        let input = BucketInput {
            baseline_to_schedule_break: true,
            oog_call_depth: Some(1),
            oog_chain_proportional: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_schedule_rescued_for_baseline_failed_schedule_succeeded() {
        // Beneficial flip: baseline failed, schedule succeeded. The schedule
        // *fixed* a tx the baseline couldn't run — not a failure, gets its
        // own bucket so it doesn't pollute the contract_broken cohort.
        let input = BucketInput { baseline_to_schedule_rescue: true, ..neutral_input() };
        assert_eq!(classify_bucket(&input), Bucket::ScheduleRescued);
    }

    #[test]
    fn bucket_break_outranks_rescue_at_input_level() {
        // The two direction flags are mutually exclusive at the producer
        // level, but classify_bucket is defensive: if both are set we take
        // the break direction (more conservative — surface the failure).
        let input = BucketInput {
            baseline_to_schedule_break: true,
            baseline_to_schedule_rescue: true,
            oog_call_depth: Some(1),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_rescue_outranks_event_logs_and_gas() {
        // A rescued tx will often have a gas delta or different logs (the
        // baseline reverted with no logs, the schedule succeeded with full
        // logs). The rescue label is the actionable signal — it should win.
        let input = BucketInput {
            baseline_to_schedule_rescue: true,
            event_logs_changed: true,
            gas_delta: -50_000,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ScheduleRescued);
    }

    #[test]
    fn bucket_as_str_matches_consumer_doc() {
        // Lock the wire format. The consumer doc enumerates these exact
        // values — any rename here is a coordinated change.
        assert_eq!(Bucket::Unchanged.as_str(), "unchanged");
        assert_eq!(Bucket::TraceOnly.as_str(), "trace_only");
        assert_eq!(Bucket::GasOnly.as_str(), "gas_only");
        assert_eq!(Bucket::EventLogsChanged.as_str(), "event_logs_changed");
        assert_eq!(Bucket::ScheduleRescued.as_str(), "schedule_rescued");
        assert_eq!(Bucket::WalletFixableShallow.as_str(), "wallet_fixable_shallow");
        assert_eq!(Bucket::WalletFixableDeepChain.as_str(), "wallet_fixable_deep_chain");
        assert_eq!(
            Bucket::InconclusiveNeedsHigherSweep.as_str(),
            "inconclusive_needs_higher_sweep"
        );
        assert_eq!(Bucket::ContractBroken.as_str(), "contract_broken");
    }

    #[test]
    fn bucket_is_drill_in_only_for_full_record_buckets() {
        for bucket in [
            Bucket::EventLogsChanged,
            Bucket::InconclusiveNeedsHigherSweep,
            Bucket::ContractBroken,
            Bucket::AaGasReestimation,
        ] {
            assert!(bucket.is_drill_in(), "{bucket:?} should be drill-in");
        }
        for bucket in [
            Bucket::Unchanged,
            Bucket::TraceOnly,
            Bucket::GasOnly,
            Bucket::ScheduleRescued,
            Bucket::WalletFixableShallow,
            Bucket::WalletFixableDeepChain,
        ] {
            assert!(!bucket.is_drill_in(), "{bucket:?} should be aggregate-only");
        }
    }
}
