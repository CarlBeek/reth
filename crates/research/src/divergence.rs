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
}

/// Location where divergence first occurred.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DivergenceLocation {
    /// Contract address where divergence occurred
    pub contract: Address,

    /// Function selector (first 4 bytes of calldata)
    pub function_selector: Option<[u8; 4]>,

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}
