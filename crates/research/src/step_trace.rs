//! Step-level execution trace + baseline/schedule trace diff.
//!
//! Some schedule failures flip a transaction from success to revert without
//! ever triggering an out-of-gas halt and without the [`ScheduleInspector`]
//! recording a divergence point. The canonical case is a native-revm schedule
//! (EIP-8037) whose gas changes live inside the EVM env rather than in
//! per-opcode deltas: a gas-observable branch (`gasleft()` guard, a forwarded
//! gas amount, a refund-sensitive `require`) takes a different path and the
//! contract cleanly `revert`s. The chain-walk classifier only fires on OOG, so
//! these land in the dashboard with no divergence opcode — "contract-broken"
//! with nothing to show.
//!
//! [`StepTraceInspector`] records the executed `(call_depth, pc, opcode,
//! contract)` sequence. Running it under the baseline env and again under the
//! schedule env produces two traces; [`first_divergence`] returns the first
//! step where they part, which is the point the schedule's gas accounting
//! first changed control flow.
//!
//! This is a diagnostic replay, run only for the small unexplained cohort, so
//! the per-step capture cost is not on the hot path.
//!
//! [`ScheduleInspector`]: crate::multi_schedule_inspector::ScheduleInspector

use crate::divergence::DivergenceLocation;
use alloy_primitives::Address;
use revm::{
    bytecode::opcode::OpCode,
    context_interface::ContextTr,
    interpreter::{
        interpreter_types::Jumps, CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter,
    },
    Inspector,
};

/// Upper bound on recorded steps. A trace longer than this is marked
/// `truncated` and the diff is skipped — comparing partial traces would risk
/// a false divergence at the truncation boundary. ~1M steps × 28 bytes ≈ 28 MB
/// transient, freed when the inspector drops; only paid for the unexplained
/// cohort.
pub const MAX_TRACE_STEPS: usize = 1 << 20;

/// One executed opcode. The comparison key is `(depth, pc, opcode)`; `contract`
/// rides along only to label the divergence point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepRecord {
    /// 0-based call depth (root frame = 0).
    pub depth: u32,
    /// Program counter within the executing bytecode.
    pub pc: u32,
    /// Opcode byte.
    pub opcode: u8,
    /// Address of the contract whose code is executing. `Address::ZERO` for
    /// frames whose address isn't known at step time (CREATE init code).
    pub contract: Address,
}

impl StepRecord {
    /// Path-identity key: two executions agree at a step iff these match.
    /// Deliberately excludes `contract` (it's derived from the same path).
    const fn key(&self) -> (u32, u32, u8) {
        (self.depth, self.pc, self.opcode)
    }
}

/// Inspector that records the executed step sequence without altering
/// execution. Gas behavior comes entirely from the EVM env it runs under, so
/// running it with a schedule's (native) env faithfully reproduces that
/// schedule's execution — which is exactly what we want to diff against
/// baseline.
#[derive(Debug)]
pub struct StepTraceInspector {
    steps: Vec<StepRecord>,
    /// Stack of executing-contract addresses, one per open frame. The root
    /// frame's address is seeded at construction (revm's `call` hook only
    /// fires for sub-calls).
    contract_stack: Vec<Address>,
    truncated: bool,
}

impl StepTraceInspector {
    /// Create an inspector seeded with the root frame's contract (the tx
    /// recipient, or `Address::ZERO` for a contract-creation tx).
    pub fn new(root_contract: Address) -> Self {
        Self { steps: Vec::new(), contract_stack: vec![root_contract], truncated: false }
    }

    /// The recorded step sequence.
    pub fn steps(&self) -> &[StepRecord] {
        &self.steps
    }

    /// Whether the trace hit [`MAX_TRACE_STEPS`] and stopped recording.
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    fn current_contract(&self) -> Address {
        self.contract_stack.last().copied().unwrap_or(Address::ZERO)
    }
}

impl<CTX> Inspector<CTX> for StepTraceInspector
where
    CTX: ContextTr,
{
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        if self.truncated {
            return;
        }
        if self.steps.len() >= MAX_TRACE_STEPS {
            self.truncated = true;
            return;
        }
        // depth: root frame sits at contract_stack.len() == 1, so depth 0.
        let depth = (self.contract_stack.len().saturating_sub(1)) as u32;
        self.steps.push(StepRecord {
            depth,
            pc: interp.bytecode.pc() as u32,
            opcode: interp.bytecode.opcode(),
            contract: self.current_contract(),
        });
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        // `bytecode_address` is the code-holder (matches the other inspectors;
        // for DELEGATECALL this is the library, not the storage context).
        self.contract_stack.push(inputs.bytecode_address);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.contract_stack.pop();
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        // The created address isn't known until `create_end`, so steps inside
        // the init code report `Address::ZERO`. The `(depth, pc, opcode)` key
        // is unaffected, so the diff still works; only the label is blank.
        self.contract_stack.push(Address::ZERO);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.contract_stack.pop();
    }
}

/// First step where two executions part, as a [`DivergenceLocation`] built from
/// the schedule-side step.
///
/// Returns `None` when the traces are identical, when either is empty, or when
/// either was truncated (a partial trace can't be diffed safely). The common
/// case is a key mismatch at some index — a `JUMPI` that branched differently
/// because a gas-observable value changed. The fallback (identical prefix, one
/// trace shorter) covers a schedule that simply halted earlier than baseline:
/// we point at the last step the schedule executed.
pub fn first_divergence(
    baseline: &[StepRecord],
    schedule: &[StepRecord],
) -> Option<DivergenceLocation> {
    if baseline.is_empty() || schedule.is_empty() {
        return None;
    }
    let common = baseline.len().min(schedule.len());
    for i in 0..common {
        if baseline[i].key() != schedule[i].key() {
            return Some(location_from(&schedule[i]));
        }
    }
    // Identical for the whole shared length. If the schedule ran past the
    // baseline, that extra step is the divergence; if it stopped earlier,
    // point at its final executed step.
    if schedule.len() > common {
        Some(location_from(&schedule[common]))
    } else if baseline.len() > common {
        Some(location_from(&schedule[common - 1]))
    } else {
        None
    }
}

fn location_from(step: &StepRecord) -> DivergenceLocation {
    DivergenceLocation {
        contract: step.contract,
        // We don't reconstruct the per-frame selector chain on this diagnostic
        // path; downstream code treats an empty chain as "unknown".
        function_selectors: Vec::new(),
        pc: step.pc as usize,
        // 1-based to match `record_divergence` (root frame = 1).
        call_depth: step.depth as usize + 1,
        opcode: step.opcode,
        opcode_name: OpCode::new(step.opcode)
            .map_or_else(|| format!("0x{:02x}", step.opcode), |op| op.as_str().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(depth: u32, pc: u32, opcode: u8) -> StepRecord {
        StepRecord { depth, pc, opcode, contract: Address::ZERO }
    }

    #[test]
    fn identical_traces_have_no_divergence() {
        let t = vec![step(0, 0, 0x60), step(0, 2, 0x60), step(0, 4, 0x01)];
        assert!(first_divergence(&t, &t).is_none());
    }

    #[test]
    fn empty_trace_is_none() {
        let t = vec![step(0, 0, 0x60)];
        assert!(first_divergence(&[], &t).is_none());
        assert!(first_divergence(&t, &[]).is_none());
    }

    #[test]
    fn branch_mismatch_returns_schedule_step() {
        // Same prefix, then the schedule takes a different PC (e.g. JUMPI went
        // the other way).
        let base = vec![step(0, 0, 0x57), step(0, 10, 0x60), step(0, 12, 0x00)];
        let sched = vec![step(0, 0, 0x57), step(0, 99, 0xfd)];
        let loc = first_divergence(&base, &sched).expect("diverges at index 1");
        assert_eq!(loc.pc, 99);
        assert_eq!(loc.opcode, 0xfd);
        assert_eq!(loc.call_depth, 1);
    }

    #[test]
    fn schedule_halts_early_points_at_last_schedule_step() {
        let base = vec![step(0, 0, 0x60), step(0, 2, 0x60), step(0, 4, 0x01), step(0, 6, 0x00)];
        let sched = vec![step(0, 0, 0x60), step(0, 2, 0x60)];
        let loc = first_divergence(&base, &sched).expect("baseline ran longer");
        assert_eq!(loc.pc, 2);
    }

    #[test]
    fn depth_is_part_of_the_key() {
        let base = vec![step(0, 0, 0xf1), step(1, 0, 0x60)];
        let sched = vec![step(0, 0, 0xf1), step(2, 0, 0x60)];
        let loc = first_divergence(&base, &sched).expect("depth differs at index 1");
        assert_eq!(loc.call_depth, 3); // depth 2 -> 1-based 3
    }
}
