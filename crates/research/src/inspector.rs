//! Inspector for tracking execution and simulating gas cost changes.

use crate::{
    config::ResearchConfig,
    divergence::{
        CallFrame, CallType, DivergenceLocation, OogPattern, OperationCounts, OutOfGasInfo,
    },
    gas_pricing::OPCODE_KECCAK256,
};
use alloy_primitives::{Address, Bytes};
use revm::{
    context_interface::ContextTr,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    Inspector,
};
use revm_interpreter::interpreter_types::Jumps;
use std::collections::VecDeque;

/// Inspector that applies per-opcode gas repricing and tracks execution details.
///
/// This inspector charges additional gas based on the difference between new and current
/// gas costs as specified in the gas pricing CSV. Operations not in the CSV are unaffected.
#[derive(Debug)]
pub struct GasResearchInspector {
    /// Configuration including gas pricing table
    config: ResearchConfig,

    /// Total additional gas charged due to repricing
    additional_gas_charged: u64,

    /// Whether out-of-gas occurred
    oog_occurred: bool,

    /// Operation counts
    op_counts: OperationCounts,

    /// Call stack for tracking depth
    call_stack: Vec<CallStackEntry>,

    /// Recorded call frames
    call_frames: Vec<CallFrame>,

    /// Location of first divergence (if any)
    first_divergence_location: Option<DivergenceLocation>,

    /// Out-of-gas information
    oog_info: Option<OutOfGasInfo>,

    /// Gas opcode usage tracking (for loop detection)
    gas_opcode_usage: VecDeque<GasOpcodeEvent>,

    /// Maximum entries to track for gas loop detection
    max_gas_events: usize,

    /// Pending precompile call info for gas charging in call_end
    pending_precompile: Option<PendingPrecompile>,
}

/// Entry in the call stack.
#[derive(Debug, Clone)]
struct CallStackEntry {
    depth: usize,
    contract: Address,
    call_type: CallType,
    gas_at_start: u64,
    function_selector: Option<[u8; 4]>,
}

/// Gas opcode usage event for loop detection.
#[derive(Debug, Clone)]
struct GasOpcodeEvent {
    pc: usize,
    #[allow(dead_code)]
    gas_remaining: u64,
    #[allow(dead_code)]
    contract: Address,
}

/// Pending precompile call for additional gas charging
#[derive(Debug, Clone)]
struct PendingPrecompile {
    address: Address,
    input: Bytes,
}

impl GasResearchInspector {
    /// Create a new inspector.
    pub fn new(config: ResearchConfig, _gas_limit: u64) -> Self {
        Self {
            config,
            additional_gas_charged: 0,
            oog_occurred: false,
            op_counts: OperationCounts::default(),
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            first_divergence_location: None,
            oog_info: None,
            gas_opcode_usage: VecDeque::new(),
            max_gas_events: 1000,
            pending_precompile: None,
        }
    }

    /// Extract function selector (first 4 bytes) from call input
    fn extract_function_selector(input: &revm::interpreter::CallInput) -> Option<[u8; 4]> {
        match input {
            revm::interpreter::CallInput::Bytes(bytes) if bytes.len() >= 4 => {
                Some([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            _ => None,
        }
    }

    /// Get the operation counts.
    pub fn operation_counts(&self) -> &OperationCounts {
        &self.op_counts
    }

    /// Get the call frames.
    pub fn call_frames(&self) -> &[CallFrame] {
        &self.call_frames
    }

    /// Get the divergence location.
    pub fn divergence_location(&self) -> Option<&DivergenceLocation> {
        self.first_divergence_location.as_ref()
    }

    /// Get out-of-gas information.
    pub fn oog_info(&self) -> Option<&OutOfGasInfo> {
        self.oog_info.as_ref()
    }

    /// Check if OOG occurred.
    pub fn oog_occurred(&self) -> bool {
        self.oog_occurred
    }

    /// Get total additional gas charged due to repricing.
    pub fn additional_gas_charged(&self) -> u64 {
        self.additional_gas_charged
    }

    /// Get simulated gas used (for compatibility with existing code).
    pub fn simulated_gas_used(&self) -> u64 {
        self.additional_gas_charged
    }

    /// Check if a potential gas-dependent loop is detected.
    pub fn has_gas_loop_pattern(&self) -> bool {
        // Look for repeated GAS opcode usage at same PC with decreasing gas
        if self.gas_opcode_usage.len() < 3 {
            return false;
        }

        // Simple heuristic: same PC accessed multiple times with decreasing gas
        let mut pc_counts = std::collections::HashMap::new();
        for event in &self.gas_opcode_usage {
            *pc_counts.entry(event.pc).or_insert(0) += 1;
        }

        pc_counts.values().any(|&count| count >= 3)
    }

    /// Calculate the new gas cost for an opcode based on the pricing table.
    /// Returns 0 if the opcode is not in the table (unaffected by repricing).
    fn calculate_opcode_new_gas(
        &self,
        opcode: u8,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
    ) -> u64 {
        let Some(pricing) = self.config.gas_pricing.get_opcode_pricing(opcode) else {
            return 0;
        };

        if opcode == OPCODE_KECCAK256 {
            // KECCAK256 has variable cost based on message size
            // Stack: [offset, size, ...] - we need to read size from stack
            let msg_size = self.get_keccak_msg_size(interp);
            let msg_words = (msg_size + 31) / 32; // Round up to words
            pricing.total_new_gas(msg_words as u64)
        } else {
            // Constant cost only
            pricing.new_constant_gas()
        }
    }

    /// Get KECCAK256 message size from the interpreter stack.
    /// KECCAK256 stack is [offset, size] so size is second from top.
    fn get_keccak_msg_size(
        &self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
    ) -> usize {
        // Stack layout for KECCAK256: top=[offset], second=[size]
        // We need the second element (size)
        if interp.stack.len() >= 2 {
            // stack.peek returns the nth element from the top (0 = top)
            if let Ok(size) = interp.stack.peek(1) {
                // Convert U256 to usize, saturating at usize::MAX
                return size.try_into().unwrap_or(usize::MAX);
            }
        }
        0
    }

    /// Calculate the new gas cost for a precompile call based on the pricing table.
    /// Returns 0 if the precompile is not in the table (unaffected by repricing).
    fn calculate_precompile_new_gas(&self, address: &Address, input: &[u8]) -> u64 {
        let Some(pricing) = self.config.gas_pricing.get_precompile_pricing(address) else {
            return 0;
        };

        // Calculate variable units based on the precompile type
        let variable_units = self.get_precompile_variable_units(address, input);
        pricing.total_new_gas(variable_units)
    }

    /// Get variable units for precompile gas calculation.
    fn get_precompile_variable_units(&self, address: &Address, input: &[u8]) -> u64 {
        // Check the last byte of the address to identify the precompile
        let addr_byte = address.0[19];

        match addr_byte {
            // ECPAIRING (0x08): num_pairs = input.len() / 192
            0x08 => (input.len() / 192) as u64,

            // BLAKE2F (0x09): num_rounds from first 4 bytes
            0x09 => {
                if input.len() >= 4 {
                    u32::from_be_bytes([input[0], input[1], input[2], input[3]]) as u64
                } else {
                    0
                }
            }

            // Other precompiles: constant cost only
            _ => 0,
        }
    }

    /// Record a divergence location if not already recorded.
    fn record_divergence_location(
        &mut self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        opcode: u8,
        opcode_name: String,
    ) {
        if self.first_divergence_location.is_none() {
            let contract = if let Some(entry) = self.call_stack.last() {
                entry.contract
            } else {
                Address::ZERO
            };

            // Collect function selectors from the entire call stack
            let function_selectors: Vec<Option<[u8; 4]>> =
                self.call_stack.iter().map(|entry| entry.function_selector).collect();

            self.first_divergence_location = Some(DivergenceLocation {
                contract,
                function_selectors,
                pc: interp.bytecode.pc(),
                call_depth: self.call_stack.len(),
                opcode,
                opcode_name,
            });
        }
    }

    /// Record OOG information.
    fn record_oog(&mut self, interp: &Interpreter, opcode: u8, opcode_name: String) {
        if self.oog_info.is_none() {
            let contract = if let Some(entry) = self.call_stack.last() {
                entry.contract
            } else {
                Address::ZERO
            };

            // Determine pattern heuristically
            let pattern = self.infer_oog_pattern(opcode);

            self.oog_info = Some(OutOfGasInfo {
                opcode,
                opcode_name,
                pc: interp.bytecode.pc(),
                contract,
                call_depth: self.call_stack.len(),
                gas_remaining: interp.gas.remaining(),
                pattern,
            });
        }
    }

    /// Infer the OOG pattern based on context.
    fn infer_oog_pattern(&self, opcode: u8) -> OogPattern {
        match opcode {
            // Storage operations
            0x54 | 0x55 => OogPattern::StorageHeavy, // SLOAD, SSTORE

            // Call operations
            0xF1 | 0xF2 | 0xF4 | 0xFA => OogPattern::CallChain, /* CALL, CALLCODE, DELEGATECALL,
                                                                  * STATICCALL */

            // Memory operations
            0x51..=0x53 => OogPattern::MemoryExpansion, // MLOAD, MSTORE, MSTORE8

            // If we detected a gas loop pattern, assume it's a loop
            _ if self.has_gas_loop_pattern() => OogPattern::Loop,

            _ => OogPattern::Unknown,
        }
    }

    /// Track a GAS opcode usage.
    fn track_gas_opcode(&mut self, interp: &Interpreter) {
        let contract =
            if let Some(entry) = self.call_stack.last() { entry.contract } else { Address::ZERO };

        self.gas_opcode_usage.push_back(GasOpcodeEvent {
            pc: interp.bytecode.pc(),
            gas_remaining: interp.gas.remaining(),
            contract,
        });

        // Keep only recent events
        while self.gas_opcode_usage.len() > self.max_gas_events {
            self.gas_opcode_usage.pop_front();
        }
    }
}

impl<CTX> Inspector<CTX, revm::interpreter::interpreter::EthInterpreter> for GasResearchInspector
where
    CTX: ContextTr,
{
    fn step(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        // Get the current opcode
        let opcode_byte = interp.bytecode.opcode();

        // Track total operations
        self.op_counts.total_ops += 1;

        // Track specific operations
        match opcode_byte {
            0x54 => self.op_counts.sload_count += 1,  // SLOAD
            0x55 => self.op_counts.sstore_count += 1, // SSTORE
            0xA0..=0xA4 => {
                // LOG0-LOG4
                self.op_counts.log_count += 1
            }
            0xF1 | 0xF2 | 0xF4 | 0xFA => {
                // CALL, CALLCODE, DELEGATECALL, STATICCALL
                self.op_counts.call_count += 1
            }
            0xF0 | 0xF5 => self.op_counts.create_count += 1, // CREATE, CREATE2
            0x5A => {
                // GAS
                if self.config.detect_gas_loops {
                    self.track_gas_opcode(interp);
                }
            }
            _ => {}
        }

        // Track memory usage
        let memory_size = interp.memory.len();
        let memory_words = memory_size.div_ceil(32);
        let memory_words_u64 = memory_words as u64;
        if memory_words_u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words_u64;
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        // Skip if we already hit OOG
        if self.oog_occurred {
            return;
        }

        let opcode_byte = interp.bytecode.opcode();

        // Calculate additional gas for this opcode based on pricing table
        let additional_gas = self.calculate_opcode_new_gas(opcode_byte, interp);

        if additional_gas > 0 {
            // Track additional gas charged
            self.additional_gas_charged =
                self.additional_gas_charged.saturating_add(additional_gas);

            // Try to charge the additional gas
            if !interp.gas.record_cost(additional_gas) {
                // OUT OF GAS! The execution failed due to repricing
                self.oog_occurred = true;

                let opcode_name = format!("0x{:02x}", opcode_byte);

                self.record_oog(interp, opcode_byte, opcode_name.clone());
                self.record_divergence_location(interp, opcode_byte, opcode_name);
            }
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        // Track call depth
        let call_type = match inputs.scheme {
            revm::interpreter::CallScheme::Call => CallType::Call,
            revm::interpreter::CallScheme::CallCode => CallType::CallCode,
            revm::interpreter::CallScheme::DelegateCall => CallType::DelegateCall,
            revm::interpreter::CallScheme::StaticCall => CallType::StaticCall,
        };

        let function_selector = Self::extract_function_selector(&inputs.input);

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: inputs.bytecode_address,
            call_type,
            gas_at_start: self.additional_gas_charged,
            function_selector,
        });

        // Check if this is a precompile that needs repricing
        if self.config.gas_pricing.is_repriced_precompile(&inputs.bytecode_address) {
            // Extract input bytes for gas calculation in call_end
            let input_bytes = match &inputs.input {
                revm::interpreter::CallInput::Bytes(bytes) => bytes.clone(),
                revm::interpreter::CallInput::SharedBuffer(_) => Bytes::new(),
            };

            self.pending_precompile =
                Some(PendingPrecompile { address: inputs.bytecode_address, input: input_bytes });
        }

        None // Let execution continue normally
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        // Handle precompile additional gas charging
        if let Some(pending) = self.pending_precompile.take() {
            // Only charge if the precompile call succeeded
            if outcome.result.result.is_ok() {
                let additional_gas =
                    self.calculate_precompile_new_gas(&pending.address, &pending.input);

                if additional_gas > 0 {
                    self.additional_gas_charged =
                        self.additional_gas_charged.saturating_add(additional_gas);

                    // For precompiles, we check if there's enough gas remaining in the outcome
                    let gas_remaining = outcome.gas().remaining();
                    if gas_remaining < additional_gas {
                        // Mark as OOG
                        self.oog_occurred = true;

                        // Record OOG info for precompile
                        if self.oog_info.is_none() {
                            self.oog_info = Some(OutOfGasInfo {
                                opcode: 0xF1, // CALL opcode
                                opcode_name: format!("PRECOMPILE_{:?}", pending.address),
                                pc: 0,
                                contract: pending.address,
                                call_depth: self.call_stack.len(),
                                gas_remaining,
                                pattern: OogPattern::CallChain,
                            });
                        }

                        // Modify outcome to reflect OOG
                        outcome.result.result = revm::interpreter::InstructionResult::OutOfGas;
                    } else {
                        // Deduct the additional gas from the outcome
                        let _ = outcome.gas().record_cost(additional_gas);
                    }
                }
            }
        }

        // Record the call frame
        if let Some(entry) = self.call_stack.pop() {
            let gas_used = self.additional_gas_charged.saturating_sub(entry.gas_at_start);

            // Extract input bytes based on CallInput enum
            let input_bytes = match &inputs.input {
                revm::interpreter::CallInput::Bytes(bytes) => Some(bytes.clone()),
                revm::interpreter::CallInput::SharedBuffer(_) => None, /* Can't safely access
                                                                        * without context */
            };

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller,
                to: Some(inputs.bytecode_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: input_bytes,
                output: Some(outcome.result.output.clone()),
            });
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: Address::ZERO, // Will be filled in create_end
            call_type: match inputs.scheme() {
                revm::context_interface::CreateScheme::Create => CallType::Create,
                revm::context_interface::CreateScheme::Create2 { .. } |
                revm::context_interface::CreateScheme::Custom { .. } => CallType::Create2,
            },
            gas_at_start: self.additional_gas_charged,
            function_selector: None, // CREATE operations don't have function selectors
        });

        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        if let Some(entry) = self.call_stack.pop() {
            let gas_used = self.additional_gas_charged.saturating_sub(entry.gas_at_start);
            let created_address = outcome.address.unwrap_or(Address::ZERO);

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller(),
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit(),
                gas_used,
                success: outcome.result.result.is_ok(),
                input: Some(inputs.init_code().clone()),
                output: Some(outcome.result.output.clone()),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gas_pricing::GasPricingTable;

    const TEST_CSV: &str = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
KECCAK256,constant,30,45
KECCAK256,msg_size,6,6
ECPAIRING,constant,45000,45000
ECPAIRING,num_pairs,34000,34103
"#;

    #[test]
    fn test_inspector_creation() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();
        let config = ResearchConfig { gas_pricing: table, ..Default::default() };

        let inspector = GasResearchInspector::new(config, 100_000);

        assert_eq!(inspector.additional_gas_charged(), 0);
        assert!(!inspector.oog_occurred());
        assert_eq!(inspector.operation_counts().total_ops, 0);
    }

    #[test]
    fn test_precompile_variable_units() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();
        let config = ResearchConfig { gas_pricing: table, ..Default::default() };

        let inspector = GasResearchInspector::new(config, 100_000);

        // ECPAIRING address (0x08)
        let ecpairing_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);

        // 3 pairs = 576 bytes (192 * 3)
        let input = vec![0u8; 576];
        let units = inspector.get_precompile_variable_units(&ecpairing_addr, &input);
        assert_eq!(units, 3);

        // BLAKE2F address (0x09)
        let blake2f_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09]);

        // 100 rounds encoded in first 4 bytes (big endian)
        let mut input = vec![0u8; 213]; // BLAKE2F input is 213 bytes
        input[0..4].copy_from_slice(&100u32.to_be_bytes());
        let units = inspector.get_precompile_variable_units(&blake2f_addr, &input);
        assert_eq!(units, 100);
    }

    #[test]
    fn test_precompile_new_gas() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();
        let config = ResearchConfig { gas_pricing: table, ..Default::default() };

        let inspector = GasResearchInspector::new(config, 100_000);

        // ECPAIRING with 3 pairs
        // New gas = 45000 + 3 * 34103 = 147309
        let ecpairing_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08]);
        let input = vec![0u8; 576]; // 3 pairs
        let new_gas = inspector.calculate_precompile_new_gas(&ecpairing_addr, &input);
        assert_eq!(new_gas, 147309);
    }

    #[test]
    fn test_non_repriced_precompile() {
        let table = GasPricingTable::from_csv(TEST_CSV.as_bytes()).unwrap();
        let config = ResearchConfig { gas_pricing: table, ..Default::default() };

        let inspector = GasResearchInspector::new(config, 100_000);

        // Precompile at 0x99 is not in the pricing table
        let non_repriced_addr =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x99]);
        let new_gas = inspector.calculate_precompile_new_gas(&non_repriced_addr, &[0u8; 32]);
        assert_eq!(new_gas, 0);
    }
}
