//! Schedule inspector that applies gas cost modifications during execution.
//!
//! This inspector re-executes a transaction under a given gas schedule by actually
//! modifying gas charges via `interp.gas.record_regular_cost()` / `erase_cost()`. Because
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
//! `inspect_instructions` loop, so `step/step_end` pairs from the subcall never
//! interleave with the parent's CALL opcode. This is why a flat
//! `call_delta_pre_applied` flag (rather than a per-frame stack) is sufficient.
//!
//! # CALL/CREATE delta semantics
//!
//! The gas delta for CALL/CREATE opcodes is applied in `step()` *before* the EVM
//! charges its own base cost for the opcode. This is safe because
//! [`GasSchedule::opcode_gas_delta`] returns an *additive* delta (`new_cost` -
//! `current_cost`), not an absolute replacement. The EVM still charges its own base
//! cost separately. The total gas consumed is the same regardless of deduction
//! order, but the intermediate `remaining` value between the inspector's charge and
//! the EVM's charge differs. This intermediate value matters: if `record_regular_cost(delta)`
//! succeeds but leaves `remaining` so low that the EVM's own CALL base-cost check
//! then fails, the EVM will halt with OOG. The inspector detects this via indirect
//! OOG detection in `call_end()` / `create_end()`, including the caller's PC from
//! the call stack for diagnostics. Schedule authors must ensure their deltas
//! represent the cost *difference*, not the total new cost; see
//! [`GasSchedule::opcode_gas_delta`] docs.
//!
//! # Asymmetry in cost increase vs decrease propagation
//!
//! Gas *increases* propagate fully through subcalls — `record_regular_cost()` can drive the
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
        CallFrame, CallType, DivergenceLocation, EventLog, FrameOpcodeCounts, OogPattern,
        OperationCounts, OutOfGasInfo, PerFrameCapture, MAX_TRACKED_FRAMES,
    },
    opcode,
    schedule::{GasSchedule, GasTaxBreakdown, OpcodeContext},
};
use alloy_primitives::{Address, U256};
use revm::{
    bytecode::opcode::OpCode,
    context_interface::ContextTr,
    interpreter::{
        CallInputs, CallOutcome, CreateInputs, CreateOutcome, InstructionResult, Interpreter,
    },
    Inspector,
};
use revm_interpreter::interpreter_types::{InputsTr, Jumps, RuntimeFlag};
use std::{collections::VecDeque, sync::Arc};

/// Whether the instruction result is an OOG-class error (as opposed to a
/// revert, stack error, or other non-gas failure). Used for indirect OOG
/// detection in `call_end()` / `create_end()` to avoid false positives from
/// errors that gas changes cannot cause.
///
/// NOTE: if revm adds new OOG-class `InstructionResult` variants, this
/// function must be updated to include them. Check revm's `InstructionResult`
/// enum after dependency upgrades.
const fn is_oog_error(result: InstructionResult) -> bool {
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

    /// Captured at `step()` for the upcoming CALL/CALLCODE/DELEGATECALL/
    /// STATICCALL opcode: the raw gas argument the caller pushed onto the
    /// stack (top of stack at CALL invocation). Consumed in `call()` to
    /// populate the child frame's `gas_requested_on_stack`. Cleared after
    /// each consumption.
    pending_call_stack_gas: Option<u64>,

    /// Per-frame opcode counters, indexed by frame-open order. Root frame
    /// is lazily pushed on the first `step()` so we capture root-frame
    /// opcodes without depending on a `call()` hook (root never gets one).
    frame_capture: PerFrameCapture,

    /// Stack of indices into `frame_capture.frames` for the currently
    /// active frame chain. `usize::MAX` means "this frame's index would
    /// have exceeded `MAX_TRACKED_FRAMES`; skip per-opcode increments".
    active_frame_stack: Vec<usize>,

    /// Next frame-open index to assign (also = pre-cap frame count).
    next_frame_index: u32,

    /// Per-opcode gas delta the inspector pre-applied in `step()` for a
    /// CALL/CREATE opcode. Read and reset in the matching `step_end()` so
    /// we can subtract it from `actual_gas_cost` to recover the EVM's
    /// natural baseline cost.
    pending_pre_applied_delta: i64,

    /// Gas the most-recently-returned callee consumed. Set in
    /// `call_end()` / `create_end()` (we already compute it for the
    /// `CallFrame` record), read and cleared in the matching `step_end()`
    /// for the outer CALL/CREATE opcode so its per-opcode total doesn't
    /// double-count the callee's gas (which is already attributed to
    /// the callee's leaf opcodes via `step/step_end` inside the sub-frame).
    pending_callee_gas_used: u64,

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

    /// Location of the first per-opcode gas delta of *any* sign (first opcode
    /// the schedule repriced). A schedule with negative deltas (e.g. CSV pricing
    /// that lowers an opcode's cost) can fire this before the first *net*
    /// surcharge — see `first_gas_divergence`.
    divergence_location: Option<DivergenceLocation>,

    /// Location where the cumulative repricing surcharge (`additional_gas_charged`)
    /// first crossed from ≤0 to >0 — the first opcode at which the schedule's
    /// running gas genuinely exceeds baseline (F10). Distinct from the
    /// behavioral `divergence_location`.
    first_gas_divergence: Option<DivergenceLocation>,

    /// Whether a CALL/CREATE delta was pre-applied in `step()` and should be
    /// skipped in `step_end()`.
    ///
    /// Safe as a flat flag (not a per-frame stack) because revm's
    /// `inspect_instructions` calls step → execute → `step_end` within a single
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

    /// Location of the first per-opcode delta of any sign.
    pub divergence_location: Option<DivergenceLocation>,

    /// Location where the cumulative surcharge first went positive (F10).
    pub first_gas_divergence: Option<DivergenceLocation>,
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
    /// Cumulative repricing gas delta accumulated during this frame's execution.
    repricing_gas_delta: i64,

    /// Whether any positive gas delta was applied in this frame, any ancestor,
    /// or any descendant. This flag is inherited from the parent on frame entry
    /// (so if the caller had positive deltas that reduced forwarded gas, the
    /// callee inherits that context) and propagated back to the parent on frame
    /// exit. This provides precise per-subtree tracking: a positive delta at
    /// depth 0 correctly propagates to a callee OOG at depth 1, but an
    /// unrelated call tree without positive deltas won't false-positive.
    any_positive_delta_in_subtree: bool,

    /// Program counter of the most recent `step()` within this frame.
    /// Captured on every opcode so that when a native-revm schedule (e.g.
    /// EIP-8037) drives the frame to an OOG inside revm's own gas accounting,
    /// `call_end` / `create_end` can attribute the divergence to the offending
    /// opcode rather than reporting placeholder zeros.
    last_step_pc: usize,
    /// Opcode of the most recent `step()` within this frame. Paired with
    /// `last_step_pc`.
    last_step_opcode: u8,
    /// Raw gas argument the caller pushed onto the stack at the CALL opcode.
    /// `None` for CREATE/CREATE2 and the root frame.
    gas_requested_on_stack: Option<u64>,
    /// Parent's remaining gas at the moment the CALL/CREATE opcode executed,
    /// before the EIP-150 cap was applied. `None` for the root frame.
    parent_gas_at_call: Option<u64>,
}

/// Gas opcode event for loop detection.
#[derive(Debug, Clone)]
struct GasOpcodeEvent {
    pc: usize,
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
            pending_call_stack_gas: None,
            frame_capture: PerFrameCapture::new(),
            active_frame_stack: Vec::new(),
            next_frame_index: 0,
            pending_pre_applied_delta: 0,
            pending_callee_gas_used: 0,
            call_stack: Vec::new(),
            call_frames: Vec::new(),
            event_logs: Vec::new(),
            additional_gas_charged: 0,
            any_positive_delta_applied: false,
            oog_occurred: false,
            oog_info: None,
            divergence_location: None,
            first_gas_divergence: None,
            call_delta_pre_applied: false,
            gas_opcode_usage: VecDeque::new(),
            max_gas_events: 1000,
            detect_gas_loops: true,
        }
    }

    /// Enable or disable gas loop detection.
    pub const fn with_gas_loop_detection(mut self, enabled: bool) -> Self {
        self.detect_gas_loops = enabled;
        self
    }

    /// Get the operation counts.
    pub const fn operation_counts(&self) -> &OperationCounts {
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

    /// Get the per-frame opcode capture for this transaction.
    ///
    /// Returns the frames in frame-open order (root at index 0), each with
    /// its counts, baseline gas, and schedule gas keyed by opcode byte.
    /// `truncated == true` indicates the tx exceeded
    /// [`crate::divergence::MAX_TRACKED_FRAMES`] and later frames' counts
    /// were dropped — execution still ran to completion.
    pub const fn frame_opcode_counts(&self) -> &PerFrameCapture {
        &self.frame_capture
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
            first_gas_divergence: self.first_gas_divergence.clone(),
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
    ///
    /// `call_depth` is emitted **1-based** (root frame = 1, first sub-call =
    /// 2, ...) so it matches the 1-based contract expected by
    /// `oog_chain::classify_oog_chain` and the `record_frame_*` fallback
    /// paths (which emit `popped.depth + 1`). Without the `+ 1`, OOGs in the
    /// root frame would surface as `call_depth: 0`, the classifier would
    /// fail `checked_sub(1)`, and the row would fall through to
    /// `ContractBroken` instead of `WalletFixableShallow`.
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
            call_depth: self.call_stack.len() + 1,
            gas_remaining: interp.gas.remaining(),
            pattern,
            additional_gas_at_oog: self.additional_gas_charged,
        });
        self.oog_occurred = true;
    }

    /// Human-readable opcode name for diagnostics.
    fn opcode_name(opcode: u8) -> String {
        OpCode::new(opcode).map_or_else(|| format!("0x{opcode:02x}"), |op| op.as_str().to_string())
    }

    /// Record divergence location (first occurrence only).
    ///
    /// `call_depth` is emitted **1-based** (root frame = 1, first sub-call =
    /// 2, ...) to match the `record_frame_*` fallback paths and the
    /// downstream `is_shallow_oog` predicate.
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
            call_depth: self.call_stack.len() + 1,
            opcode,
            opcode_name: Self::opcode_name(opcode),
        });
    }

    /// Record divergence location from a popped frame's last-seen step.
    ///
    /// Used by `call_end` / `create_end` when a frame halts with an OOG-class
    /// error but no explicit `apply_gas_delta` ever fired for the schedule
    /// (i.e. native-revm schedules like EIP-8037, where gas accounting lives
    /// inside revm rather than in per-opcode deltas). The popped entry's
    /// `last_step_pc` / `last_step_opcode` are the most recent opcode the
    /// inspector saw within that frame, which is the opcode whose execution
    /// triggered revm's gas check.
    ///
    /// `contract` is taken from the popped frame; `function_selectors` come
    /// from the still-active call stack plus the popped frame so the chain
    /// reflects what was on the stack at the OOG point. No-ops if a divergence
    /// has already been recorded.
    fn record_frame_divergence(&mut self, popped: &CallStackEntry) {
        if self.divergence_location.is_some() {
            return;
        }

        let mut function_selectors: Vec<Option<[u8; 4]>> =
            self.call_stack.iter().map(|entry| entry.function_selector).collect();
        function_selectors.push(popped.function_selector);

        self.divergence_location = Some(DivergenceLocation {
            contract: popped.contract,
            function_selectors,
            pc: popped.last_step_pc,
            call_depth: popped.depth + 1,
            opcode: popped.last_step_opcode,
            opcode_name: Self::opcode_name(popped.last_step_opcode),
        });
    }

    /// Record OOG diagnostic info from a popped frame's last-seen step.
    ///
    /// Mirrors `record_frame_divergence` for the `OutOfGasInfo` field. Used
    /// when a frame halts with an OOG-class error and no `apply_gas_delta`
    /// fired (native-revm schedules). Without this path the only signal
    /// downstream forensics receive is the unstructured `halt_info` string,
    /// which doesn't carry pc / opcode / contract fields. No-ops if
    /// `oog_info` is already populated.
    fn record_frame_oog_info(&mut self, popped: &CallStackEntry, gas_remaining: u64) {
        if self.oog_info.is_some() {
            return;
        }
        let pattern = self.infer_oog_pattern(popped.last_step_opcode);
        self.oog_info = Some(OutOfGasInfo {
            opcode: popped.last_step_opcode,
            opcode_name: Self::opcode_name(popped.last_step_opcode),
            pc: popped.last_step_pc,
            contract: popped.contract,
            call_depth: popped.depth + 1,
            gas_remaining,
            pattern,
            additional_gas_at_oog: self.additional_gas_charged,
        });
    }

    /// Infer OOG pattern based on opcode.
    fn infer_oog_pattern(&self, opcode: u8) -> OogPattern {
        match opcode {
            opcode::SLOAD | opcode::SSTORE => OogPattern::StorageHeavy,
            opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
                OogPattern::CallChain
            }
            0x51..=0x53 => OogPattern::MemoryExpansion, // MLOAD, MSTORE, MSTORE8
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
        }
    }

    /// Whether this opcode dispatches a subcall whose gas allocation depends on
    /// the caller's remaining gas (CALL, CALLCODE, DELEGATECALL, STATICCALL,
    /// CREATE, CREATE2).
    const fn is_call_or_create(opcode: u8) -> bool {
        use crate::opcode::*;
        matches!(opcode, CREATE | CALL | CALLCODE | DELEGATECALL | CREATE2 | STATICCALL)
    }

    /// Push a new per-frame opcode counter and return its stack index.
    ///
    /// Returns `usize::MAX` when the [`MAX_TRACKED_FRAMES`] cap has been
    /// reached; the caller still pushes this sentinel onto
    /// `active_frame_stack` so `call_end` / `create_end` can pop in the
    /// usual way. `record_opcode_in_active_frame` ignores the sentinel.
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

    /// Ensure the root frame's per-opcode counter exists before any opcode
    /// is recorded. The root frame never enters `call()`, so we lazily
    /// initialise on first use from `step()`.
    fn ensure_root_frame_counter(&mut self) {
        if self.active_frame_stack.is_empty() {
            let idx = self.push_frame_counter();
            self.active_frame_stack.push(idx);
        }
    }

    /// Increment the count for `opcode` on the current frame's counter.
    fn record_opcode_count_in_active_frame(&mut self, opcode: u8) {
        let Some(&idx) = self.active_frame_stack.last() else { return };
        if idx == usize::MAX {
            return;
        }
        if let Some(frame) = self.frame_capture.frames.get_mut(idx) {
            frame.counts[opcode as usize] = frame.counts[opcode as usize].saturating_add(1);
        }
    }

    /// Add gas-baseline and gas-schedule amounts to the active frame's
    /// counters. Called once per opcode in `step_end()` after the EVM has
    /// charged the natural cost and any per-opcode delta has been resolved.
    fn record_opcode_gas_in_active_frame(
        &mut self,
        opcode: u8,
        gas_baseline: u64,
        gas_schedule: u64,
    ) {
        let Some(&idx) = self.active_frame_stack.last() else { return };
        if idx == usize::MAX {
            return;
        }
        if let Some(frame) = self.frame_capture.frames.get_mut(idx) {
            let i = opcode as usize;
            frame.gas_baseline[i] = frame.gas_baseline[i].saturating_add(gas_baseline);
            frame.gas_schedule[i] = frame.gas_schedule[i].saturating_add(gas_schedule);
        }
    }

    /// Accumulate a per-opcode tax breakdown into the tx's running per-category
    /// sums (F12). The sum of these reconciles to `additional_gas_charged`.
    const fn accumulate_tax(op_counts: &mut OperationCounts, b: &GasTaxBreakdown) {
        op_counts.tax_second_db_read += b.second_db_read;
        op_counts.tax_other += b.other;
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
        let surcharge_before = self.additional_gas_charged;
        self.additional_gas_charged += gas_delta;
        // F10: first opcode where the cumulative surcharge crosses into the
        // positive (schedule's running gas first exceeds baseline). Distinct
        // from `divergence_location`, which fires on the first delta of any
        // sign — a schedule with negative deltas (e.g. CSV pricing) can fire
        // that earlier than this net-positive crossing.
        if self.first_gas_divergence.is_none() &&
            surcharge_before <= 0 &&
            self.additional_gas_charged > 0
        {
            let contract = self.call_stack.last().map(|e| e.contract).unwrap_or(Address::ZERO);
            self.first_gas_divergence = Some(DivergenceLocation {
                contract,
                function_selectors: self
                    .call_stack
                    .iter()
                    .map(|entry| entry.function_selector)
                    .collect(),
                pc: interp.bytecode.pc(),
                call_depth: self.call_stack.len() + 1,
                opcode,
                opcode_name: Self::opcode_name(opcode),
            });
        }
        if let Some(frame) = self.call_stack.last_mut() {
            frame.repricing_gas_delta += gas_delta;
        }

        if gas_delta > 0 {
            self.any_positive_delta_applied = true;
            if let Some(frame) = self.call_stack.last_mut() {
                frame.any_positive_delta_in_subtree = true;
            }
            if !interp.gas.record_regular_cost(gas_delta as u64) {
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
        self.gas_opcode_usage.push_back(GasOpcodeEvent { pc: interp.bytecode.pc() });

        while self.gas_opcode_usage.len() > self.max_gas_events {
            self.gas_opcode_usage.pop_front();
        }
    }
}

/// Classify whether a cold-account-access target is touched cold this tx,
/// **read-only**. Returns the cold-ness only (no code/no-code classification).
///
/// `load_account_info_skip_cold_load(skip_cold_load = true)` never warms the
/// target: it returns `Ok` for an already-warm account and `ColdLoadSkipped`
/// for a cold one, so the replay's own cold/warm accounting is preserved.
///
/// Invoked for **every** schedule (not just EIP-8038): the cold-account
/// classification is a near-free native baseline, and schedules that don't price
/// it simply ignore it. See [`OperationCounts::cold_account_access_count`].
fn classify_account_target<CTX>(context: &mut CTX, addr: Address) -> bool
where
    CTX: ContextTr,
{
    use revm::context_interface::{journaled_state::JournalLoadError, JournalTr};

    match context.journal_mut().load_account_info_skip_cold_load(addr, false, true) {
        Ok(_) => false,
        Err(JournalLoadError::ColdLoadSkipped) => true,
        Err(JournalLoadError::DBError(err)) => {
            // A real DB error here would also fail revm's own account load and
            // abort the tx; surface it rather than silently classifying as warm.
            tracing::warn!(target: "exex::research", %addr, ?err, "cold-account classification: journal load failed; treating as warm");
            false
        }
    }
}

/// Classify a storage slot for the EIP-8038 storage-reprice drivers (F8),
/// **read-only**. Returns `(is_cold, current)`.
///
/// `sload_skip_cold_load(.., skip_cold_load = true)` never warms the slot:
/// `Ok` for an already-warm slot (carrying its current journaled value) and
/// `ColdLoadSkipped` for a cold one (short-circuiting before any DB load). A
/// cold slot's current value equals its committed `original`, so `current` is
/// `None` there and the caller substitutes the committed value.
fn classify_storage_slot<CTX>(context: &mut CTX, addr: Address, key: U256) -> (bool, Option<U256>)
where
    CTX: ContextTr,
{
    use revm::context_interface::{journaled_state::JournalLoadError, JournalTr};

    match context.journal_mut().sload_skip_cold_load(addr, key, true) {
        Ok(load) => (false, Some(load.data)),
        Err(JournalLoadError::ColdLoadSkipped) => (true, None),
        Err(JournalLoadError::DBError(err)) => {
            tracing::warn!(target: "exex::research", %addr, ?err, "cold-storage classification: journal load failed; treating as warm");
            (false, None)
        }
    }
}

/// EIP-2200 / EIP-3529 SSTORE outcome buckets (mutually exclusive), the
/// repricing drivers for EIP-8038's `STORAGE_WRITE` / `REFUND_STORAGE_CLEAR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SstoreTransition {
    /// `current == new` — no state change (warm read cost only).
    Noop,
    /// Clean slot (`original == current`), `original == 0`, `new != 0` — a fresh
    /// set. Charged `STORAGE_WRITE`.
    Set,
    /// Clean slot, `original != 0`, `new != 0`, `new != original` — overwrite.
    /// Charged `STORAGE_WRITE`.
    Reset,
    /// Clean slot, `original != 0`, `new == 0` — clear. Charged `STORAGE_WRITE`
    /// and earns `REFUND_STORAGE_CLEAR`.
    Clear,
    /// Slot already dirtied earlier this tx (`original != current`) — rewrite at
    /// the warm rate, no fresh write surcharge.
    Dirty,
}

/// Classify an SSTORE from its EIP-2200 `(original, current, new)` triple.
fn classify_sstore_transition(original: U256, current: U256, new: U256) -> SstoreTransition {
    if current == new {
        return SstoreTransition::Noop;
    }
    if original != current {
        return SstoreTransition::Dirty;
    }
    // Clean slot (this is the first write to it this tx).
    if original.is_zero() {
        SstoreTransition::Set
    } else if new.is_zero() {
        SstoreTransition::Clear
    } else {
        SstoreTransition::Reset
    }
}

impl<CTX> Inspector<CTX, revm::interpreter::interpreter::EthInterpreter> for ScheduleInspector
where
    CTX: ContextTr,
{
    fn step(
        &mut self,
        interp: &mut Interpreter<revm::interpreter::interpreter::EthInterpreter>,
        context: &mut CTX,
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

        // Track the most recent opcode/pc per active frame. For native-revm
        // schedules (e.g. EIP-8037) the inspector's `apply_gas_delta` never
        // fires, so `record_divergence` is never called from `step_end`. When
        // revm's own gas accounting then halts a frame with OOG, `call_end` /
        // `create_end` reads these fields to attribute the divergence to the
        // opcode that triggered it rather than a placeholder.
        if let Some(frame) = self.call_stack.last_mut() {
            frame.last_step_pc = self.current_pc;
            frame.last_step_opcode = self.current_opcode;
        }
        // Reset for this opcode. The flag may be true here if a parent frame's
        // CALL set it and then the subcall frame started — this is expected.
        // The invariant is that step_end() of the *same* frame respects the
        // flag set by step() of the *same* opcode, which holds because revm
        // calls step → execute → step_end within a single loop iteration.
        self.call_delta_pre_applied = false;

        // Cold-account classification: classify the account-access target
        // (cold?) read-only *before* execution — and, for the CALL family,
        // before the gas is forwarded. The target's stack position differs by
        // opcode: top-of-stack for BALANCE / EXTCODE* / SELFDESTRUCT, index 1
        // for the CALL family.
        let target_pos = match self.current_opcode {
            opcode::BALANCE |
            opcode::EXTCODESIZE |
            opcode::EXTCODECOPY |
            opcode::EXTCODEHASH |
            opcode::SELFDESTRUCT => Some(0),
            opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => Some(1),
            _ => None,
        };
        if let Some(pos) = target_pos &&
            let Ok(word) = interp.stack.peek(pos)
        {
            let bytes = word.to_be_bytes::<32>();
            let addr = Address::from_slice(&bytes[12..]);
            let is_cold = classify_account_target(context, addr);
            // Count the cold/warm account-access split for data collection (F2).
            if is_cold {
                self.op_counts.cold_account_access_count += 1;
            } else {
                self.op_counts.warm_account_access_count += 1;
            }
        }

        // Value-transfer classification (F2): CALL / CALLCODE carry the
        // transferred value as the 3rd stack item (index 2 — after gas at 0
        // and target at 1). DELEGATECALL/STATICCALL carry no value and are
        // excluded. Peeked read-only before execution, mirroring the
        // account-target peek above.
        if matches!(self.current_opcode, opcode::CALL | opcode::CALLCODE) &&
            let Ok(value) = interp.stack.peek(2) &&
            !value.is_zero()
        {
            self.op_counts.value_transfer_count += 1;
        }

        // EIP-8038 storage-reprice drivers (F8): classify SLOAD/SSTORE slot
        // cold/warm read-only, and the SSTORE (original, current, new)
        // transition. The storage context is the executing frame's
        // `target_address` — revm's own source for these opcodes
        // (`instructions::host::{sload,sstore}`) — so DELEGATECALL and the root
        // frame are handled uniformly. Classified *before* execution so the
        // cold/warm read mirrors what revm charges, without warming the slot.
        use revm::context_interface::Database;
        match self.current_opcode {
            opcode::SLOAD => {
                if let Ok(key) = interp.stack.peek(0) {
                    let addr = interp.input.target_address();
                    let (is_cold, _current) = classify_storage_slot(context, addr, key);
                    if is_cold {
                        self.op_counts.sload_cold_count += 1;
                    } else {
                        self.op_counts.sload_warm_count += 1;
                    }
                }
            }
            // A static-context SSTORE is rejected by revm before it writes, so
            // skip it rather than count a write that never happens.
            opcode::SSTORE if !interp.runtime_flag.is_static() => {
                if let (Ok(key), Ok(new)) = (interp.stack.peek(0), interp.stack.peek(1)) {
                    let addr = interp.input.target_address();
                    let (is_cold, current) = classify_storage_slot(context, addr, key);
                    if is_cold {
                        self.op_counts.sstore_cold_count += 1;
                    }
                    // `original` = committed value (DB layer, below the journal's
                    // warm set). For a cold slot `current` equals `original`.
                    let original = context.db_mut().storage(addr, key).unwrap_or(U256::ZERO);
                    let current = current.unwrap_or(original);
                    match classify_sstore_transition(original, current, new) {
                        SstoreTransition::Noop => self.op_counts.sstore_noop_count += 1,
                        SstoreTransition::Set => self.op_counts.sstore_set_count += 1,
                        SstoreTransition::Reset => self.op_counts.sstore_reset_count += 1,
                        SstoreTransition::Clear => self.op_counts.sstore_clear_count += 1,
                        SstoreTransition::Dirty => self.op_counts.sstore_dirty_count += 1,
                    }
                }
            }
            _ => {}
        }

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
                    exp.bit_len().div_ceil(8)
                }
            })
        } else {
            None
        };

        // Capture the raw gas argument the caller pushed onto the stack at
        // CALL/CALLCODE/DELEGATECALL/STATICCALL. Top-of-stack is `gas` for all
        // four. Saturated to u64; the caller may push values up to 2^256-1
        // (canonical "all available gas" pattern), so we clamp to u64::MAX.
        // Consumed by `call()` to populate the child frame's
        // `gas_requested_on_stack`.
        self.pending_call_stack_gas = match self.current_opcode {
            opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
                interp.stack.peek(0).ok().map(|gas| gas.saturating_to::<u64>())
            }
            _ => None,
        };

        // Track operation counts
        self.op_counts.total_ops += 1;

        match self.current_opcode {
            opcode::SLOAD => self.op_counts.sload_count += 1,
            opcode::SSTORE => self.op_counts.sstore_count += 1,
            0xA0..=0xA4 => self.op_counts.log_count += 1,
            opcode::CALL | opcode::CALLCODE | opcode::DELEGATECALL | opcode::STATICCALL => {
                self.op_counts.call_count += 1
            }
            opcode::CREATE | opcode::CREATE2 => self.op_counts.create_count += 1,
            opcode::GAS if self.detect_gas_loops => self.track_gas_opcode(interp),
            _ => {}
        }

        // Per-frame opcode bookkeeping. `call()` and `create()` push a new
        // frame counter when a sub-frame opens; the root frame never receives
        // those hooks, so we lazily initialise it here on the first opcode.
        // Gas baseline/schedule is recorded later in `step_end()`.
        self.ensure_root_frame_counter();
        self.record_opcode_count_in_active_frame(self.current_opcode);

        // Track memory usage
        let memory_words = interp.memory.len().div_ceil(32);
        if memory_words as u64 > self.op_counts.memory_words_allocated {
            self.op_counts.memory_words_allocated = memory_words as u64;
        }

        // Reset the pending pre-applied delta for this opcode. If the
        // CALL/CREATE branch below applies a delta, we'll set it; otherwise
        // step_end() will see 0 and treat `actual_gas_cost` as the natural
        // baseline cost.
        self.pending_pre_applied_delta = 0;

        // For CALL/CREATE opcodes, apply the gas delta BEFORE execution so it
        // feeds into the 63/64 gas forwarding rule for the subcall.
        if Self::is_call_or_create(self.current_opcode) {
            let opcode_ctx = self.build_opcode_context(interp);
            // F12: the breakdown's total equals `opcode_gas_delta`, so gas
            // behavior is unchanged; we additionally fold the categories in.
            let breakdown =
                self.schedule.opcode_gas_tax_breakdown(self.current_opcode, &opcode_ctx);
            let gas_delta = breakdown.total();
            if gas_delta != 0 {
                self.call_delta_pre_applied = true;
                self.pending_pre_applied_delta = gas_delta;
                Self::accumulate_tax(&mut self.op_counts, &breakdown);
                if !self.apply_gas_delta(interp, gas_delta, self.current_opcode) { // OOG — interpreter is halted, don't continue
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
        // `gas_before - gas_remaining_after` for a CALL/CREATE includes
        // the callee's net consumption; the callee's gas is already
        // accounted for by step/step_end inside its sub-frame, so
        // subtract it here to leave only the intrinsic CALL/CREATE
        // charge on the outer opcode (avoids double-counting).
        let pending_callee = std::mem::take(&mut self.pending_callee_gas_used);
        let raw_cost = gas_before.saturating_sub(interp.gas.remaining());
        let actual_gas_cost = if pending_callee > 0 && Self::is_call_or_create(current_opcode) {
            raw_cost.saturating_sub(pending_callee)
        } else {
            raw_cost
        };

        let opcode_ctx = self.build_opcode_context(interp);
        // F12: explicit breakdown (empty when the delta was pre-applied in
        // step()); its total equals the old `explicit_gas_delta`.
        let explicit_breakdown = if self.call_delta_pre_applied {
            GasTaxBreakdown::default()
        } else {
            self.schedule.opcode_gas_tax_breakdown(current_opcode, &opcode_ctx)
        };
        // Uniform multipliers are derived from the EVM's observed native cost
        // after the opcode has executed. That makes them work for the live
        // schedule path, but unlike explicit additive deltas they do not feed
        // into CALL/CREATE gas forwarding before dispatch. Multipliers are
        // unclassified → `other`.
        let multiplier_gas_delta = self.multiplier_gas_delta(actual_gas_cost);
        let mut tax_breakdown = explicit_breakdown;
        tax_breakdown.other = tax_breakdown.other.saturating_add(multiplier_gas_delta);
        let gas_delta = tax_breakdown.total();

        // Per-frame gas accounting. `actual_gas_cost` includes any delta
        // pre-applied in `step()` (CALL/CREATE pre-deduction); subtract it
        // so `gas_baseline` reflects only the EVM's natural charge. The
        // total schedule cost is the natural charge plus all deltas
        // (pre-applied + step_end additions).
        let pre_applied = self.pending_pre_applied_delta;
        self.pending_pre_applied_delta = 0;
        let gas_baseline_increment =
            (actual_gas_cost as i64).saturating_sub(pre_applied).max(0) as u64;
        let total_opcode_delta = pre_applied.saturating_add(gas_delta);
        let gas_schedule_increment =
            (gas_baseline_increment as i64).saturating_add(total_opcode_delta).max(0) as u64;
        self.record_opcode_gas_in_active_frame(
            current_opcode,
            gas_baseline_increment,
            gas_schedule_increment,
        );

        if gas_delta != 0 {
            // F12: the step()-pre-applied portion was already folded in; here we
            // add the step_end portion (explicit-if-not-pre-applied + multiplier).
            Self::accumulate_tax(&mut self.op_counts, &tax_breakdown);
            self.apply_gas_delta(interp, gas_delta, current_opcode);
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let (call_type, opcode) = match inputs.scheme {
            revm::interpreter::CallScheme::Call => (CallType::Call, crate::opcode::CALL),
            revm::interpreter::CallScheme::CallCode => {
                (CallType::CallCode, crate::opcode::CALLCODE)
            }
            revm::interpreter::CallScheme::DelegateCall => {
                (CallType::DelegateCall, crate::opcode::DELEGATECALL)
            }
            revm::interpreter::CallScheme::StaticCall => {
                (CallType::StaticCall, crate::opcode::STATICCALL)
            }
        };

        let function_selector = Self::extract_function_selector(&inputs.input);

        // Inherit parent's positive delta flag so that if the caller applied
        // positive deltas (reducing forwarded gas), the callee knows it may
        // OOG as a consequence.
        let parent_has_positive_delta =
            self.call_stack.last().is_some_and(|p| p.any_positive_delta_in_subtree);

        // Take the stack-gas captured in step() (None if call() fired without
        // a preceding step, e.g. the root frame).
        let gas_requested_on_stack = self.pending_call_stack_gas.take();
        let parent_gas_at_call = self.gas_before_step;

        // Open a new per-frame opcode counter for the sub-call. The root
        // frame was lazily initialised in `step()`; this `call()` always
        // corresponds to a sub-call, so we push unconditionally.
        let frame_capture_idx = self.push_frame_counter();
        self.active_frame_stack.push(frame_capture_idx);

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: inputs.bytecode_address,
            call_type,
            opcode,
            caller_pc: self.current_pc,
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector,
            repricing_gas_delta: 0,
            any_positive_delta_in_subtree: parent_has_positive_delta,
            last_step_pc: 0,
            last_step_opcode: 0,
            gas_requested_on_stack,
            parent_gas_at_call,
        });

        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        // Pop the per-frame opcode counter stack to match the matching call()
        // push. The counts stay in `frame_capture.frames` for later
        // emission; only the active-frame index is unwound. Done regardless
        // of how the frame ended so we never leave the stack out of sync.
        self.active_frame_stack.pop();

        if let Some(entry) = self.call_stack.pop() {
            let mut frame_repricing_delta = entry.repricing_gas_delta;

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
                    frame_repricing_delta += total_delta;
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
                                    additional_gas_at_oog: self.additional_gas_charged,
                                });
                            }
                        } else {
                            // Charge the extra gas from the precompile frame's budget.
                            // We already checked above that remaining >= delta, so this
                            // cannot fail.
                            let _ = outcome.result.gas.record_regular_cost(total_delta as u64);
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

            // Transfer-value only: `CallValue::Apparent` (DELEGATECALL)
            // is not transferred, so we treat its presence as "no
            // transfer" rather than recording the apparent number.
            let value_wei = match inputs.value {
                revm::interpreter::CallValue::Transfer(v) => Some(v.saturating_to::<u128>()),
                revm::interpreter::CallValue::Apparent(_) => None,
            };

            // Hand the callee's net gas to the next step_end() for the
            // parent's CALL opcode so its per-opcode total isn't
            // double-counted. Uses gas_limit (forwarded to the callee),
            // not `entry.gas_at_start` (caller's gas before the CALL
            // intrinsic was charged) — we want exactly the callee's
            // consumption, not the intrinsic CALL cost.
            //
            // Skipped for precompile calls: revm runs precompiles
            // inline without firing step()/step_end() in the sub-frame,
            // so the precompile's gas isn't attributed to any leaf
            // opcode. Leaving it under the outer CALL keeps it visible
            // in the per-opcode breakdown.
            if !outcome.was_precompile_called {
                self.pending_callee_gas_used =
                    inputs.gas_limit.saturating_sub(outcome.result.gas.remaining());
            }

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
                repricing_gas_delta: frame_repricing_delta,
                gas_requested_on_stack: entry.gas_requested_on_stack,
                parent_gas_at_call: entry.parent_gas_at_call,
                value_wei,
                // F9: failing-frame context.
                caller_pc: Some(entry.caller_pc),
                was_precompile: outcome.was_precompile_called,
                precompile_address: outcome
                    .was_precompile_called
                    .then_some(inputs.bytecode_address),
                gas_remaining_at_fail: (!call_success).then(|| outcome.result.gas.remaining()),
                // F3: storage/call target, distinct from `to` (the code holder)
                // under DELEGATECALL.
                storage_target: Some(inputs.target_address),
            });

            // Propagate per-frame positive delta flag to parent.
            if entry.any_positive_delta_in_subtree &&
                let Some(parent) = self.call_stack.last_mut()
            {
                parent.any_positive_delta_in_subtree = true;
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
                        additional_gas_at_oog: self.additional_gas_charged,
                    });
                }
            }

            // Capture divergence location for native-revm schedules. When the
            // schedule's gas accounting lives inside revm (e.g. EIP-8037),
            // `apply_gas_delta` never fires so `record_divergence` was never
            // called from `step_end`. The most recent step seen within the
            // popped frame is the opcode whose execution triggered revm's gas
            // check, so use that as the divergence point. Limited to OOG-class
            // results to avoid attributing intentional reverts to the schedule.
            //
            // Skipped when `last_step_opcode == 0` (no step ran in the frame —
            // empty bytecode or precompile) so we don't claim a STOP at pc 0.
            if is_oog_error(outcome.result.result) &&
                self.divergence_location.is_none() &&
                entry.last_step_opcode != 0
            {
                self.record_frame_divergence(&entry);
            }

            // Same path for `oog_info`. Without this, native-revm OOGs only
            // surface as the unstructured `halt_info` string ("Execution
            // halted: OutOfGas(...)") which downstream regex pipelines can't
            // extract pc / opcode / contract from.
            if is_oog_error(outcome.result.result) && entry.last_step_opcode != 0 {
                self.record_frame_oog_info(&entry, outcome.result.gas.remaining());
            }
        }
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let (call_type, opcode) = match inputs.scheme() {
            revm::context_interface::CreateScheme::Create => {
                (CallType::Create, crate::opcode::CREATE)
            }
            revm::context_interface::CreateScheme::Create2 { .. } |
            revm::context_interface::CreateScheme::Custom { .. } => {
                (CallType::Create2, crate::opcode::CREATE2)
            }
        };

        let parent_has_positive_delta =
            self.call_stack.last().is_some_and(|p| p.any_positive_delta_in_subtree);

        // Open a per-frame opcode counter for the CREATE/CREATE2 sub-frame.
        // Mirrors `call()` — the root frame is lazy-init in `step()`.
        let frame_capture_idx = self.push_frame_counter();
        self.active_frame_stack.push(frame_capture_idx);

        self.call_stack.push(CallStackEntry {
            depth: self.call_stack.len(),
            contract: Address::ZERO,
            call_type,
            opcode,
            caller_pc: self.current_pc,
            gas_at_start: self.gas_before_step.unwrap_or(0),
            function_selector: None,
            repricing_gas_delta: 0,
            any_positive_delta_in_subtree: parent_has_positive_delta,
            last_step_pc: 0,
            last_step_opcode: 0,
            // CREATE/CREATE2 don't take a gas argument from the stack; the
            // EVM forwards 63/64 of available gas automatically.
            gas_requested_on_stack: None,
            parent_gas_at_call: self.gas_before_step,
        });

        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        inputs: &CreateInputs,
        outcome: &mut CreateOutcome,
    ) {
        // Pop the per-frame opcode counter stack to match the matching
        // `create()` push (parallel to `call_end()`).
        self.active_frame_stack.pop();

        if let Some(entry) = self.call_stack.pop() {
            let created_address = outcome.address.unwrap_or(Address::ZERO);
            let create_success = outcome.result.result.is_ok();

            // CREATE/CREATE2 endow the new contract with `inputs.value`;
            // we capture it in the same column as CALL's transfer value.
            let value_wei = Some(inputs.value().saturating_to::<u128>());

            // Same double-count fix as call_end: feed the callee's net
            // gas (forwarded - returned) into the next step_end so the
            // outer CREATE/CREATE2 opcode gets only the intrinsic charge.
            self.pending_callee_gas_used =
                inputs.gas_limit().saturating_sub(outcome.result.gas.remaining());

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
                repricing_gas_delta: entry.repricing_gas_delta,
                gas_requested_on_stack: entry.gas_requested_on_stack,
                parent_gas_at_call: entry.parent_gas_at_call,
                value_wei,
                // F9: failing-frame context. CREATE never hits a precompile.
                caller_pc: Some(entry.caller_pc),
                was_precompile: false,
                precompile_address: None,
                gas_remaining_at_fail: (!create_success).then(|| outcome.result.gas.remaining()),
                // F3: CREATE has no separate storage target.
                storage_target: None,
            });

            // Propagate per-frame positive delta flag to parent.
            if entry.any_positive_delta_in_subtree &&
                let Some(parent) = self.call_stack.last_mut()
            {
                parent.any_positive_delta_in_subtree = true;
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
                        additional_gas_at_oog: self.additional_gas_charged,
                    });
                }
            }

            // Native-revm divergence capture (see call_end for rationale).
            // For CREATE, populate the popped entry's contract with the
            // resolved created address so downstream forensics can attribute
            // the failure to a specific deploy.
            if is_oog_error(outcome.result.result) && entry.last_step_opcode != 0 {
                let mut popped = entry;
                popped.contract = created_address;
                if self.divergence_location.is_none() {
                    self.record_frame_divergence(&popped);
                }
                self.record_frame_oog_info(&popped, outcome.result.gas.remaining());
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

    /// F8: the EIP-2200 SSTORE transition buckets over the canonical
    /// (original, current, new) cases.
    #[test]
    fn sstore_transition_classification() {
        let z = U256::ZERO;
        let a = U256::from(1);
        let b = U256::from(2);
        // current == new → no-op, regardless of original.
        assert_eq!(classify_sstore_transition(a, b, b), SstoreTransition::Noop);
        assert_eq!(classify_sstore_transition(z, z, z), SstoreTransition::Noop);
        // Clean slot (original == current):
        assert_eq!(classify_sstore_transition(z, z, a), SstoreTransition::Set); // 0 → nonzero
        assert_eq!(classify_sstore_transition(a, a, z), SstoreTransition::Clear); // nonzero → 0
        assert_eq!(classify_sstore_transition(a, a, b), SstoreTransition::Reset); // nonzero → other
                                                                                  // nonzero
                                                                                  // Already dirtied
                                                                                  // this tx (original
                                                                                  // != current) →
                                                                                  // dirty re-write.
        assert_eq!(classify_sstore_transition(z, a, b), SstoreTransition::Dirty);
        assert_eq!(classify_sstore_transition(a, b, z), SstoreTransition::Dirty);
    }

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

    #[test]
    fn frame_capture_starts_empty() {
        let inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        let capture = inspector.frame_opcode_counts();
        assert_eq!(capture.frames.len(), 0);
        assert!(!capture.truncated);
    }

    #[test]
    fn frame_capture_ensure_root_creates_single_frame() {
        let mut inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        inspector.ensure_root_frame_counter();
        let capture = inspector.frame_opcode_counts();
        assert_eq!(capture.frames.len(), 1);
        assert_eq!(capture.frames[0].call_index, 0);
        assert_eq!(inspector.active_frame_stack, vec![0]);
        // Idempotent — a second call doesn't double-push.
        inspector.ensure_root_frame_counter();
        assert_eq!(inspector.frame_opcode_counts().frames.len(), 1);
    }

    #[test]
    fn frame_capture_records_opcode_count_on_active_frame() {
        let mut inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        inspector.ensure_root_frame_counter();
        // Simulate two opcodes in root.
        inspector.record_opcode_count_in_active_frame(0x01); // ADD
        inspector.record_opcode_count_in_active_frame(0x01);
        inspector.record_opcode_count_in_active_frame(0x20); // KECCAK256
        let root = &inspector.frame_opcode_counts().frames[0];
        assert_eq!(root.counts[0x01], 2);
        assert_eq!(root.counts[0x20], 1);
        // Verify nonzero() filtering yields only the touched opcodes.
        let nonzero: Vec<_> = root.nonzero().map(|(op, c, _, _)| (op, c)).collect();
        assert_eq!(nonzero, vec![(0x01, 2), (0x20, 1)]);
    }

    #[test]
    fn frame_capture_records_gas_for_baseline_and_schedule() {
        let mut inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        inspector.ensure_root_frame_counter();
        // 50 baseline / 60 schedule = a +10 delta the inspector applied.
        inspector.record_opcode_gas_in_active_frame(0x01, 50, 60);
        inspector.record_opcode_gas_in_active_frame(0x01, 30, 35);
        let root = &inspector.frame_opcode_counts().frames[0];
        assert_eq!(root.gas_baseline[0x01], 80);
        assert_eq!(root.gas_schedule[0x01], 95);
    }

    #[test]
    fn frame_capture_pushes_and_pops_via_helpers() {
        // Simulate the lifecycle: root opens, sub-call opens, sub-call closes.
        let mut inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        inspector.ensure_root_frame_counter();
        // Sub-call 1.
        let sub_idx = inspector.push_frame_counter();
        inspector.active_frame_stack.push(sub_idx);
        assert_eq!(inspector.frame_opcode_counts().frames.len(), 2);
        assert_eq!(inspector.frame_opcode_counts().frames[1].call_index, 1);
        assert_eq!(inspector.active_frame_stack, vec![0, sub_idx]);
        // Increment within sub-call.
        inspector.record_opcode_count_in_active_frame(0x55); // SSTORE
        assert_eq!(inspector.frame_opcode_counts().frames[1].counts[0x55], 1);
        assert_eq!(inspector.frame_opcode_counts().frames[0].counts[0x55], 0);
        // Pop back to root.
        inspector.active_frame_stack.pop();
        assert_eq!(inspector.active_frame_stack, vec![0]);
        inspector.record_opcode_count_in_active_frame(0x55);
        assert_eq!(inspector.frame_opcode_counts().frames[0].counts[0x55], 1);
        // The sub-frame's counts persist after it's popped — that's the
        // point of keeping them in `frame_capture.frames`.
        assert_eq!(inspector.frame_opcode_counts().frames[1].counts[0x55], 1);
    }

    #[test]
    fn frame_capture_truncates_at_cap() {
        let mut inspector = ScheduleInspector::new(Arc::new(BaselineSchedule));
        inspector.ensure_root_frame_counter();
        // Open MAX_TRACKED_FRAMES - 1 more sub-frames to hit the cap exactly.
        for _ in 1..MAX_TRACKED_FRAMES {
            let idx = inspector.push_frame_counter();
            inspector.active_frame_stack.push(idx);
        }
        assert!(!inspector.frame_opcode_counts().truncated);
        assert_eq!(inspector.frame_opcode_counts().frames.len() as u32, MAX_TRACKED_FRAMES);
        // One more pushes the truncated flag and returns the sentinel.
        let overflow_idx = inspector.push_frame_counter();
        assert_eq!(overflow_idx, usize::MAX);
        assert!(inspector.frame_opcode_counts().truncated);
        // Recording on the sentinel is a no-op (doesn't panic, doesn't
        // affect any existing frame).
        inspector.active_frame_stack.push(overflow_idx);
        let pre_counts = inspector.frame_opcode_counts().frames[0].counts[0x01];
        inspector.record_opcode_count_in_active_frame(0x01);
        inspector.record_opcode_gas_in_active_frame(0x01, 10, 12);
        let post_counts = inspector.frame_opcode_counts().frames[0].counts[0x01];
        assert_eq!(pre_counts, post_counts, "sentinel should not bleed into root");
    }
}
