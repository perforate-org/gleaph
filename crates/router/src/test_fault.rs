//! Test-only (`pocket-ic-e2e`) fault injection for Router write-path durable boundaries.
//!
//! The armed fault is a committed heap flag, set by its own `test_arm_fault` ingress and read by the
//! gql write path. On the IC a trap rolls back only the trapping message's state, so a flag set in a
//! *prior, committed* message survives the trap; the test then clears it with a separate
//! `test_arm_fault(0)` ingress before driving recovery. This lets a single injected trap reproduce a
//! real partial-failure boundary without leaving the canister wedged in a trap loop.
//!
//! Compiled only under `pocket-ic-e2e`; the call sites in `gql.rs` are `#[cfg]`-gated, so production
//! builds contain none of this.

use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InjectedFault {
    None,
    /// Trap after the no-`await` Try, before the first dispatch `await`. Because the reservation and
    /// envelope co-commit only at that first `await`, the trap rolls them back with the message.
    TrapAfterTry,
    /// Trap in the post-dispatch callback before Confirm. The shard's canonical write and pinned
    /// `Acquire` are already durable; only the Router-side Confirm is rolled back, leaving the
    /// reservation `Reserved` (a commit-but-reply-lost boundary for recovery to converge).
    TrapBeforeConfirm,
    /// Trap after the typed Graph batch has committed but before Router target/projection
    /// convergence. The durable typed replay record must recover this ambiguous boundary.
    TrapAfterTypedGraphCommit,
}

thread_local! {
    static FAULT: Cell<InjectedFault> = const { Cell::new(InjectedFault::None) };
    static TYPED_BATCH_TRACE: RefCell<String> = const { RefCell::new(String::new()) };
}

pub(crate) fn arm(fault: InjectedFault) {
    FAULT.with(|f| f.set(fault));
}

pub(crate) fn record_typed_batch_trace(stage: impl Into<String>) {
    TYPED_BATCH_TRACE.with_borrow_mut(|trace| *trace = stage.into());
}

pub(crate) fn typed_batch_trace() -> String {
    TYPED_BATCH_TRACE.with_borrow(Clone::clone)
}

/// Map a candid-friendly code to a fault (`0` clears). Unknown codes are rejected by the caller.
pub(crate) fn fault_from_code(code: u8) -> Option<InjectedFault> {
    match code {
        0 => Some(InjectedFault::None),
        1 => Some(InjectedFault::TrapAfterTry),
        2 => Some(InjectedFault::TrapBeforeConfirm),
        3 => Some(InjectedFault::TrapAfterTypedGraphCommit),
        _ => None,
    }
}

fn armed() -> InjectedFault {
    FAULT.with(Cell::get)
}

/// Trap if [`InjectedFault::TrapAfterTry`] is armed (call before the first dispatch `await`).
pub(crate) fn maybe_trap_after_try() {
    if armed() == InjectedFault::TrapAfterTry {
        ic_cdk::trap("pocket-ic-e2e injected fault: trap after Try (before dispatch)");
    }
}

/// Trap if [`InjectedFault::TrapBeforeConfirm`] is armed (call in the post-dispatch callback before
/// Confirm).
pub(crate) fn maybe_trap_before_confirm() {
    if armed() == InjectedFault::TrapBeforeConfirm {
        ic_cdk::trap("pocket-ic-e2e injected fault: trap before Confirm (after canonical commit)");
    }
}

pub(crate) fn maybe_trap_after_typed_graph_commit() {
    if armed() == InjectedFault::TrapAfterTypedGraphCommit {
        ic_cdk::trap(
            "pocket-ic-e2e injected fault: trap after typed Graph commit before Router convergence",
        );
    }
}
