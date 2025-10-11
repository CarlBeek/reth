//! Simple inspector that only tracks operations without modifying execution.

use crate::divergence::OperationCounts;
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
}

impl TrackingInspector {
    /// Create a new tracking inspector.
    pub fn new() -> Self {
        Self { op_counts: OperationCounts::default() }
    }

    /// Get the operation counts.
    pub fn operation_counts(&self) -> &OperationCounts {
        &self.op_counts
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
        _inputs: &mut CallInputs,
    ) -> Option<CallOutcome> {
        None
    }

    fn call_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CallInputs,
        _outcome: &mut CallOutcome,
    ) {
        // No-op
    }

    fn create(
        &mut self,
        _context: &mut CTX,
        _inputs: &mut CreateInputs,
    ) -> Option<CreateOutcome> {
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        // No-op
    }
}
