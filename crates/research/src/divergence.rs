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

    /// Gas efficiency ratio: (experimental_gas / multiplier) / normal_gas
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

/// Per-tx classification used to decide whether the divergence is written
/// per-tx (drill-in bucket) or rolled into per-block aggregates.
///
/// See `crates/research/docs/storage-redesign.md` for the bucket semantics.
/// The classifier in [`classify_bucket`] applies a fixed priority order:
/// `status_changed` divergences fall into one of the wallet-fixable variants
/// or `ContractBroken`; non-status-changed divergences are classified by the
/// next available signal (event logs, gas, other-trace), and otherwise come
/// out as `Unchanged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Outcome flipped *and* the legacy shallow heuristic holds: divergence
    /// happened at depth ≤ 1 with no internal calls. Wallet-fixable: raising
    /// the wallet's gas resolves it.
    WalletFixableShallow,
    /// Outcome flipped *and* the OOG-chain walk confirmed every parent→child
    /// hop got proportional gas via the EIP-150 63/64 rule. Wallet-fixable
    /// even though the tx involves subcalls.
    WalletFixableDeepChain,
    /// Outcome flipped and neither wallet-fixable rule applies. Drill-in
    /// bucket — full per-tx record retained.
    ContractBroken,
}

impl Bucket {
    /// Stable lowercase identifier for storage and the dashboard layer.
    /// Order kept identical to the consumer-side doc's enum.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::TraceOnly => "trace_only",
            Self::GasOnly => "gas_only",
            Self::EventLogsChanged => "event_logs_changed",
            Self::WalletFixableShallow => "wallet_fixable_shallow",
            Self::WalletFixableDeepChain => "wallet_fixable_deep_chain",
            Self::ContractBroken => "contract_broken",
        }
    }

    /// Whether divergences in this bucket are written per-tx (drill-in) or
    /// rolled into per-block aggregates.
    pub fn is_drill_in(&self) -> bool {
        matches!(self, Self::EventLogsChanged | Self::ContractBroken)
    }
}

impl std::fmt::Display for Bucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Inputs to [`classify_bucket`].
///
/// Bundled so the classifier signature stays readable at call sites and so
/// future fields (e.g. EOF-era markers) can be threaded through without
/// touching every caller.
#[derive(Debug, Clone, Copy)]
pub struct BucketInput {
    /// Baseline vs schedule `is_success()` differs.
    pub status_changed: bool,
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
    /// 1-based depth at which gas accounting first diverged, from
    /// `DivergenceLocation.call_depth`. `None` when no per-opcode divergence
    /// was recorded (native-revm OOGs, intrinsic-only schedules, etc.).
    pub divergence_call_depth: Option<usize>,
    /// Tx-level CALL/STATICCALL/DELEGATECALL/CALLCODE opcode count, from
    /// `OperationCounts.call_count`. Zero means the tx had no subcalls.
    pub call_count: u64,
    /// Result of the OOG-chain classifier (`oog_chain::classify_oog_chain`):
    /// `Some(true)` if every hop from root to OOG was proportional;
    /// `Some(false)` if some hop throttled; `None` when the divergence isn't
    /// an OOG or call frames were missing.
    pub oog_chain_proportional: Option<bool>,
}

/// Classify a per-tx divergence into a [`Bucket`] using the priority ladder
/// from `crates/research/docs/storage-redesign.md`:
///
/// 1. `status_changed` →
///    - `WalletFixableShallow` if the divergence was at depth ≤ 1 with no internal calls (legacy
///      shallow heuristic);
///    - else `WalletFixableDeepChain` if the OOG-chain classifier confirmed every hop was
///      proportional;
///    - else `ContractBroken`.
/// 2. `event_logs_changed` → `EventLogsChanged`.
/// 3. `gas_delta != 0` → `GasOnly`.
/// 4. Any other trace flag set → `TraceOnly`.
/// 5. Otherwise → `Unchanged`.
///
/// The ordering matters: a status-changed tx whose logs also differ is
/// `ContractBroken` (or wallet-fixable), not `EventLogsChanged`, because the
/// outcome flip is the actionable signal. Similarly, a non-status-changed tx
/// with both event-log and gas differences is classified as
/// `EventLogsChanged` — log changes are harder to dismiss than a gas delta
/// alone.
pub fn classify_bucket(input: &BucketInput) -> Bucket {
    if input.status_changed {
        if is_shallow_oog(input.divergence_call_depth, input.call_count) {
            Bucket::WalletFixableShallow
        } else if input.oog_chain_proportional == Some(true) {
            Bucket::WalletFixableDeepChain
        } else {
            Bucket::ContractBroken
        }
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

/// Legacy "shallow" heuristic preserved from the consumer's SQL:
/// `divergence_call_depth IS NOT NULL AND divergence_call_depth <= 1 AND
/// coalesce(call_count, 0) == 0`.
///
/// A tx with no internal calls and whose divergence was attributed to the
/// root frame is the canonical "wallets just need to estimate more gas" case
/// — kept distinct from `WalletFixableDeepChain` for the dashboard's
/// breakdown view.
fn is_shallow_oog(divergence_call_depth: Option<usize>, call_count: u64) -> bool {
    matches!(divergence_call_depth, Some(d) if d <= 1) && call_count == 0
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
            status_changed: false,
            gas_delta: 0,
            event_logs_changed: false,
            call_tree_changed: false,
            output_changed: false,
            created_address_changed: false,
            logs_bloom_changed: false,
            divergence_call_depth: None,
            call_count: 0,
            oog_chain_proportional: None,
        }
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
    fn bucket_status_changed_outranks_event_logs() {
        // A status flip with diverging logs is still a wallet-fixable /
        // contract-broken case; the outcome flip is the actionable signal.
        let input = BucketInput {
            status_changed: true,
            event_logs_changed: true,
            // Make it shallow so the variant is deterministic without
            // depending on oog_chain_proportional.
            divergence_call_depth: Some(1),
            call_count: 0,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_for_root_oog_without_subcalls() {
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(1),
            call_count: 0,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_wallet_fixable_shallow_requires_zero_call_count() {
        // Depth ≤ 1 but the tx made an internal call → not shallow even
        // though depth is small.
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(1),
            call_count: 1,
            oog_chain_proportional: None,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ContractBroken);
    }

    #[test]
    fn bucket_wallet_fixable_deep_chain_when_proportional_with_subcalls() {
        // Depth > 1 with subcalls, but the OOG-chain classifier said every
        // hop got proportional gas → the deep-chain wallet-fixable rescue.
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(3),
            call_count: 5,
            oog_chain_proportional: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableDeepChain);
    }

    #[test]
    fn bucket_contract_broken_when_chain_throttled() {
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(2),
            call_count: 3,
            oog_chain_proportional: Some(false),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ContractBroken);
    }

    #[test]
    fn bucket_contract_broken_when_chain_classification_missing() {
        // Status flipped, not shallow, and the OOG-chain classifier didn't
        // produce a verdict (None) — be conservative and call it broken
        // rather than wallet-fixable.
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(2),
            call_count: 3,
            oog_chain_proportional: None,
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::ContractBroken);
    }

    #[test]
    fn bucket_shallow_outranks_deep_chain_when_both_apply() {
        // Depth 1, no subcalls (shallow), but also proportional chain. The
        // shallow variant wins because it's the more specific case.
        let input = BucketInput {
            status_changed: true,
            divergence_call_depth: Some(1),
            call_count: 0,
            oog_chain_proportional: Some(true),
            ..neutral_input()
        };
        assert_eq!(classify_bucket(&input), Bucket::WalletFixableShallow);
    }

    #[test]
    fn bucket_as_str_matches_consumer_doc() {
        // Lock the wire format. The consumer doc enumerates these exact
        // values — any rename here is a coordinated change.
        assert_eq!(Bucket::Unchanged.as_str(), "unchanged");
        assert_eq!(Bucket::TraceOnly.as_str(), "trace_only");
        assert_eq!(Bucket::GasOnly.as_str(), "gas_only");
        assert_eq!(Bucket::EventLogsChanged.as_str(), "event_logs_changed");
        assert_eq!(Bucket::WalletFixableShallow.as_str(), "wallet_fixable_shallow");
        assert_eq!(Bucket::WalletFixableDeepChain.as_str(), "wallet_fixable_deep_chain");
        assert_eq!(Bucket::ContractBroken.as_str(), "contract_broken");
    }

    #[test]
    fn bucket_is_drill_in_only_for_full_record_buckets() {
        for bucket in [Bucket::EventLogsChanged, Bucket::ContractBroken] {
            assert!(bucket.is_drill_in(), "{bucket:?} should be drill-in");
        }
        for bucket in [
            Bucket::Unchanged,
            Bucket::TraceOnly,
            Bucket::GasOnly,
            Bucket::WalletFixableShallow,
            Bucket::WalletFixableDeepChain,
        ] {
            assert!(!bucket.is_drill_in(), "{bucket:?} should be aggregate-only");
        }
    }
}
