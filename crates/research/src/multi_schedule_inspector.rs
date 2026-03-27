//! Schedule inspector that applies gas cost modifications during execution.
//!
//! This inspector re-executes a transaction under a given gas schedule by actually
//! modifying gas charges via `interp.gas.record_cost()` / `erase_cost()`. Because
//! the EVM sees the real modified gas, subcall gas forwarding is handled naturally —
//! if a caller burns more gas before a CALL, the subcall receives less gas, and may
//! OOG where it previously succeeded.
//!
//! For CALL/CREATE opcodes specifically, the gas delta is applied in the `step()`
//! hook (before the opcode executes and dispatches the subcall), so the modified gas
//! balance feeds into the 63/64 forwarding rule. For all other opcodes the delta is
//! applied in `step_end()` (after the opcode has executed).
//!
//! # Performance cost
//!
//! Each execution-modifying schedule requires its own full EVM execution pass,
//! resulting in N+1 total executions per transaction (1 baseline + N schedules).
//! Each schedule also needs its own state database to track cascading effects
//! across transactions within a block. This is acceptable for a small number of
//! research schedules but will degrade linearly with schedule count.
//!
//! # Execution model (revm frame-based loop)
//!
//! revm's `inspect_instructions` loop calls `step()` → execute → `step_end()` for
//! every opcode within a single frame. When CALL/CREATE executes it sets a
//! `NewFrame` action and marks the bytecode as ended; `step_end()` still fires for
//! that same opcode in the same loop iteration, *before* the subcall frame is
//! created. The subcall then runs in its own `frame_run()` invocation with its own
//! `inspect_instructions` loop, so step/step_end pairs from the subcall never
//! interleave with the parent's CALL opcode. This is why a flat
//! `call_delta_pre_applied` flag (rather than a per-frame stack) is sufficient.
//!
//! # CALL/CREATE delta semantics
//!
//! The gas delta for CALL/CREATE opcodes is applied in `step()` *before* the EVM
//! charges its own base cost for the opcode. This is safe because
//! [`GasSchedule::opcode_gas_delta`] returns an *additive* delta (new_cost -
//! current_cost), not an absolute replacement. The EVM still charges its own base
//! cost separately. The total gas consumed is the same regardless of deduction
//! order, but the intermediate `remaining` value between the inspector's charge and
//! the EVM's charge differs. This intermediate value matters: if `record_cost(delta)`
//! succeeds but leaves `remaining` so low that the EVM's own CALL base-cost check
//! then fails, the EVM will halt with OOG. The inspector detects this via indirect
//! OOG detection in `call_end()` / `create_end()`, including the caller's PC from
//! the call stack for diagnostics. Schedule authors must ensure their deltas
//! represent the cost *difference*, not the total new cost; see
//! [`GasSchedule::opcode_gas_delta`] docs.
//!
//! # Asymmetry in cost increase vs decrease propagation
//!
//! Gas *increases* propagate fully through subcalls — `record_cost()` can drive the
//! frame to OOG. Gas *decreases* (refunds via `erase_cost()`) are capped at the
//! frame's gas limit to avoid arithmetic overflow, since `erase_cost()` performs
//! unchecked addition on `remaining`. This means a schedule that makes opcodes
//! cheaper will not grant the caller more gas to forward than the frame originally
//! had. The effect is conservative: cost savings are bounded by the frame limit.
//!
//! # Precompile gas deltas
//!
//! Precompile calls bypass the interpreter loop (no `step()`/`step_end()` hooks fire), so
//! precompile gas deltas are applied in `call_end()` when `outcome.was_precompile_called` is
//! true. The delta is computed via [`GasSchedule::precompile_gas_delta`] using the precompile
//! address and input data. For cost increases, the delta is charged from the precompile frame's
//! remaining gas; if insufficient, OOG is flagged. For cost decreases, a capped refund is
//! applied (same asymmetry as opcode refunds).
//!
//! # Variable-cost opcode support
//!
//! For KECCAK256 and EXP, the inspector reads stack operands in `step()` (before execution
//! consumes them) to populate `OpcodeContext::keccak_msg_size` and `exp_byte_size`. This
//! allows schedule implementations to compute accurate variable-component deltas (e.g.,
//! per-word cost for KECCAK256, per-byte cost for EXP).
//!
//! # OOG diagnostics
//!
//! OOG is detected in two ways:
//! 1. **Direct**: `apply_gas_delta` drives `remaining` below 0, recorded immediately with full
//!    diagnostics (opcode, contract, call depth, pattern).
//! 2. **Indirect**: Earlier deltas reduce gas enough that the EVM's own handler (e.g., CALL base
//!    cost check) triggers OOG. The inspector detects this in `call_end()` / `create_end()` by
//!    matching the outcome against OOG-specific `InstructionResult` variants (`OutOfGas`,
//!    `MemoryOOG`, `PrecompileOOG`, `MemoryLimitOOG`, `InvalidOperandOOG`, `ReentrancySentryOOG`)
//!    when at least one positive gas delta was applied within the frame's subtree — the frame
//!    itself, any ancestor, or any descendant (`any_positive_delta_in_subtree`). The flag is
//!    inherited from parent to child on frame entry and propagated back from child to parent on
//!    frame exit. This tracks gas pressure per call subtree: if the caller applied positive deltas
//!    that reduced forwarded gas, the callee inherits that context. But an unrelated call tree
//!    without positive deltas won't false-positive. Non-gas errors (`StackOverflow`,
//!    `InvalidFEOpcode`, etc.) and intentional reverts (`REVERT`, `OutOfFunds`) are excluded.
//!    Diagnostics include the actual CALL/CREATE opcode variant and the caller's program counter
//!    (captured in `step()`) so the dispatching instruction can be located in the caller's
//!    bytecode.

use crate::{
    divergence::{
        CallFrame, CallType, DivergenceLocation, EventLog, OogPattern, OperationCounts,
        OutOfGasInfo,
    },
    schedule::{GasSchedule, OpcodeContext},
};
use alloy_primitives::Address;
use revm::{
    bytecode::opcode::OpCode,
    context_interface::ContextTr,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
    },
    Inspector,
};
use revm_interpreter::interpreter_types::Jumps;
use std::{collections::VecDeque, sync::Arc};

/// Whether the instruction result is an OOG-class error (as opposed to a
/// revert, stack error, or other non-gas failure). Used for indirect OOG
/// detection in `call_end()` / `create_end()` to avoid false positives from
/// errors that gas changes cannot cause.
///
/// NOTE: if revm adds new OOG-class `InstructionResult` variants, this
/// function must be updated to include them. Check revm's `InstructionResult`
/// enum after dependency upgrades.
fn is_oog_error(result: InstructionResult) -> bool {
    matches!(
        result,
        InstructionResult::OutOfGas |
            InstructionResult::MemoryOOG |
            InstructionResult::MemoryLimitOOG |
            InstructionResult::PrecompileOOG |
            InstructionResult::InvalidOperandOOG |
            InstructionResult::ReentrancySentryOOG
    )
}

/// Inspector that applies a single gas schedule's cost modifications during execution.
///
/// Unlike a read-only delta tracker, this inspector actually charges/refunds gas so
/// that the EVM naturally propagates cost changes through subcalls. Each execution-
/// modifying schedule requires its own execution pass with this inspector.
#[derive(Debug)]
pub struct ScheduleInspector {
    /// The schedule being applied
    schedule: Arc<dyn GasSchedule>,

    /// Operation tracking
    op_counts: OperationCounts,

    /// Gas remaining before the current step
    gas_before_step: Option<u64>,

    /// Current opcode being executed
    current_opcode: u8,

    /// Program counter of the current opcode (captured in `step()`).
    /// Used to populate `caller_pc` in call stack entries for indirect OOG
    /// diagnostics, since the `call()` / `create()` hooks don't receive the
    /// interpreter.
    current_pc: usize,

    /// Cached variable-cost opcode parameters extracted from the stack in
    /// `step()` (before execution consumes them). These are used when building
    /// the `OpcodeContext` in `step_end()`.
    cached_keccak_msg_size: Option<usize>,
    cached_exp_byte_size: Option<usize>,

    /// Call stack for tracking depth
    call_stack: Vec<CallStackEntry>,

    /// Recorded call frames
    call_frames: Vec<CallFrame>,

    /// Event logs captured during execution.
    event_logs: Vec<EventLog>,

    /// Cumulative additional gas charged (for reporting)
    additional_gas_charged: i64,

    /// Whether any individual positive gas delta was ever applied. Used for
    /// indirect OOG detection: a subcall OOG is only attributed to the schedule
    /// if at least one positive delta was charged. This avoids false positives
    /// from schedules that only *decrease* costs (where `additional_gas_charged`
    /// could be negative even though individual positive deltas existed).
    any_positive_delta_applied: bool,

    /// Whether OOG was triggered by the schedule's gas modifications
    oog_occurred: bool,

    /// OOG diagnostic info (first OOG only)
    oog_info: Option<OutOfGasInfo>,

    /// Location of first gas divergence
    divergence_location: Option<DivergenceLocation>,

    /// Whether a CALL/CREATE delta was pre-applied in `step()` and should be
    /// skipped in `step_end()`.
    ///
    /// Safe as a flat flag (not a per-frame stack) because revm's
    /// `inspect_instructions` calls step → execute → step_end within a single
    /// loop iteration; the subcall frame runs in a separate `frame_run()`
    /// invocation, so no interleaving occurs. A debug assertion in `step()`
    /// validates that the flag is always reset before each opcode.
    call_delta_pre_applied: bool,

    /// Gas opcode usage for loop detection
    gas_opcode_usage: VecDeque<GasOpcodeEvent>,

    /// Maximum gas events to track
    max_gas_events: usize,

    /// Whether to detect gas-dependent loops
    detect_gas_loops: bool,
}

/// Result of executing under a schedule, as tracked by the inspector.
///
/// These values reflect what the inspector observed during execution. Due to
/// cascading effects (different execution paths, subcall OOGs), the actual gas
/// difference between baseline and schedule execution may differ from
/// `additional_gas`. Callers should prefer comparing actual `gas_used` from the
/// EVM result over relying on `additional_gas` for the final delta.
#[derive(Debug, Clone)]
pub struct ScheduleResult {
    /// Schedule name
    pub schedule_name: String,

    /// Cumulative gas delta applied by the inspector via `record_cost`/`erase_cost`.
    ///
    /// This is the sum of per-opcode deltas the inspector charged. It may differ
    /// from the actual `gas_used` difference between baseline and schedule execution
    /// because cascading effects (subcall OOGs, different execution paths) cause
    /// the real gas consumption to diverge from the per-opcode sum.
    pub additional_gas: i64,

    /// Whether the execution OOG'd due to the schedule's gas modifications.
    ///
    /// This is `true` when either:
    /// - `apply_gas_delta` directly triggered OOG via `record_cost`, or
    /// - A subcall/create hit an OOG-specific error variant (`OutOfGas`, `MemoryOOG`,
    ///   `PrecompileOOG`, etc.) and the inspector had charged additional gas, indicating the EVM's
    ///   own OOG was likely caused by the schedule's changes. Non-gas errors and intentional
    ///   reverts are excluded to avoid false positives.
    pub would_oog: bool,

    /// OOG diagnostic information (first OOG occurrence only).
    pub oog_info: Option<OutOfGasInfo>,

    /// Location of first gas divergence.
    pub divergence_location: Option<DivergenceLocation>,
}

/// Entry in the call stack.
#[derive(Debug, Clone)]
struct CallStackEntry {
    depth: usize,
    contract: Address,
    call_type: CallType,
    /// The opcode that initiated this call (0xF1 for CALL, 0xFA for STATICCALL, etc.)
    opcode: u8,
    /// Program counter of the CALL/CREATE opcode in the caller's bytecode.
    /// Used for indirect OOG diagnostics so we can report where the subcall
    /// was dispatched from.
    caller_pc: usize,
    gas_at_start: u64,
    function_selector: Option<[u8; 4]>,
    /// Whether any positive gas delta was applied in this frame, any ancestor,
    /// or any descendant. This flag is inherited from the parent on frame entry
    /// (so if the caller had positive deltas that reduced forwarded gas, the
    /// callee inherits that context) and propagated back to the parent on frame
    /// exit. This provides precise per-subtree tracking: a positive delta at
    /// depth 0 correctly propagates to a callee OOG at depth 1, but an
    /// unrelated call tree without positive deltas won't false-positive.
    any_positive_delta_in_subtree: bool,
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

impl ScheduleInspector {
    /// Create a new schedule inspector for a single gas schedule.
    pub fn new(schedule: Arc<dyn GasSchedule>) -> Self {
        Self {
            schedule,
            op_counts: OperationCounts::default(),
            gas_before_step: None,
            current_opcode: 0,
            current_pc: 0,
            cached_keccak_msg_size: None,
            cached_exp_byte_size: None,
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            event_logs: Vec::new(),
            additional_gas_charged: 0,
            any_positive_delta_applied: false,
            oog_occurred: false,
            oog_info: None,
            divergence_location: None,
            call_delta_pre_applied: false,
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

    /// Get the event logs captured during execution.
    pub fn event_logs(&self) -> &[EventLog] {
        &self.event_logs
    }

    /// Get the schedule name.
    pub fn schedule_name(&self) -> &str {
        self.schedule.name()
    }

    /// Get the result after execution.
    pub fn result(&self) -> ScheduleResult {
        ScheduleResult {
            schedule_name: self.schedule.name().to_string(),
            additional_gas: self.additional_gas_charged,
            would_oog: self.oog_occurred,
            oog_info: self.oog_info.clone(),
            divergence_location: self.divergence_location.clone(),
        }
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

    /// Record OOG diagnostic info (first occurrence only).
    fn record_oog(
        &mut self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        opcode: u8,
    ) {
        if self.oog_info.is_some() {
            return;
        }

        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);
        let pattern = self.infer_oog_pattern(opcode);

        self.oog_info = Some(OutOfGasInfo {
            opcode,
            opcode_name: Self::opcode_name(opcode),
            pc: interp.bytecode.pc(),
            contract,
            call_depth: self.call_stack.len(),
            gas_remaining: interp.gas.remaining(),
            pattern,
        });
        self.oog_occurred = true;
    }

    /// Human-readable opcode name for diagnostics.
    fn opcode_name(opcode: u8) -> String {
        OpCode::new(opcode).map_or_else(|| format!("0x{opcode:02x}"), |op| op.as_str().to_string())
    }

    /// Record divergence location (first occurrence only).
    fn record_divergence(
        &mut self,
        interp: &Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        opcode: u8,
    ) {
        if self.divergence_location.is_some() {
            return;
        }

        let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);
        let function_selectors: Vec<Option<[u8; 4]>> =
            self.call_stack.iter().map(|entry| entry.function_selector).collect();

        self.divergence_location = Some(DivergenceLocation {
            contract,
            function_selectors,
            pc: interp.bytecode.pc(),
            call_depth: self.call_stack.len(),
            opcode,
            opcode_name: format!("0x{:02x}", opcode),
        });
    }

    /// Infer OOG pattern based on opcode.
    fn infer_oog_pattern(&self, opcode: u8) -> OogPattern {
        match opcode {
            0x54 | 0x55 => OogPattern::StorageHeavy, // SLOAD, SSTORE
            0xF1 | 0xF2 | 0xF4 | 0xFA => OogPattern::CallChain, // CALL variants
            0x51 | 0x52 | 0x53 => OogPattern::MemoryExpansion, // MLOAD, MSTORE, MSTORE8
            _ if self.has_gas_loop_pattern() => OogPattern::Loop,
            _ => OogPattern::Unknown,
        }
    }

    /// Build opcode context for schedule gas calculations.
    ///
    /// Note: when called from `step()` (for CALL/CREATE), `gas_remaining` is the
    /// pre-execution value (before the EVM charges the opcode's base cost). When
    /// called from `step_end()` (all other opcodes), `gas_remaining` is the
    /// post-execution value. Schedule implementations should not rely on
    /// `gas_remaining` for delta calculation — use opcode identity, memory size,
    /// etc. instead.
    ///
    /// For variable-cost opcodes (KECCAK256, EXP), the context uses values
    /// cached in `step()` (before execution consumes them from the stack) rather
    /// than reading the stack here, since `step_end()` fires after execution.
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
            keccak_msg_size: self.cached_keccak_msg_size,
            exp_byte_size: self.cached_exp_byte_size,
            memory_offset: None,
            memory_access_size: None,
        }
    }

    /// Whether this opcode dispatches a subcall whose gas allocation depends on
    /// the caller's remaining gas (CALL, CALLCODE, DELEGATECALL, STATICCALL,
    /// CREATE, CREATE2).
    fn is_call_or_create(opcode: u8) -> bool {
        matches!(opcode, 0xF0 | 0xF1 | 0xF2 | 0xF4 | 0xF5 | 0xFA)
    }

    /// Record a gas delta against the per-opcode counters in OperationCounts.
    fn record_opcode_gas_delta(op_counts: &mut OperationCounts, opcode: u8, delta: i64) {
        match opcode {
            0x04 => op_counts.div_gas_delta += delta,
            0x05 => op_counts.sdiv_gas_delta += delta,
            0x06 => op_counts.mod_gas_delta += delta,
            0x07 => op_counts.smod_gas_delta += delta,
            0x08 => op_counts.addmod_gas_delta += delta,
            0x09 => op_counts.mulmod_gas_delta += delta,
            0x0A => op_counts.exp_gas_delta += delta,
            0x20 => op_counts.keccak256_gas_delta += delta,
            _ => {}
        }
    }

    /// Apply a gas delta to the interpreter, recording divergence/OOG as needed.
    ///
    /// Returns `true` if execution should continue, `false` if OOG halted the
    /// interpreter.
    fn apply_gas_delta(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        gas_delta: i64,
        opcode: u8,
    ) -> bool {
        self.record_divergence(interp, opcode);
        self.additional_gas_charged += gas_delta;

        if gas_delta > 0 {
            self.any_positive_delta_applied = true;
            if let Some(frame) = self.call_stack.last_mut() {
                frame.any_positive_delta_in_subtree = true;
            }
            if !interp.gas.record_cost(gas_delta as u64) {
                self.record_oog(interp, opcode);
                interp.halt_oog();
                return false;
            }
        } else {
            // Refund gas (schedule is cheaper for this opcode).
            //
            // Cap the refund so `remaining` never exceeds the frame's gas limit,
            // since `erase_cost()` does unchecked addition on `remaining`. This
            // creates a deliberate asymmetry: cost increases propagate fully
            // through subcall gas forwarding, but cost decreases are bounded by
            // the frame's allocation. The effect is conservative — savings cannot
            // inflate a frame beyond its original budget.
            let refund = (-gas_delta) as u64;
            let headroom = interp.gas.limit().saturating_sub(interp.gas.remaining());
            interp.gas.erase_cost(refund.min(headroom));
        }
        true
    }

    /// Compute the gas delta implied by a uniform multiplier over the EVM's
    /// observed native gas cost for the current opcode.
    fn multiplier_gas_delta(&self, actual_gas_cost: u64) -> i64 {
        let Some(multiplier) = self.schedule.execution_gas_multiplier() else {
            return 0;
        };

        if multiplier <= 1 {
            return 0;
        }

        actual_gas_cost.saturating_mul(multiplier - 1) as i64
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

impl<CTX> Inspector<CTX, revm::interpreter::interpreter::EthInterpreter> for ScheduleInspector
where
    CTX: ContextTr,
{
    fn step(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        // Once OOG has been triggered, stop applying further deltas. The
        // interpreter should be halted, but guard against revm calling step()
        // on the parent frame after a subcall OOG.
        if self.oog_occurred {
            return;
        }

        self.gas_before_step = Some(interp.gas.remaining());
        self.current_opcode = interp.bytecode.opcode();
        self.current_pc = interp.bytecode.pc();
        // Reset for this opcode. The flag may be true here if a parent frame's
        // CALL set it and then the subcall frame started — this is expected.
        // The invariant is that step_end() of the *same* frame respects the
        // flag set by step() of the *same* opcode, which holds because revm
        // calls step → execute → step_end within a single loop iteration.
        self.call_delta_pre_applied = false;

        // Cache variable-cost opcode parameters from the stack before execution
        // consumes them. These are used in build_opcode_context() during step_end().
        self.cached_keccak_msg_size = if self.current_opcode == 0x20 {
            // KECCAK256: stack[0] = offset, stack[1] = size
            interp.stack.peek(1).ok().map(|size| size.saturating_to::<usize>())
        } else {
            None
        };
        self.cached_exp_byte_size = if self.current_opcode == 0x0A {
            // EXP: stack[0] = base, stack[1] = exponent
            interp.stack.peek(1).ok().map(|exp| {
                if exp.is_zero() {
                    0usize
                } else {
                    (exp.bit_len() + 7) / 8
                }
            })
        } else {
            None
        };

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

        // Count repriced opcodes
        match self.current_opcode {
            0x04 => self.op_counts.div_count += 1,
            0x05 => self.op_counts.sdiv_count += 1,
            0x06 => self.op_counts.mod_count += 1,
            0x07 => self.op_counts.smod_count += 1,
            0x08 => self.op_counts.addmod_count += 1,
            0x09 => self.op_counts.mulmod_count += 1,
            0x0A => self.op_counts.exp_count += 1,
            0x20 => self.op_counts.keccak256_count += 1,
            _ => {}
        }

        // Track memory usage
        let memory_words = (interp.memory.len() + 31) / 32;
        if memory_words as u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words as u64;
        }

        // For CALL/CREATE opcodes, apply the gas delta BEFORE execution so it
        // feeds into the 63/64 gas forwarding rule for the subcall.
        if Self::is_call_or_create(self.current_opcode) {
            let opcode_ctx = self.build_opcode_context(interp);
            let gas_delta = self.schedule.opcode_gas_delta(self.current_opcode, &opcode_ctx);
            if gas_delta != 0 {
                self.call_delta_pre_applied = true;
                Self::record_opcode_gas_delta(&mut self.op_counts, self.current_opcode, gas_delta);
                if !self.apply_gas_delta(interp, gas_delta, self.current_opcode) {
                    return; // OOG — interpreter is halted, don't continue
                }
            }
        }
    }

    fn step_end(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        _context: &mut CTX,
    ) {
        if self.oog_occurred || self.gas_before_step.is_none() {
            return;
        }

        let current_opcode = self.current_opcode;
        let gas_before = self.gas_before_step.expect("checked above");
        let actual_gas_cost = gas_before.saturating_sub(interp.gas.remaining());

        let opcode_ctx = self.build_opcode_context(interp);
        let explicit_gas_delta = if self.call_delta_pre_applied {
            0
        } else {
            self.schedule.opcode_gas_delta(current_opcode, &opcode_ctx)
        };
        // Uniform multipliers are derived from the EVM's observed native cost
        // after the opcode has executed. That makes them work for the live
        // schedule path, but unlike explicit additive deltas they do not feed
        // into CALL/CREATE gas forwarding before dispatch.
        let multiplier_gas_delta = self.multiplier_gas_delta(actual_gas_cost);
        let gas_delta = explicit_gas_delta.saturating_add(multiplier_gas_delta);

        if gas_delta != 0 {
            Self::record_opcode_gas_delta(&mut self.op_counts, current_opcode, gas_delta);
            self.apply_gas_delta(interp, gas_delta, current_opcode);
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let (call_type, opcode) = match inputs.scheme {
            revm::interpreter::CallScheme::Call => (CallType::Call, 0xF1u8),
            revm::interpreter::CallScheme::CallCode => (CallType::CallCode, 0xF2),
            revm::interpreter::CallScheme::DelegateCall => (CallType::DelegateCall, 0xF4),
            revm::interpreter::CallScheme::StaticCall => (CallType::StaticCall, 0xFA),
        };

        let function_selector = Self::extract_function_selector(&inputs.input);

        // Inherit parent's positive delta flag so that if the caller applied
        // positive deltas (reducing forwarded gas), the callee knows it may
        // OOG as a consequence.
        let parent_has_positive_delta =
            self.call_stack.last().map_or(false, |p| p.any_positive_delta_in_subtree);

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: inputs.bytecode_address,
            call_type,
            opcode,
            caller_pc: self.current_pc,
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector,
            any_positive_delta_in_subtree: parent_has_positive_delta,
        });

        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        if let Some(entry) = self.call_stack.pop() {
            // Apply precompile gas delta if this was a precompile call.
            // The inspector's step()/step_end() hooks don't fire for precompiles
            // (they execute inline without an interpreter loop), so we apply the
            // delta here by adjusting the outcome's gas usage. Since the precompile
            // has already executed, we record the delta for reporting and modify
            // the outcome's gas to reflect the schedule's pricing.
            if outcome.was_precompile_called {
                let input_bytes = match &inputs.input {
                    revm::interpreter::CallInput::Bytes(bytes) => bytes.as_ref(),
                    revm::interpreter::CallInput::SharedBuffer(_) => {
                        // SharedBuffer inputs cannot be inspected; precompile gas
                        // delta will use empty input, which may undercount variable
                        // units for precompiles like ECPAIRING or BLAKE2F.
                        &[]
                    }
                };
                let precompile_delta =
                    self.schedule.precompile_gas_delta(&inputs.bytecode_address, input_bytes);
                let multiplier_delta = self.multiplier_gas_delta(
                    inputs.gas_limit.saturating_sub(outcome.result.gas.remaining()),
                );
                let total_delta = precompile_delta.saturating_add(multiplier_delta);
                if total_delta != 0 {
                    self.additional_gas_charged += total_delta;
                    if total_delta > 0 {
                        self.any_positive_delta_applied = true;
                        if let Some(parent) = self.call_stack.last_mut() {
                            parent.any_positive_delta_in_subtree = true;
                        }
                        // Check if the precompile's increased cost would have caused
                        // OOG. The precompile already ran with the original cost, so
                        // we check if the delta exceeds the remaining gas headroom.
                        let gas_after_precompile = outcome.result.gas.remaining();
                        if (total_delta as u64) > gas_after_precompile {
                            if !self.oog_occurred {
                                self.oog_occurred = true;
                                self.oog_info = Some(OutOfGasInfo {
                                    opcode: entry.opcode,
                                    opcode_name: Self::opcode_name(entry.opcode),
                                    pc: entry.caller_pc,
                                    contract: inputs.bytecode_address,
                                    call_depth: entry.depth + 1,
                                    gas_remaining: gas_after_precompile,
                                    pattern: OogPattern::CallChain,
                                });
                            }
                        } else {
                            // Charge the extra gas from the precompile frame's budget.
                            // We already checked above that remaining >= delta, so this
                            // cannot fail.
                            let _ = outcome.result.gas.record_cost(total_delta as u64);
                        }
                    } else {
                        // Refund gas (precompile is cheaper under this schedule)
                        let refund = (-total_delta) as u64;
                        let headroom = outcome
                            .result
                            .gas
                            .limit()
                            .saturating_sub(outcome.result.gas.remaining());
                        outcome.result.gas.erase_cost(refund.min(headroom));
                    }
                }
            }

            let call_success = outcome.result.result.is_ok();
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
                success: call_success,
                input: input_bytes,
                output: Some(outcome.result.output.clone()),
            });

            // Propagate per-frame positive delta flag to parent.
            if entry.any_positive_delta_in_subtree {
                if let Some(parent) = self.call_stack.last_mut() {
                    parent.any_positive_delta_in_subtree = true;
                }
            }

            // Indirect OOG detection: only flag when the subcall hit an OOG-class
            // error specifically — not StackOverflow, InvalidFEOpcode, or other
            // non-gas errors that gas schedule changes cannot cause. Intentional
            // reverts (REVERT, OutOfFunds) are also excluded.
            //
            // Uses per-frame `any_positive_delta_in_subtree` rather than the global
            // `any_positive_delta_applied` to avoid false positives where a positive
            // delta at depth 0 is unrelated to a natural OOG at depth 3.
            if is_oog_error(outcome.result.result) &&
                !self.oog_occurred &&
                entry.any_positive_delta_in_subtree
            {
                self.oog_occurred = true;
                if self.oog_info.is_none() {
                    self.oog_info = Some(OutOfGasInfo {
                        opcode: entry.opcode,
                        opcode_name: Self::opcode_name(entry.opcode),
                        pc: entry.caller_pc,
                        contract: inputs.bytecode_address,
                        call_depth: entry.depth + 1,
                        gas_remaining: 0,
                        pattern: OogPattern::CallChain,
                    });
                }
            }
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let (call_type, opcode) = match inputs.scheme() {
            revm::context_interface::CreateScheme::Create => (CallType::Create, 0xF0u8),
            revm::context_interface::CreateScheme::Create2 { .. } |
            revm::context_interface::CreateScheme::Custom { .. } => (CallType::Create2, 0xF5),
        };

        let parent_has_positive_delta =
            self.call_stack.last().map_or(false, |p| p.any_positive_delta_in_subtree);

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: Address::ZERO,
            call_type,
            opcode,
            caller_pc: self.current_pc,
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector: None,
            any_positive_delta_in_subtree: parent_has_positive_delta,
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
            let create_success = outcome.result.result.is_ok();

            self.call_frames.push(CallFrame {
                call_index: self.call_frames.len(),
                depth: entry.depth,
                from: inputs.caller(),
                to: Some(created_address),
                call_type: entry.call_type,
                gas_provided: inputs.gas_limit(),
                gas_used: entry.gas_at_start.saturating_sub(outcome.result.gas.remaining()),
                success: create_success,
                input: Some(inputs.init_code().clone()),
                output: Some(outcome.result.output.clone()),
            });

            // Propagate per-frame positive delta flag to parent.
            if entry.any_positive_delta_in_subtree {
                if let Some(parent) = self.call_stack.last_mut() {
                    parent.any_positive_delta_in_subtree = true;
                }
            }

            // Indirect OOG detection for CREATE (same per-subtree logic as call_end).
            if is_oog_error(outcome.result.result) &&
                !self.oog_occurred &&
                entry.any_positive_delta_in_subtree
            {
                self.oog_occurred = true;
                if self.oog_info.is_none() {
                    self.oog_info = Some(OutOfGasInfo {
                        opcode: entry.opcode,
                        opcode_name: Self::opcode_name(entry.opcode),
                        pc: entry.caller_pc,
                        contract: created_address,
                        call_depth: entry.depth + 1,
                        gas_remaining: 0,
                        pattern: OogPattern::CallChain,
                    });
                }
            }
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
    use crate::schedule::{BaselineSchedule, CsvPricingSchedule};

    #[test]
    fn test_schedule_inspector_new() {
        let inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        assert_eq!(inspector.schedule_name(), "baseline");
    }

    #[test]
    fn test_schedule_inspector_result() {
        let inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        let result = inspector.result();

        assert_eq!(result.schedule_name, "baseline");
        assert_eq!(result.additional_gas, 0);
        assert!(!result.would_oog);
    }

    #[test]
    fn test_csv_schedule_integration() {
        let csv_data = r#"Opcode,Parameter,Current Gas,New Gas
DIV,constant,5,15
"#;
        let csv_schedule =
            CsvPricingSchedule::from_csv("test-csv".to_string(), csv_data.as_bytes()).unwrap();

        let inspector = ScheduleInspector::new(Arc::new(csv_schedule));
        assert_eq!(inspector.schedule_name(), "test-csv");
    }

    #[test]
    fn test_operation_counts_initial() {
        let inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));

        assert_eq!(inspector.operation_counts().total_ops, 0);
        assert_eq!(inspector.operation_counts().sload_count, 0);
    }

    #[test]
    fn test_gas_loop_detection_disabled() {
        let inspector =
            ScheduleInspector::new(Arc::new(BaselineSchedule)).with_gas_loop_detection(false);

        assert!(!inspector.detect_gas_loops);
    }
}
