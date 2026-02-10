//! Context types for gas schedule calculations.

use alloy_primitives::{Address, Bytes, U256};

/// Transaction context for intrinsic gas calculation.
///
/// Contains all information needed to classify a transaction and
/// calculate its intrinsic gas under different schedules.
#[derive(Debug, Clone, Default)]
pub struct TxContext {
    /// Transaction sender address
    pub sender: Address,

    /// Transaction recipient (None for contract creation)
    pub recipient: Option<Address>,

    /// Transaction value (ETH being transferred)
    pub value: U256,

    /// Transaction input data (calldata)
    pub input: Bytes,

    /// Gas limit set by the transaction
    pub gas_limit: u64,

    /// Whether this is a contract creation transaction
    pub is_create: bool,

    /// Recipient account info (for classifying tx type)
    pub recipient_info: Option<RecipientInfo>,
}

/// Information about the transaction recipient.
#[derive(Debug, Clone, Default)]
pub struct RecipientInfo {
    /// Whether the recipient account exists
    pub exists: bool,

    /// Whether the recipient has code (is a contract)
    pub has_code: bool,

    /// Recipient's current balance
    pub balance: U256,

    /// Recipient's current nonce
    pub nonce: u64,
}

impl RecipientInfo {
    /// Check if this is an empty account (zero balance, zero nonce, no code).
    pub fn is_empty(&self) -> bool {
        !self.has_code && self.balance.is_zero() && self.nonce == 0
    }

    /// Check if this is an EOA (externally owned account - no code).
    pub fn is_eoa(&self) -> bool {
        !self.has_code
    }

    /// Check if this is a contract (has code).
    pub fn is_contract(&self) -> bool {
        self.has_code
    }
}

impl TxContext {
    /// Create a new transaction context.
    pub fn new(
        sender: Address,
        recipient: Option<Address>,
        value: U256,
        input: Bytes,
        gas_limit: u64,
        is_create: bool,
    ) -> Self {
        Self { sender, recipient, value, input, gas_limit, is_create, recipient_info: None }
    }

    /// Add recipient account info to the context.
    pub fn with_recipient_info(mut self, info: RecipientInfo) -> Self {
        self.recipient_info = Some(info);
        self
    }

    /// Check if this is a self-transfer (sender == recipient).
    pub fn is_self_transfer(&self) -> bool {
        self.recipient.map(|r| r == self.sender).unwrap_or(false)
    }

    /// Check if this transaction transfers value.
    pub fn has_value(&self) -> bool {
        !self.value.is_zero()
    }

    /// Calculate calldata gas cost (4 per zero byte, 16 per non-zero byte).
    pub fn calldata_gas(&self) -> u64 {
        let zero_bytes = self.input.iter().filter(|&&b| b == 0).count() as u64;
        let nonzero_bytes = self.input.len() as u64 - zero_bytes;
        zero_bytes * 4 + nonzero_bytes * 16
    }
}

/// Opcode execution context for per-opcode gas calculation.
///
/// Contains information about the current execution state needed
/// to calculate gas costs for variable-cost operations.
#[derive(Debug, Clone, Default)]
pub struct OpcodeContext {
    /// Current contract being executed
    pub contract: Address,

    /// Program counter
    pub pc: usize,

    /// Current call depth
    pub call_depth: usize,

    /// Gas remaining before this opcode
    pub gas_remaining: u64,

    /// Memory size in bytes (for memory expansion calculations)
    pub memory_size: usize,

    /// For KECCAK256: message size in bytes
    pub keccak_msg_size: Option<usize>,

    /// For EXP: exponent byte size
    pub exp_byte_size: Option<usize>,

    /// For memory operations: offset being accessed
    pub memory_offset: Option<usize>,

    /// For memory operations: size being accessed
    pub memory_access_size: Option<usize>,
}

impl OpcodeContext {
    /// Create a new opcode context.
    pub fn new(contract: Address, pc: usize, call_depth: usize, gas_remaining: u64) -> Self {
        Self {
            contract,
            pc,
            call_depth,
            gas_remaining,
            memory_size: 0,
            keccak_msg_size: None,
            exp_byte_size: None,
            memory_offset: None,
            memory_access_size: None,
        }
    }

    /// Set KECCAK256 message size.
    pub fn with_keccak_msg_size(mut self, size: usize) -> Self {
        self.keccak_msg_size = Some(size);
        self
    }

    /// Set EXP exponent byte size.
    pub fn with_exp_byte_size(mut self, size: usize) -> Self {
        self.exp_byte_size = Some(size);
        self
    }

    /// Set memory access info.
    pub fn with_memory_access(mut self, offset: usize, size: usize) -> Self {
        self.memory_offset = Some(offset);
        self.memory_access_size = Some(size);
        self
    }

    /// Get KECCAK256 message size in 32-byte words.
    pub fn keccak_words(&self) -> u64 {
        self.keccak_msg_size.map(|s| s.div_ceil(32) as u64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_context_calldata_gas() {
        let ctx = TxContext {
            input: Bytes::from(vec![0, 0, 0, 1, 2, 3]),
            ..Default::default()
        };
        // 3 zeros * 4 + 3 non-zeros * 16 = 12 + 48 = 60
        assert_eq!(ctx.calldata_gas(), 60);
    }

    #[test]
    fn test_recipient_info_classification() {
        let empty = RecipientInfo { exists: true, has_code: false, balance: U256::ZERO, nonce: 0 };
        assert!(empty.is_empty());
        assert!(empty.is_eoa());
        assert!(!empty.is_contract());

        let eoa =
            RecipientInfo { exists: true, has_code: false, balance: U256::from(100), nonce: 5 };
        assert!(!eoa.is_empty());
        assert!(eoa.is_eoa());

        let contract =
            RecipientInfo { exists: true, has_code: true, balance: U256::ZERO, nonce: 1 };
        assert!(contract.is_contract());
        assert!(!contract.is_eoa());
    }

    #[test]
    fn test_opcode_context_keccak_words() {
        let ctx = OpcodeContext::default().with_keccak_msg_size(100);
        // 100 bytes = 4 words (ceil(100/32))
        assert_eq!(ctx.keccak_words(), 4);

        let ctx = OpcodeContext::default().with_keccak_msg_size(32);
        assert_eq!(ctx.keccak_words(), 1);

        let ctx = OpcodeContext::default().with_keccak_msg_size(33);
        assert_eq!(ctx.keccak_words(), 2);
    }
}
