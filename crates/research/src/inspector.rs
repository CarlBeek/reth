//! Inspector for tracking execution and simulating gas cost changes.

use crate::{
    config::ResearchConfig,
    divergence::{
        CallFrame, CallType, DivergenceLocation, OogPattern, OperationCounts, OutOfGasInfo,
    },
};
use alloy_primitives::Address;
use revm::{
    context_interface::ContextTr,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    Inspector,
};
use revm_interpreter::interpreter_types::Jumps;
use std::collections::VecDeque;

/// Inspector that simulates high gas costs and tracks execution details.
#[derive(Debug)]
pub struct GasResearchInspector {
    /// Configuration
    config: ResearchConfig,

    /// Simulated gas used (with multiplier applied)
    simulated_gas_used: u64,

    /// Simulated gas limit (inflated)
    simulated_gas_limit: u64,

    /// Whether out-of-gas occurred in simulation
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
}

/// Entry in the call stack.
#[derive(Debug, Clone)]
struct CallStackEntry {
    depth: usize,
    contract: Address,
    call_type: CallType,
    gas_at_start: u64,
}

/// Gas opcode usage event for loop detection.
#[derive(Debug, Clone)]
struct GasOpcodeEvent {
    pc: usize,
    gas_remaining: u64,
    contract: Address,
}

impl GasResearchInspector {
    /// Create a new inspector.
    pub fn new(config: ResearchConfig, gas_limit: u64) -> Self {
        let simulated_gas_limit = gas_limit
            .saturating_mul(config.effective_gas_limit_multiplier())
            .saturating_sub(21000 * (config.gas_multiplier - 1)); // Adjust for intrinsic gas

        Self {
            config,
            simulated_gas_used: 0,
            simulated_gas_limit,
            oog_occurred: false,
            op_counts: OperationCounts::default(),
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            first_divergence_location: None,
            oog_info: None,
            gas_opcode_usage: VecDeque::new(),
            max_gas_events: 1000,
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

    /// Get simulated gas used.
    pub fn simulated_gas_used(&self) -> u64 {
        self.simulated_gas_used
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

    /// Calculate the gas cost for an operation with the multiplier applied.
    fn calculate_gas_cost(&self, base_cost: u64) -> u64 {
        base_cost.saturating_mul(self.config.gas_multiplier)
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

            self.first_divergence_location = Some(DivergenceLocation {
                contract,
                function_selector: None, // Could extract from calldata
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
            0x51 | 0x52 | 0x53 => OogPattern::MemoryExpansion, // MLOAD, MSTORE, MSTORE8

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
            0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 => {
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
        let memory_words = (memory_size + 31) / 32;
        let memory_words_u64 = memory_words as u64;
        if memory_words_u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words_u64;
        }

        // Get gas remaining (interp.gas is a public field)
        let gas_remaining = interp.gas.remaining();

        // Note: We can't actually intercept the gas calculation here, as revm will
        // calculate it after this hook returns. Instead, we estimate based on the opcode.
        // For accurate simulation, we'd need to fork revm or use a different approach.

        // Estimate gas cost (this is approximate - real costs depend on context)
        let estimated_base_cost = estimate_opcode_gas_cost(opcode_byte);
        let simulated_cost = self.calculate_gas_cost(estimated_base_cost);

        // Add to simulated gas used
        self.simulated_gas_used = self.simulated_gas_used.saturating_add(simulated_cost);

        // Check if we've exceeded the simulated gas limit
        if !self.oog_occurred && self.simulated_gas_used > self.simulated_gas_limit {
            self.oog_occurred = true;

            let opcode_name = format!("0x{:02x}", opcode_byte);

            self.record_oog(interp, opcode_byte, opcode_name.clone());
            self.record_divergence_location(interp, opcode_byte, opcode_name);

            // Note: We don't actually terminate here in our simulation approach
            // We just record that OOG would have occurred
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

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: inputs.bytecode_address,
            call_type,
            gas_at_start: self.simulated_gas_used,
        });

        None // Let execution continue normally
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        // Record the call frame
        if let Some(entry) = self.call_stack.pop() {
            let gas_used = self.simulated_gas_used.saturating_sub(entry.gas_at_start);

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
            call_type: match inputs.scheme {
                revm::context_interface::CreateScheme::Create => CallType::Create,
                revm::context_interface::CreateScheme::Create2 { .. } |
                revm::context_interface::CreateScheme::Custom { .. } => CallType::Create2,
            },
            gas_at_start: self.simulated_gas_used,
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
            let gas_used = self.simulated_gas_used.saturating_sub(entry.gas_at_start);
            let created_address = outcome.address.unwrap_or(Address::ZERO);

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller,
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: Some(inputs.init_code.clone()),
                output: Some(outcome.result.output.clone()),
            });
        }
    }
}

/// Estimate base gas cost for an opcode.
/// This is a simplified estimation - real costs depend on context (memory, storage, etc.)
fn estimate_opcode_gas_cost(opcode: u8) -> u64 {
    match opcode {
        // Very cheap: 2-3 gas (arithmetic, stack, etc.)
        0x01..=0x0B | // ADD through SIGNEXTEND
        0x10..=0x1D | // LT through BYTE
        0x1B..=0x1D | // SHL, SHR, SAR
        0x50 | // POP
        0x51 | 0x52 | 0x53 | // MLOAD, MSTORE, MSTORE8
        0x5F..=0x7F | // PUSH0-PUSH32
        0x80..=0x8F | // DUP1-DUP16
        0x90..=0x9F => 3, // SWAP1-SWAP16

        // Medium: 5-10 gas
        0x0A => 10, // EXP - Base cost, can be much higher
        0x20 => 30, // SHA3 - Base cost
        0x35 | 0x36 | 0x37 => 3, // CALLDATALOAD, CALLDATASIZE, CALLDATACOPY
        0x38 | 0x39 => 3, // CODESIZE, CODECOPY
        0x3D | 0x3E => 3, // RETURNDATASIZE, RETURNDATACOPY

        // Expensive: Storage operations
        0x54 => 800, // SLOAD - Warm access, can be 2100 for cold
        0x55 => 2900, // SSTORE - Can be 20000 for cold or creation

        // Very expensive: External calls and creates
        0xF1 | 0xF2 => 700, // CALL, CALLCODE - Base cost, can be much higher
        0xF4 | 0xFA => 700, // DELEGATECALL, STATICCALL
        0xF0 => 32000, // CREATE
        0xF5 => 32000, // CREATE2

        // Logs
        0xA0 => 375, // LOG0
        0xA1 => 375, // LOG1
        0xA2 => 375, // LOG2
        0xA3 => 375, // LOG3
        0xA4 => 375, // LOG4

        // Other operations
        0x57 => 10, // JUMPI
        0x56 => 8,  // JUMP
        0x58 => 2,  // PC
        0x59 => 2,  // MSIZE
        0x5A => 2,  // GAS

        // Default
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gas_calculation() {
        let config = ResearchConfig { gas_multiplier: 128, ..Default::default() };

        let inspector = GasResearchInspector::new(config, 100_000);

        // Base cost of 100 should become 12,800
        assert_eq!(inspector.calculate_gas_cost(100), 12_800);
    }

    #[test]
    fn test_opcode_gas_estimation() {
        assert_eq!(estimate_opcode_gas_cost(0x01), 3); // ADD
        assert_eq!(estimate_opcode_gas_cost(0x54), 800); // SLOAD
        assert_eq!(estimate_opcode_gas_cost(0x55), 2900); // SSTORE
        assert_eq!(estimate_opcode_gas_cost(0xF1), 700); // CALL
    }

    #[test]
    fn test_operation_counts_tracking() {
        let config = ResearchConfig::default();
        let inspector = GasResearchInspector::new(config, 100_000);

        assert_eq!(inspector.operation_counts().total_ops, 0);
        assert_eq!(inspector.operation_counts().sload_count, 0);
    }
}
