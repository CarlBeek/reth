//! Multi-schedule inspector for tracking gas deltas across multiple schedules.
//!
//! This inspector executes a transaction once while tracking what the gas usage
//! would be under each configured gas schedule.

use crate::{
    divergence::{
        CallFrame, CallType, DivergenceLocation, OogPattern, OperationCounts, OutOfGasInfo,
    },
    schedule::{GasSchedule, OpcodeContext},
};
use alloy_primitives::Address;
use revm::{
    context_interface::ContextTr,
    interpreter::{CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter},
    Inspector,
};
use revm_interpreter::interpreter_types::Jumps;
use std::{collections::VecDeque, sync::Arc};

/// Inspector that tracks gas usage across multiple schedules simultaneously.
///
/// Instead of re-executing the transaction for each schedule, this inspector
/// runs through the transaction once and calculates what the gas usage would
/// be under each schedule.
#[derive(Debug)]
pub struct MultiScheduleInspector {
    /// The schedules being tracked
    schedules: Vec<Arc<dyn GasSchedule>>,

    /// Per-schedule execution state
    schedule_states: Vec<ScheduleExecutionState>,

    /// Shared operation tracking
    op_counts: OperationCounts,

    /// Gas remaining before the current step
    gas_before_step: Option<u64>,

    /// Current opcode being executed
    current_opcode: u8,

    /// Call stack for tracking depth
    call_stack: Vec<CallStackEntry>,

    /// Recorded call frames
    call_frames: Vec<CallFrame>,

    /// Gas opcode usage for loop detection
    gas_opcode_usage: VecDeque<GasOpcodeEvent>,

    /// Maximum gas events to track
    max_gas_events: usize,

    /// Whether to detect gas-dependent loops
    detect_gas_loops: bool,
}

/// Execution state tracked per-schedule.
#[derive(Debug, Clone)]
pub struct ScheduleExecutionState {
    /// Schedule name
    pub schedule_name: String,

    /// Additional gas charged under this schedule
    pub additional_gas_charged: i64,

    /// Whether this schedule would have caused OOG
    pub oog_occurred: bool,

    /// Out-of-gas information (if OOG occurred)
    pub oog_info: Option<OutOfGasInfo>,

    /// Location where divergence would first occur
    pub divergence_location: Option<DivergenceLocation>,

    /// Whether this schedule has caused any divergence
    pub has_divergence: bool,
}

impl ScheduleExecutionState {
    /// Create a new schedule execution state.
    pub fn new(schedule_name: String) -> Self {
        Self {
            schedule_name,
            additional_gas_charged: 0,
            oog_occurred: false,
            oog_info: None,
            divergence_location: None,
            has_divergence: false,
        }
    }
}

/// Result of executing under a schedule.
#[derive(Debug, Clone)]
pub struct ScheduleResult {
    /// Schedule name
    pub schedule_name: String,

    /// Additional gas that would be charged
    pub additional_gas: i64,

    /// Whether OOG would occur
    pub would_oog: bool,

    /// OOG information if applicable
    pub oog_info: Option<OutOfGasInfo>,

    /// Divergence location if applicable
    pub divergence_location: Option<DivergenceLocation>,
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

/// Gas opcode event for loop detection.
#[derive(Debug, Clone)]
struct GasOpcodeEvent {
    pc: usize,
    #[allow(dead_code)]
    gas_remaining: u64,
    #[allow(dead_code)]
    contract: Address,
}

impl MultiScheduleInspector {
    /// Create a new multi-schedule inspector.
    pub fn new(schedules: Vec<Arc<dyn GasSchedule>>) -> Self {
        let schedule_states = schedules
            .iter()
            .map(|s| ScheduleExecutionState::new(s.name().to_string()))
            .collect();

        Self {
            schedules,
            schedule_states,
            op_counts: OperationCounts::default(),
            gas_before_step: None,
            current_opcode: 0,
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            gas_opcode_usage: VecDeque::new(),
            max_gas_events: 1000,
            detect_gas_loops: true,
        }
    }

    /// Enable or disable gas loop detection.
    pub fn with_gas_loop_detection(mut self, enabled: bool) -> Self {
        self.detect_gas_loops = enabled;
        self
    }

    /// Get the operation counts.
    pub fn operation_counts(&self) -> &OperationCounts {
        &self.op_counts
    }

    /// Get the call frames.
    pub fn call_frames(&self) -> &[CallFrame] {
        &self.call_frames
    }

    /// Get results for all schedules.
    pub fn results(&self) -> Vec<ScheduleResult> {
        self.schedule_states
            .iter()
            .map(|state| ScheduleResult {
                schedule_name: state.schedule_name.clone(),
                additional_gas: state.additional_gas_charged,
                would_oog: state.oog_occurred,
                oog_info: state.oog_info.clone(),
                divergence_location: state.divergence_location.clone(),
            })
            .collect()
    }

    /// Get result for a specific schedule by name.
    pub fn result_for(&self, schedule_name: &str) -> Option<ScheduleResult> {
        self.schedule_states.iter().find(|s| s.schedule_name == schedule_name).map(|state| {
            ScheduleResult {
                schedule_name: state.schedule_name.clone(),
                additional_gas: state.additional_gas_charged,
                would_oog: state.oog_occurred,
                oog_info: state.oog_info.clone(),
                divergence_location: state.divergence_location.clone(),
            }
        })
    }

    /// Get schedules that would cause OOG.
    pub fn oog_schedules(&self) -> Vec<&str> {
        self.schedule_states
            .iter()
            .filter(|s| s.oog_occurred)
            .map(|s| s.schedule_name.as_str())
            .collect()
    }

    /// Get schedules that would NOT cause OOG but have gas changes.
    pub fn modified_schedules(&self) -> Vec<&str> {
        self.schedule_states
            .iter()
            .filter(|s| !s.oog_occurred && s.additional_gas_charged != 0)
            .map(|s| s.schedule_name.as_str())
            .collect()
    }

    /// Check if a potential gas-dependent loop is detected.
    pub fn has_gas_loop_pattern(&self) -> bool {
        if self.gas_opcode_usage.len() < 3 {
            return false;
        }

        let mut pc_counts = std::collections::HashMap::new();
        for event in &self.gas_opcode_usage {
            *pc_counts.entry(event.pc).or_insert(0) += 1;
        }

        pc_counts.values().any(|&count| count >= 3)
    }

    /// Extract function selector from call input.
    fn extract_function_selector(input: &revm::interpreter::CallInput) -> Option<[u8; 4]> {
        match input {
            revm::interpreter::CallInput::Bytes(bytes) if bytes.len() >= 4 => {
                Some([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            _ => None,
        }
    }

    /// Build opcode context for schedule gas calculations.
    fn build_opcode_context(
        &self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
    ) -> OpcodeContext {
        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);

        OpcodeContext {
            contract,
            pc: interp.bytecode.pc(),
            call_depth: self.call_stack.len(),
            gas_remaining: interp.gas.remaining(),
            memory_size: interp.memory.len(),
            keccak_msg_size: None, // Would need stack inspection for accurate value
            exp_byte_size: None,
            memory_offset: None,
            memory_access_size: None,
        }
    }

    /// Record divergence location for a schedule.
    fn record_schedule_divergence(
        &mut self,
        schedule_idx: usize,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        opcode: u8,
    ) {
        let state = &mut self.schedule_states[schedule_idx];
        if state.divergence_location.is_some() {
            return;
        }

        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);
        let function_selectors: Vec<Option<[u8; 4]>> =
            self.call_stack.iter().map(|entry| entry.function_selector).collect();

        state.divergence_location = Some(DivergenceLocation {
            contract,
            function_selectors,
            pc: interp.bytecode.pc(),
            call_depth: self.call_stack.len(),
            opcode,
            opcode_name: format!("0x{:02x}", opcode),
        });
        state.has_divergence = true;
    }

    /// Record OOG for a schedule.
    fn record_schedule_oog(
        &mut self,
        schedule_idx: usize,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        opcode: u8,
    ) {
        if self.schedule_states[schedule_idx].oog_info.is_some() {
            return;
        }

        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);
        let pattern = self.infer_oog_pattern(opcode);
        let call_depth = self.call_stack.len();

        let state = &mut self.schedule_states[schedule_idx];
        state.oog_info = Some(OutOfGasInfo {
            opcode,
            opcode_name: format!("0x{:02x}", opcode),
            pc: interp.bytecode.pc(),
            contract,
            call_depth,
            gas_remaining: interp.gas.remaining(),
            pattern,
        });
        state.oog_occurred = true;
    }

    /// Infer OOG pattern based on opcode.
    fn infer_oog_pattern(&self, opcode: u8) -> OogPattern {
        match opcode {
            0x54 | 0x55 => OogPattern::StorageHeavy,             // SLOAD, SSTORE
            0xF1 | 0xF2 | 0xF4 | 0xFA => OogPattern::CallChain, // CALL variants
            0x51 | 0x52 | 0x53 => OogPattern::MemoryExpansion,  // MLOAD, MSTORE, MSTORE8
            _ if self.has_gas_loop_pattern() => OogPattern::Loop,
            _ => OogPattern::Unknown,
        }
    }

    /// Track GAS opcode usage for loop detection.
    fn track_gas_opcode(
        &mut self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
    ) {
        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);

        self.gas_opcode_usage.push_back(GasOpcodeEvent {
            pc: interp.bytecode.pc(),
            gas_remaining: interp.gas.remaining(),
            contract,
        });

        while self.gas_opcode_usage.len() > self.max_gas_events {
            self.gas_opcode_usage.pop_front();
        }
    }
}

impl<CTX> Inspector<CTX, revm::interpreter::interpreter::EthInterpreter> for MultiScheduleInspector
where
    CTX: ContextTr,
{
    fn step(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        // Record gas before step
        self.gas_before_step = Some(interp.gas.remaining());
        self.current_opcode = interp.bytecode.opcode();

        // Track operation counts
        self.op_counts.total_ops += 1;

        match self.current_opcode {
            0x54 => self.op_counts.sload_count += 1,
            0x55 => self.op_counts.sstore_count += 1,
            0xA0..=0xA4 => self.op_counts.log_count += 1,
            0xF1 | 0xF2 | 0xF4 | 0xFA => self.op_counts.call_count += 1,
            0xF0 | 0xF5 => self.op_counts.create_count += 1,
            0x5A if self.detect_gas_loops => self.track_gas_opcode(interp),
            _ => {}
        }

        // Track memory usage
        let memory_words = (interp.memory.len() + 31) / 32;
        if memory_words as u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words as u64;
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        let Some(gas_before) = self.gas_before_step else {
            return;
        };

        let gas_after = interp.gas.remaining();
        let _actual_cost = gas_before.saturating_sub(gas_after);
        let current_opcode = self.current_opcode;

        // Build opcode context for schedule calculations
        let opcode_ctx = self.build_opcode_context(interp);

        // First pass: collect gas deltas for each schedule (read-only on schedules)
        let schedule_deltas: Vec<(usize, i64, bool, bool)> = self
            .schedules
            .iter()
            .enumerate()
            .filter_map(|(idx, schedule)| {
                // Skip if this schedule already OOG'd
                if self.schedule_states[idx].oog_occurred {
                    return None;
                }

                // Only check execution-modifying schedules
                if !schedule.modifies_execution() {
                    return None;
                }

                // Get gas delta from this schedule
                let gas_delta = schedule.opcode_gas_delta(current_opcode, &opcode_ctx);

                if gas_delta != 0 {
                    let would_oog = gas_delta > 0 && gas_delta as u64 > gas_after;
                    let needs_divergence =
                        self.schedule_states[idx].divergence_location.is_none();
                    Some((idx, gas_delta, would_oog, needs_divergence))
                } else {
                    None
                }
            })
            .collect();

        // Second pass: apply mutations
        for (idx, gas_delta, would_oog, needs_divergence) in schedule_deltas {
            self.schedule_states[idx].additional_gas_charged += gas_delta;

            if would_oog {
                self.record_schedule_oog(idx, interp, current_opcode);
                self.record_schedule_divergence(idx, interp, current_opcode);
            } else if needs_divergence {
                self.record_schedule_divergence(idx, interp, current_opcode);
            }
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
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
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector,
        });

        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if let Some(entry) = self.call_stack.pop() {
            let input_bytes = match &inputs.input {
                revm::interpreter::CallInput::Bytes(bytes) => Some(bytes.clone()),
                revm::interpreter::CallInput::SharedBuffer(_) => None,
            };

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller,
                to: Some(inputs.bytecode_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit,
                gas_used: entry.gas_at_start.saturating_sub(outcome.result.gas.remaining()),
                success: outcome.result.result.is_ok(),
                input: input_bytes,
                output: Some(outcome.result.output.clone()),
            });
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: Address::ZERO,
            call_type: match inputs.scheme() {
                revm::context_interface::CreateScheme::Create => CallType::Create,
                revm::context_interface::CreateScheme::Create2 { .. } |
                revm::context_interface::CreateScheme::Custom { .. } => CallType::Create2,
            },
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector: None,
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

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller(),
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit(),
                gas_used: entry.gas_at_start.saturating_sub(outcome.result.gas.remaining()),
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
    use crate::schedule::{BaselineSchedule, CsvPricingSchedule, MultiplierSchedule};

    #[test]
    fn test_multi_schedule_inspector_new() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![
            Arc::new(BaselineSchedule),
            Arc::new(MultiplierSchedule::with_multiplier(128)),
        ];

        let inspector = MultiScheduleInspector::new(schedules);
        assert_eq!(inspector.schedules.len(), 2);
        assert_eq!(inspector.schedule_states.len(), 2);
        assert_eq!(inspector.schedule_states[0].schedule_name, "baseline");
        assert_eq!(inspector.schedule_states[1].schedule_name, "128x");
    }

    #[test]
    fn test_multi_schedule_results() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![
            Arc::new(BaselineSchedule),
            Arc::new(MultiplierSchedule::with_multiplier(128)),
        ];

        let inspector = MultiScheduleInspector::new(schedules);
        let results = inspector.results();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].schedule_name, "baseline");
        assert_eq!(results[0].additional_gas, 0);
        assert!(!results[0].would_oog);
    }

    #[test]
    fn test_result_for_schedule() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![
            Arc::new(BaselineSchedule),
            Arc::new(MultiplierSchedule::with_multiplier(128)),
        ];

        let inspector = MultiScheduleInspector::new(schedules);

        let baseline_result = inspector.result_for("baseline");
        assert!(baseline_result.is_some());
        assert_eq!(baseline_result.unwrap().schedule_name, "baseline");

        let missing = inspector.result_for("nonexistent");
        assert!(missing.is_none());
    }

    #[test]
    fn test_csv_schedule_integration() {
        let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
"#;
        let csv_schedule =
            CsvPricingSchedule::from_csv("test-csv".to_string(), csv_data.as_bytes()).unwrap();

        let schedules: Vec<Arc<dyn GasSchedule>> =
            vec![Arc::new(BaselineSchedule), Arc::new(csv_schedule)];

        let inspector = MultiScheduleInspector::new(schedules);
        assert_eq!(inspector.schedules.len(), 2);

        let result = inspector.result_for("test-csv");
        assert!(result.is_some());
    }

    #[test]
    fn test_operation_counts_initial() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![Arc::new(BaselineSchedule)];
        let inspector = MultiScheduleInspector::new(schedules);

        assert_eq!(inspector.operation_counts().total_ops, 0);
        assert_eq!(inspector.operation_counts().sload_count, 0);
    }

    #[test]
    fn test_oog_schedules_empty() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![Arc::new(BaselineSchedule)];
        let inspector = MultiScheduleInspector::new(schedules);

        assert!(inspector.oog_schedules().is_empty());
    }

    #[test]
    fn test_gas_loop_detection_disabled() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![Arc::new(BaselineSchedule)];
        let inspector = MultiScheduleInspector::new(schedules).with_gas_loop_detection(false);

        assert!(!inspector.detect_gas_loops);
    }

    #[test]
    fn test_schedule_execution_state_new() {
        let state = ScheduleExecutionState::new("test".to_string());
        assert_eq!(state.schedule_name, "test");
        assert_eq!(state.additional_gas_charged, 0);
        assert!(!state.oog_occurred);
        assert!(state.oog_info.is_none());
        assert!(state.divergence_location.is_none());
        assert!(!state.has_divergence);
    }

    #[test]
    fn test_modified_schedules_empty() {
        let schedules: Vec<Arc<dyn GasSchedule>> = vec![Arc::new(BaselineSchedule)];
        let inspector = MultiScheduleInspector::new(schedules);

        assert!(inspector.modified_schedules().is_empty());
    }
}
