//! Simple inspector that only tracks operations without modifying execution.

use crate::divergence::{CallFrame, CallType, OperationCounts};
use alloy_primitives::{Address, Bytes};
use revm::{
    context_interface::ContextTr,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    Inspector,
};
use revm_interpreter::interpreter_types::Jumps;

/// Inspector that tracks operation counts without modifying execution.
///
/// Used for the "normal" execution to get accurate operation counts
/// without any gas manipulation.
#[derive(Debug, Default)]
pub struct TrackingInspector {
    /// Operation counts
    op_counts: OperationCounts,

    /// Call stack for tracking depth
    call_stack: Vec<CallStackEntry>,

    /// Recorded call frames
    call_frames: Vec<CallFrame>,

    /// Event logs captured
    event_logs: Vec<EventLogEntry>,
}

/// Entry in the call stack.
#[derive(Debug, Clone)]
struct CallStackEntry {
    call_index: usize,
    depth: usize,
    from: Address,
    to: Option<Address>,
    call_type: CallType,
    gas_provided: u64,
}

/// Captured event log.
#[derive(Debug, Clone)]
pub struct EventLogEntry {
    pub log_index: usize,
    pub address: Address,
    pub topics: Vec<alloy_primitives::B256>,
    pub data: Bytes,
}

impl TrackingInspector {
    /// Create a new tracking inspector.
    pub fn new() -> Self {
        Self {
            op_counts: OperationCounts::default(),
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            event_logs: Vec::new(),
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

    /// Get the event logs.
    pub fn event_logs(&self) -> &[EventLogEntry] {
        &self.event_logs
    }
}

impl<CTX> Inspector<CTX> for TrackingInspector
where
    CTX: ContextTr,
{
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        self.op_counts.total_ops += 1;

        // Track specific operations
        let opcode = interp.bytecode.opcode();

        match opcode {
            0x54 => {
                // SLOAD
                self.op_counts.sload_count += 1;
            }
            0x55 => {
                // SSTORE
                self.op_counts.sstore_count += 1;
            }
            0xF1 | 0xF2 | 0xF4 | 0xFA => {
                // CALL, CALLCODE, DELEGATECALL, STATICCALL
                self.op_counts.call_count += 1;
            }
            0xF0 | 0xF5 => {
                // CREATE, CREATE2
                self.op_counts.create_count += 1;
            }
            0xA0..=0xA4 => {
                // LOG0-LOG4
                self.op_counts.log_count += 1;
            }
            _ => {}
        }

        // Track memory allocation
        let memory_words = interp.memory.len() / 32;
        if memory_words as u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words as u64;
        }
    }

    fn call(
        &mut self,
        _context: &mut CTX,
        inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        let call_index = self.call_frames.len();
        let depth = self.call_stack.len();

        let call_type = match inputs.scheme {
            revm::interpreter::CallScheme::Call => CallType::Call,
            revm::interpreter::CallScheme::CallCode => CallType::CallCode,
            revm::interpreter::CallScheme::DelegateCall => CallType::DelegateCall,
            revm::interpreter::CallScheme::StaticCall => CallType::StaticCall,
        };

        self.call_stack.push(CallStackEntry {
            call_index,
            depth,
            from: inputs.caller,
            to: Some(inputs.target_address),
            call_type,
            gas_provided: inputs.gas_limit,
        });

        None
    }

    fn call_end(
        &mut self,
        _context: &mut CTX,
        inputs: &CallInputs,
        outcome: &mut CallOutcome,
    ) {
        if let Some(entry) = self.call_stack.pop() {
            // Extract input bytes based on CallInput enum
            let input_bytes = match &inputs.input {
                revm::interpreter::CallInput::Bytes(bytes) => Some(bytes.clone()),
                revm::interpreter::CallInput::SharedBuffer(_) => None,
            };

            // Calculate gas used (gas_provided - gas_remaining)
            let gas_used = entry.gas_provided.saturating_sub(outcome.result.gas.remaining());

            self.call_frames.push(CallFrame {
                call_index: entry.call_index,
                depth: entry.depth,
                from: entry.from,
                to: entry.to,
                call_type: entry.call_type,
                gas_provided: entry.gas_provided,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: input_bytes,
                output: Some(outcome.result.output.clone()),
            });
        }
    }

    fn create(
        &mut self,
        _context: &mut CTX,
        inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        let call_index = self.call_frames.len();
        let depth = self.call_stack.len();

        let call_type = match inputs.scheme {
            revm::context_interface::CreateScheme::Create => CallType::Create,
            revm::context_interface::CreateScheme::Create2 { .. } |
            revm::context_interface::CreateScheme::Custom { .. } => CallType::Create2,
        };

        self.call_stack.push(CallStackEntry {
            call_index,
            depth,
            from: inputs.caller,
            to: None, // CREATE doesn't have a target address yet
            call_type,
            gas_provided: inputs.gas_limit,
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
            let created_address = outcome.address.unwrap_or(Address::ZERO);
            let gas_used = entry.gas_provided.saturating_sub(outcome.result.gas.remaining());

            self.call_frames.push(CallFrame {
                call_index: entry.call_index,
                depth: entry.depth,
                from: entry.from,
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: entry.gas_provided,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: Some(inputs.init_code.clone()),
                output: Some(outcome.result.output.clone()),
            });
        }
    }

    fn log(
        &mut self,
        _interp: &mut Interpreter,
        _context: &mut CTX,
        log: alloy_primitives::Log,
    ) {
        self.event_logs.push(EventLogEntry {
            log_index: self.event_logs.len(),
            address: log.address,
            topics: log.topics().to_vec(),
            data: log.data.data.clone(),
        });
    }
}
