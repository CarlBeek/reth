//! Simple inspector that only tracks operations without modifying execution.

use crate::divergence::{
    CallFrame, CallType, EventLog, FrameOpcodeCounts, OperationCounts, PerFrameCapture,
    MAX_TRACKED_FRAMES,
};
use alloy_primitives::Address;
use revm::{
    context_interface::ContextTr,
    interpreter::{
        interpreter_types::Jumps, CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
    },
    Inspector,
};

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
    event_logs: Vec<EventLog>,

    /// Per-frame opcode counters. Mirrors the `ScheduleInspector`'s
    /// `frame_capture`; on this side `gas_baseline == gas_schedule` because
    /// no schedule deltas are applied.
    frame_capture: PerFrameCapture,

    /// Stack of indices into `frame_capture.frames` for the active frame
    /// chain. `usize::MAX` sentinel = "exceeded cap; skip increments".
    active_frame_stack: Vec<usize>,

    /// Next frame-open index to assign.
    next_frame_index: u32,

    /// Gas remaining at the start of the current opcode, captured in
    /// `step()` for the matching `step_end()` to derive natural cost.
    gas_before_step: Option<u64>,

    /// Opcode currently being executed (captured in `step()`).
    current_opcode: u8,

    /// Gas used by the callee that just returned. Set in
    /// `call_end()` / `create_end()`, consumed and cleared in the
    /// matching `step_end()` for the outer CALL/CREATE opcode.
    ///
    /// Without this, `step_end()`'s `gas_before - gas_remaining_after`
    /// for a CALL/CREATE opcode double-counts the callee: the callee's
    /// per-opcode gas is already attributed to its leaf opcodes via
    /// `step/step_end` inside the sub-frame, so attributing it again to
    /// the outer CALL/CREATE inflates totals (a tx whose root just
    /// CALLs a heavy contract would report twice its real gas).
    pending_callee_gas_used: u64,
}

/// Entry in the call stack.
#[derive(Debug, Clone)]
struct CallStackEntry {
    depth: usize,
    from: Address,
    to: Option<Address>,
    call_type: CallType,
    gas_provided: u64,
    #[allow(dead_code)]
    function_selector: Option<[u8; 4]>,
}

/// Captured event log.
pub type EventLogEntry = EventLog;

impl TrackingInspector {
    /// Create a new tracking inspector.
    pub fn new() -> Self {
        Self {
            op_counts: OperationCounts::default(),
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            event_logs: Vec::new(),
            frame_capture: PerFrameCapture::new(),
            active_frame_stack: Vec::new(),
            next_frame_index: 0,
            gas_before_step: None,
            current_opcode: 0,
            pending_callee_gas_used: 0,
        }
    }

    /// Whether `opcode` opens a sub-frame (CALL family + CREATE/CREATE2),
    /// i.e. one whose `step_end()` cost needs the callee's gas subtracted
    /// to avoid double-counting.
    const fn is_call_or_create(opcode: u8) -> bool {
        matches!(opcode, 0xF0 | 0xF1 | 0xF2 | 0xF4 | 0xF5 | 0xFA)
    }

    /// Get the operation counts.
    pub const fn operation_counts(&self) -> &OperationCounts {
        &self.op_counts
    }

    /// Get the call frames.
    pub fn call_frames(&self) -> &[CallFrame] {
        &self.call_frames
    }

    /// Get the event logs.
    pub fn event_logs(&self) -> &[EventLog] {
        &self.event_logs
    }

    /// Get the per-frame opcode capture. On the baseline path
    /// `gas_baseline == gas_schedule` for every opcode, since this
    /// inspector applies no schedule deltas.
    pub const fn frame_opcode_counts(&self) -> &PerFrameCapture {
        &self.frame_capture
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

    /// Push a new frame counter, honouring [`MAX_TRACKED_FRAMES`]; returns
    /// the index (or `usize::MAX` when the cap is hit).
    fn push_frame_counter(&mut self) -> usize {
        if self.next_frame_index >= MAX_TRACKED_FRAMES {
            self.frame_capture.truncated = true;
            return usize::MAX;
        }
        let idx = self.frame_capture.frames.len();
        self.frame_capture.frames.push(FrameOpcodeCounts::new(self.next_frame_index));
        self.next_frame_index += 1;
        idx
    }

    /// Lazily push the root frame's counter on the first observed opcode.
    fn ensure_root_frame_counter(&mut self) {
        if self.active_frame_stack.is_empty() {
            let idx = self.push_frame_counter();
            self.active_frame_stack.push(idx);
        }
    }

    /// Increment count and natural gas cost on the active frame's counter.
    /// Both `gas_baseline` and `gas_schedule` get the same value because
    /// this inspector never applies a schedule delta.
    fn record_opcode_in_active_frame(&mut self, opcode: u8, gas_cost: u64) {
        let Some(&idx) = self.active_frame_stack.last() else { return };
        if idx == usize::MAX {
            return;
        }
        if let Some(frame) = self.frame_capture.frames.get_mut(idx) {
            let i = opcode as usize;
            frame.counts[i] = frame.counts[i].saturating_add(1);
            frame.gas_baseline[i] = frame.gas_baseline[i].saturating_add(gas_cost);
            frame.gas_schedule[i] = frame.gas_schedule[i].saturating_add(gas_cost);
        }
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
        self.current_opcode = opcode;
        self.gas_before_step = Some(interp.gas.remaining());

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

        // Per-frame opcode bookkeeping. The root frame is lazy-init here
        // because `call()` / `create()` only fire for sub-frames. Count
        // increment happens now; gas is added in `step_end()` once we can
        // observe the actual EVM charge.
        self.ensure_root_frame_counter();

        // Track memory allocation
        let memory_words = interp.memory.len() / 32;
        if memory_words as u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words as u64;
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        // Pair with `step()`: compute the natural EVM cost for this opcode
        // and apply it to the active frame's per-opcode counter. The
        // baseline inspector applies no schedule deltas, so `gas_baseline`
        // and `gas_schedule` both receive the same value.
        let Some(gas_before) = self.gas_before_step.take() else { return };
        let mut actual_gas_cost = gas_before.saturating_sub(interp.gas.remaining());
        // For CALL/CREATE opcodes, `gas_before - gas_remaining_after`
        // includes the callee's net consumption (the EVM forwards gas
        // into the sub-frame, then refunds whatever it didn't burn).
        // The callee's own opcodes already accrued in its sub-frame
        // counters via step/step_end, so we subtract the callee's gas
        // here to leave only the intrinsic CALL/CREATE charge on the
        // outer opcode.
        let pending = std::mem::take(&mut self.pending_callee_gas_used);
        if pending > 0 && Self::is_call_or_create(self.current_opcode) {
            actual_gas_cost = actual_gas_cost.saturating_sub(pending);
        }
        self.record_opcode_in_active_frame(self.current_opcode, actual_gas_cost);
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = self.call_stack.len();

        let call_type = match inputs.scheme {
            revm::interpreter::CallScheme::Call => CallType::Call,
            revm::interpreter::CallScheme::CallCode => CallType::CallCode,
            revm::interpreter::CallScheme::DelegateCall => CallType::DelegateCall,
            revm::interpreter::CallScheme::StaticCall => CallType::StaticCall,
        };

        let function_selector = Self::extract_function_selector(&inputs.input);

        // Use `bytecode_address` (the contract whose code is executing) rather
        // than `target_address` (the storage context — which equals the caller
        // for DELEGATECALL). This matches `ScheduleInspector`, so call frames
        // from the two inspectors line up structurally for the
        // call-tree-changed comparison in the ExEx.
        self.call_stack.push(CallStackEntry {
            depth,
            from: inputs.caller,
            to: Some(inputs.bytecode_address),
            call_type,
            gas_provided: inputs.gas_limit,
            function_selector,
        });

        // Open a per-frame opcode counter for the sub-call (root was lazy-
        // init in `step()`).
        let frame_capture_idx = self.push_frame_counter();
        self.active_frame_stack.push(frame_capture_idx);

        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        // Pop the per-frame opcode counter stack to match `call()`'s push.
        self.active_frame_stack.pop();

        if let Some(entry) = self.call_stack.pop() {
            // Extract input bytes based on CallInput enum
            let input_bytes = match &inputs.input {
                revm::interpreter::CallInput::Bytes(bytes) => Some(bytes.clone()),
                revm::interpreter::CallInput::SharedBuffer(_) => None,
            };

            // Calculate gas used (gas_provided - gas_remaining)
            let gas_used = entry.gas_provided.saturating_sub(outcome.result.gas.remaining());
            // Hand the callee's net gas to the next step_end() so it can
            // subtract it from the parent CALL opcode's recorded cost.
            // Skipped for precompile calls (no step events fire in the
            // sub-frame, so the precompile's gas wouldn't get attributed
            // anywhere else — leave it under the outer CALL).
            if !outcome.was_precompile_called {
                self.pending_callee_gas_used = gas_used;
            }

            // Assign `call_index` at completion time to match
            // `ScheduleInspector` (sequential by completion order rather than
            // dispatch order). The two inspectors otherwise produce different
            // indices for nested calls, breaking the call-tree-changed
            // comparison even on identical executions.
            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: entry.from,
                to: entry.to,
                call_type: entry.call_type,
                gas_provided: entry.gas_provided,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: input_bytes,
                output: Some(outcome.result.output.clone()),
                repricing_gas_delta: 0,
                gas_requested_on_stack: None,
                parent_gas_at_call: None,
                value_wei: None,
                caller_pc: None,
                was_precompile: false,
                precompile_address: None,
                gas_remaining_at_fail: None,
                // F3: baseline frames don't carry the storage-target split; the
                // schedule trace is the forensic surface for the code_address.
                storage_target: None,
            });
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let depth = self.call_stack.len();

        let call_type = match inputs.scheme() {
            revm::context_interface::CreateScheme::Create => CallType::Create,
            revm::context_interface::CreateScheme::Create2 { .. } |
            revm::context_interface::CreateScheme::Custom { .. } => CallType::Create2,
        };

        self.call_stack.push(CallStackEntry {
            depth,
            from: inputs.caller(),
            to: None, // CREATE doesn't have a target address yet
            call_type,
            gas_provided: inputs.gas_limit(),
            function_selector: None, // CREATE operations don't have function selectors
        });

        // Open a per-frame opcode counter for the CREATE/CREATE2 sub-frame.
        let frame_capture_idx = self.push_frame_counter();
        self.active_frame_stack.push(frame_capture_idx);

        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        // Pop the per-frame opcode counter stack to match `create()`.
        self.active_frame_stack.pop();

        if let Some(entry) = self.call_stack.pop() {
            let created_address = outcome.address.unwrap_or(Address::ZERO);
            let gas_used = entry.gas_provided.saturating_sub(outcome.result.gas.remaining());
            // Same double-count fix as call_end: hand the callee's gas
            // to the next step_end() so the outer CREATE/CREATE2 opcode
            // gets credited only for its intrinsic charge.
            self.pending_callee_gas_used = gas_used;

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: entry.from,
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: entry.gas_provided,
                gas_used,
                success: outcome.result.result.is_ok(),
                input: Some(inputs.init_code().clone()),
                output: Some(outcome.result.output.clone()),
                repricing_gas_delta: 0,
                gas_requested_on_stack: None,
                parent_gas_at_call: None,
                value_wei: None,
                caller_pc: None,
                was_precompile: false,
                precompile_address: None,
                gas_remaining_at_fail: None,
                // F3: baseline frames don't carry the storage-target split; the
                // schedule trace is the forensic surface for the code_address.
                storage_target: None,
            });
        }
    }

    fn log(&mut self, _context: &mut CTX, log: alloy_primitives::Log) {
        self.event_logs.push(EventLog {
            log_index: self.event_logs.len(),
            address: log.address,
            topics: log.topics().to_vec(),
            data: log.data.data.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_capture_starts_empty() {
        let inspector = TrackingInspector::new();
        assert_eq!(inspector.frame_opcode_counts().frames.len(), 0);
        assert!(!inspector.frame_opcode_counts().truncated);
    }

    #[test]
    fn frame_capture_records_opcode_with_baseline_equals_schedule() {
        // The baseline inspector applies no deltas, so the two columns
        // should always agree.
        let mut inspector = TrackingInspector::new();
        inspector.ensure_root_frame_counter();
        inspector.record_opcode_in_active_frame(0x55, 20_000);
        let root = &inspector.frame_opcode_counts().frames[0];
        assert_eq!(root.counts[0x55], 1);
        assert_eq!(root.gas_baseline[0x55], 20_000);
        assert_eq!(root.gas_schedule[0x55], 20_000);
    }

    #[test]
    fn frame_capture_truncates_at_cap() {
        let mut inspector = TrackingInspector::new();
        inspector.ensure_root_frame_counter();
        for _ in 1..MAX_TRACKED_FRAMES {
            let _ = inspector.push_frame_counter();
        }
        assert!(!inspector.frame_opcode_counts().truncated);
        let overflow = inspector.push_frame_counter();
        assert_eq!(overflow, usize::MAX);
        assert!(inspector.frame_opcode_counts().truncated);
    }
}
