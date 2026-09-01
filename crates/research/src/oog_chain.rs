//! OOG-chain classifier: walk a transaction's call frames from root to the
//! out-of-gas frame and decide whether the failure is recoverable by simply
//! sending more gas (every parent→child hop got proportional gas via the
//! EIP-150 63/64 cap) or whether some frame in the chain throttled gas with a
//! hardcoded amount (`.transfer()` 2300 stipend, fixed constant, or fractional
//! patterns like `gas() / 2`).
//!
//! See the `Analyzer` ExEx in `bin/reth-research` for how this is wired into
//! the divergence pipeline; the outputs feed into the `oog_chain_proportional`,
//! `oog_bottleneck_depth`, and `oog_bottleneck_kind` columns of `divergences`.

use crate::divergence::{CallFrame, CallType};

/// Tolerance applied to the EIP-150 63/64 cap when judging proportionality.
///
/// EIP-150 rounds the cap down to integer arithmetic (`floor(parent * 63 /
/// 64)`), and the parent's gas at the moment we read it differs from the
/// gas-after-call-cost figure by a small constant (the CALL/CREATE base cost,
/// access cost, and value-transfer stipend). 100 gas absorbs all of those for
/// the purpose of deciding "did the caller pass essentially `gas()`".
const PROPORTIONAL_TOLERANCE: u64 = 100;

/// Absolute threshold below which a non-stipend stack-gas value is classified
/// as a small fixed constant (e.g. hand-rolled gas budgets like 30k, 50k,
/// 100k) rather than a fractional pattern (`gas() / N`).
///
/// Solidity's `.transfer()` / `.send()` use 2300 (handled separately as
/// `Stipend2300`); other commonly seen hardcoded values are well under 100k.
/// Above this threshold the caller is more likely to be passing a meaningful
/// fraction of available gas, even if not the full 63/64 cap.
const FIXED_GAS_THRESHOLD: u64 = 100_000;

/// Type of throttle at the bottleneck frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OogBottleneckKind {
    /// Caller pushed exactly 2300 onto the stack — Solidity `.transfer()` /
    /// `.send()` stipend.
    Stipend2300,
    /// Caller pushed a fraction of the available cap (e.g. `gas() / 2`,
    /// `gas() / 4`). Heuristic: stack-gas <= cap / 2.
    FractionalGas,
    /// Caller pushed a hardcoded gas value that doesn't fit the other two
    /// patterns (small or medium fixed constant).
    FixedGas,
}

impl OogBottleneckKind {
    /// Stable string form for the `oog_bottleneck_kind` DB column.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Stipend2300 => "Stipend2300",
            Self::FractionalGas => "FractionalGas",
            Self::FixedGas => "FixedGas",
        }
    }
}

/// Result of the chain-walk classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OogChainAnalysis {
    /// Whether every parent→child hop on the path from the tx root to the
    /// OOG frame received gas via the EIP-150 63/64 cap. When true, raising
    /// the wallet's outer gas limit propagates through every hop and can in
    /// principle clear the OOG.
    pub proportional: bool,
    /// `CallFrame.depth` (0-based, root = 0) of the first throttled child
    /// walking root → OOG. `None` when the chain is fully proportional.
    pub bottleneck_depth: Option<u64>,
    /// Type of throttle at the bottleneck. `None` when proportional, or when
    /// the throttle was detected via missing call-frame data.
    pub bottleneck_kind: Option<OogBottleneckKind>,
}

impl OogChainAnalysis {
    /// All-proportional result. Trivially returned for OOGs at depth 0
    /// (single-frame transactions) where there are no parent→child hops to
    /// classify.
    const fn proportional() -> Self {
        Self { proportional: true, bottleneck_depth: None, bottleneck_kind: None }
    }

    const fn throttled(depth: u64, kind: Option<OogBottleneckKind>) -> Self {
        Self { proportional: false, bottleneck_depth: Some(depth), bottleneck_kind: kind }
    }
}

/// Classify the chain of frames from root to the OOG frame.
///
/// `oog_call_depth` is the 1-based call depth as recorded by the inspector in
/// `OutOfGasInfo.call_depth` (root frame = 1). It is converted to the 0-based
/// `CallFrame.depth` internally.
///
/// Returns `None` if the call frames are empty or the OOG frame can't be
/// located in the recorded chain.
pub fn classify_oog_chain(
    schedule_call_frames: &[CallFrame],
    oog_call_depth: usize,
) -> Option<OogChainAnalysis> {
    // Convert 1-based OOG depth to 0-based frame depth. An OOG at the root
    // frame is reported with call_depth = 1 by the inspector but corresponds
    // to a CallFrame with depth = 0.
    let oog_frame_depth = oog_call_depth.checked_sub(1)?;

    // OOG at the root frame is trivially proportional: there are no
    // parent→child hops on the path. More wallet gas would simply raise
    // the root's budget (and, by 63/64, every descendant frame's budget),
    // so the OOG is wallet-fixable regardless of what happened in any
    // subcalls. Short-circuit here so we don't depend on the root frame
    // appearing in `schedule_call_frames` — revm's `call`/`call_end`
    // hooks fire for sub-calls only, so the root frame is rarely present.
    if oog_frame_depth == 0 {
        return Some(OogChainAnalysis::proportional());
    }

    if schedule_call_frames.is_empty() {
        return None;
    }

    // Locate the OOG frame: the LAST frame in the array (post-order DFS) at
    // exactly oog_frame_depth that did not succeed. We use `rposition`
    // because frames at the same depth can repeat across siblings, and the
    // failed frame is normally the most recently completed one at that depth.
    let oog_idx = schedule_call_frames
        .iter()
        .rposition(|f| f.depth == oog_frame_depth && !f.success)
        .or_else(|| {
            // Fallback: the inspector occasionally records the OOG depth one
            // off when the failure is in the parent's call accounting rather
            // than in the child frame itself. Accept any frame at exactly
            // oog_frame_depth as a last resort.
            schedule_call_frames.iter().rposition(|f| f.depth == oog_frame_depth)
        })?;

    let chain = ancestor_chain(schedule_call_frames, oog_idx);
    Some(classify_chain(&chain))
}

/// Reconstruct the chain of ancestors from root to `oog_idx`, returned in
/// root-first order. Call frames are emitted in post-order DFS by the
/// inspector — children's `call_end` fires before the parent's — so an
/// ancestor of `frames[oog_idx]` is the FIRST subsequent frame whose depth
/// equals (`current_depth` - 1), iterating forward from `oog_idx`.
fn ancestor_chain(frames: &[CallFrame], oog_idx: usize) -> Vec<&CallFrame> {
    let oog = &frames[oog_idx];
    let mut chain = vec![oog];
    let mut want_depth = oog.depth.checked_sub(1);

    for frame in &frames[oog_idx + 1..] {
        let Some(target) = want_depth else { break };
        if frame.depth == target {
            chain.push(frame);
            if target == 0 {
                break;
            }
            want_depth = Some(target - 1);
        }
    }

    chain.reverse(); // root first, OOG last
    chain
}

/// Classify each parent→child hop in `chain` (root-first). Returns
/// proportional iff every hop is proportional or specially handled
/// (CREATE/CREATE2 always auto-forward 63/64, so we treat them as
/// proportional).
///
/// Each frame's `gas_requested_on_stack` / `parent_gas_at_call` describes
/// the hop from *its caller* into that frame, so a chain of length N
/// represents N parent→child hops. The previous version of this function
/// skipped `chain[0]` on the (incorrect) assumption that `chain[0]` was
/// the synthetic root frame. revm never fires `call_end` on the root, so
/// in practice `chain[0]` is the topmost *captured* ancestor — its fields
/// describe the uncaptured-root → chain[0] hop, and dropping it on the
/// floor hides `Stipend2300` / `FixedGas` patterns at depth 1 (the
/// `.transfer()` / `.send()` case that this classifier exists to catch).
fn classify_chain(chain: &[&CallFrame]) -> OogChainAnalysis {
    for frame in chain {
        // Defensive: skip a captured root frame (depth == 0) if a future
        // inspector ever surfaces one. The root has no caller in the chain,
        // so there's nothing to classify.
        if frame.depth == 0 {
            continue;
        }
        // CREATE / CREATE2 don't take a gas argument from the stack; the EVM
        // forwards floor(parent_gas_after_create_cost * 63/64) automatically.
        // Treat as proportional regardless of `gas_requested_on_stack`.
        if matches!(frame.call_type, CallType::Create | CallType::Create2) {
            continue;
        }

        let (Some(stack_gas), Some(parent_gas)) =
            (frame.gas_requested_on_stack, frame.parent_gas_at_call)
        else {
            // Missing data — conservatively report as throttled at this depth
            // with no kind. This shouldn't happen for runs after the data
            // capture lands, but old DB rows or partial captures fall here.
            return OogChainAnalysis::throttled(frame.depth as u64, None);
        };

        let cap = parent_gas.saturating_mul(63) / 64;
        let cap_minus_tol = cap.saturating_sub(PROPORTIONAL_TOLERANCE);
        if stack_gas >= cap_minus_tol {
            continue; // proportional hop
        }

        // Throttled. Distinguish the kinds:
        // - Exactly 2300 → Solidity `.transfer()`/`.send()` stipend.
        // - Small absolute value (< FIXED_GAS_THRESHOLD) → hardcoded constant.
        // - Otherwise → caller passed a fraction of available gas (still less than the 63/64 cap,
        //   otherwise we'd be in the proportional branch above).
        let kind = if stack_gas == 2300 {
            OogBottleneckKind::Stipend2300
        } else if stack_gas < FIXED_GAS_THRESHOLD {
            OogBottleneckKind::FixedGas
        } else {
            OogBottleneckKind::FractionalGas
        };
        return OogChainAnalysis::throttled(frame.depth as u64, Some(kind));
    }

    OogChainAnalysis::proportional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    /// Build a minimal CallFrame for tests.
    fn frame(
        depth: usize,
        success: bool,
        call_type: CallType,
        gas_requested_on_stack: Option<u64>,
        parent_gas_at_call: Option<u64>,
    ) -> CallFrame {
        CallFrame {
            call_index: 0,
            depth,
            from: Address::ZERO,
            to: Some(Address::ZERO),
            call_type,
            gas_provided: 0,
            gas_used: 0,
            success,
            input: None,
            output: None,
            repricing_gas_delta: 0,
            value_wei: None,
            caller_pc: None,
            was_precompile: false,
            precompile_address: None,
            gas_remaining_at_fail: None,
            storage_target: None,
            gas_requested_on_stack,
            parent_gas_at_call,
        }
    }

    /// Post-order DFS sequence: children complete before their parent.
    /// `frames[0]` is the deepest-completed; `frames.last()` is the root.

    #[test]
    fn empty_call_frames_with_subcall_oog_returns_none() {
        // OOG at depth 1 (sub-call) but no call frames captured — can't
        // walk the chain. Stays unclassified.
        assert!(classify_oog_chain(&[], 2).is_none());
    }

    #[test]
    fn empty_call_frames_with_root_oog_is_proportional() {
        // Root-frame OOG that the inspector didn't capture per-frame:
        // there are no hops to throttle, so trivially proportional. This
        // covers the common case where reth's `call_end` hook doesn't
        // fire for the root frame and we synthesize a root-OOG record on
        // the producer side.
        let res = classify_oog_chain(&[], 1).expect("root OOG always classifies");
        assert!(res.proportional);
        assert_eq!(res.bottleneck_depth, None);
        assert_eq!(res.bottleneck_kind, None);
    }

    #[test]
    fn oog_in_root_is_proportional() {
        // Single-frame tx: only the root, OOG at depth 0 (oog_call_depth=1).
        let frames = vec![frame(0, false, CallType::Call, None, None)];
        let res = classify_oog_chain(&frames, 1).unwrap();
        assert!(res.proportional, "OOG at depth 0 should be wallet-fixable");
        assert_eq!(res.bottleneck_depth, None);
    }

    #[test]
    fn all_proportional_chain_is_wallet_fixable() {
        // root → call → call where every child got effectively all available gas.
        // depth 2 frame gets 9_840_000 against parent (10M); cap = 9_843_750.
        // depth 1 frame gets 9_843_750 against parent (10M); cap = 9_843_750.
        let frames = vec![
            frame(2, false, CallType::Call, Some(u64::MAX), Some(10_000_000)),
            frame(1, true, CallType::Call, Some(u64::MAX), Some(10_000_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 3).unwrap();
        assert!(res.proportional, "all-proportional chain should be wallet-fixable");
    }

    #[test]
    fn stipend_2300_is_detected() {
        // root → call(2300, ...) → OOG. Classic .transfer()/.send() pattern.
        let frames = vec![
            frame(1, false, CallType::Call, Some(2300), Some(100_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_depth, Some(1));
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::Stipend2300));
    }

    #[test]
    fn fixed_gas_constant_is_detected() {
        // root → call(50_000) → OOG. Caller hardcoded 50k against a 1M
        // available budget; cap would be ~984k.
        let frames = vec![
            frame(1, false, CallType::Call, Some(50_000), Some(1_000_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::FixedGas));
    }

    #[test]
    fn fractional_gas_is_detected() {
        // call(gas() / 2): caller passed half of available.
        let frames = vec![
            frame(1, false, CallType::Call, Some(500_000), Some(1_000_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::FractionalGas));
    }

    #[test]
    fn delegatecall_with_gas_is_proportional() {
        // DELEGATECALL with caller passing `gas()` (= u64::MAX after saturate)
        // is proportional even though it isn't a regular CALL.
        let frames = vec![
            frame(1, false, CallType::DelegateCall, Some(u64::MAX), Some(500_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(res.proportional);
    }

    #[test]
    fn create_is_treated_as_proportional() {
        // CREATE doesn't have a stack-gas argument; auto-forwards 63/64.
        let frames = vec![
            frame(1, false, CallType::Create, None, Some(500_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(res.proportional, "CREATE should be treated as proportional");
    }

    #[test]
    fn missing_data_falls_back_to_throttled() {
        // No gas_requested_on_stack on the depth-1 frame and not a CREATE.
        // Conservative classification: throttled with no kind.
        let frames = vec![
            frame(1, false, CallType::Call, None, Some(500_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_kind, None);
    }

    #[test]
    fn first_bottleneck_is_reported_when_chain_has_multiple_throttles() {
        // Two-hop chain: depth 1 throttled by .transfer() stipend; depth 2 also
        // hardcoded. We should report the OUTER (first encountered walking
        // root → OOG = lowest depth) bottleneck — that's the one to fix.
        let frames = vec![
            // Post-order: deepest frame first.
            frame(2, false, CallType::Call, Some(50_000), Some(80_000)),
            frame(1, true, CallType::Call, Some(2300), Some(500_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 3).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_depth, Some(1));
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::Stipend2300));
    }

    #[test]
    fn tolerance_absorbs_call_overhead() {
        // Caller passed exactly the cap minus a few tens of gas (call cost
        // overhead). Should still classify as proportional.
        // parent=1M, cap=floor(1M * 63 / 64)=984_375. Pass 984_300 (75 short).
        let frames = vec![
            frame(1, false, CallType::Call, Some(984_300), Some(1_000_000)),
            frame(0, true, CallType::Call, None, None),
        ];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(res.proportional, "small under-cap should still be proportional");
    }

    #[test]
    fn bottleneck_kind_str_is_stable() {
        assert_eq!(OogBottleneckKind::Stipend2300.as_str(), "Stipend2300");
        assert_eq!(OogBottleneckKind::FractionalGas.as_str(), "FractionalGas");
        assert_eq!(OogBottleneckKind::FixedGas.as_str(), "FixedGas");
    }

    // ── Regression tests for the "root frame is uncaptured" path ──
    //
    // revm doesn't fire `call_end` on the root, so in real runs the OOG
    // frame's chain ends with the topmost captured ancestor (not the
    // root). Prior to the §2.1 fix, `classify_chain` skipped `chain[0]`
    // unconditionally, dropping the only hop available to inspect when
    // the OOG was at depth 1.

    #[test]
    fn depth_1_oog_with_stipend_2300_is_throttled_without_captured_root() {
        // Realistic shape from a `payable(addr).transfer(value)` flow: root
        // frame isn't in `frames` (revm never fires `call_end` for it).
        // The depth-1 frame's `gas_requested_on_stack = 2300` describes
        // the root → depth-1 hop, and `classify_chain` must surface it as
        // `Stipend2300` rather than swallowing the chain as proportional.
        let frames = vec![frame(1, false, CallType::Call, Some(2300), Some(100_000))];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_depth, Some(1));
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::Stipend2300));
    }

    #[test]
    fn depth_1_oog_with_gas_call_is_proportional_without_captured_root() {
        // DELEGATECALL forwarding `gas()` from the (uncaptured) root frame:
        // single-frame chain, stack-gas saturates to u64::MAX, every hop
        // proportional → wallet-fixable deep chain.
        let frames = vec![frame(1, false, CallType::DelegateCall, Some(u64::MAX), Some(500_000))];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(res.proportional);
        assert_eq!(res.bottleneck_depth, None);
        assert_eq!(res.bottleneck_kind, None);
    }

    #[test]
    fn depth_1_oog_with_fixed_30k_gas_is_throttled_without_captured_root() {
        // Common pattern: `target.call{gas: 30_000}(...)`. Even though the
        // OOG is at depth 1, the hop is throttled — adding more wallet gas
        // can't push more than 30k through to the callee.
        let frames = vec![frame(1, false, CallType::Call, Some(30_000), Some(500_000))];
        let res = classify_oog_chain(&frames, 2).unwrap();
        assert!(!res.proportional);
        assert_eq!(res.bottleneck_kind, Some(OogBottleneckKind::FixedGas));
    }
}
