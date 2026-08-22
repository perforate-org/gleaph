//! Test-only (`pocket-ic-e2e`) fault injection for Router write-path durable boundaries.
//!
//! The armed fault is a committed heap flag, set by its own `test_arm_fault` ingress and read by
//! the GQL and Vector recovery paths. Trap faults roll back only the trapping message's state,
//! while application faults return after a durable transition. Codes 8 and 9 persistently drop a
//! decoded Vector batch reply or frontier reply before the Router's durable follow-up; the test
//! clears any armed fault with a separate `test_arm_fault(0)` ingress before driving recovery. This
//! reproduces partial-failure boundaries without leaving the canister wedged in a fault loop.
//!
//! Compiled only under `pocket-ic-e2e`; the call sites in `gql.rs` and `vector_sync.rs` are
//! `#[cfg]`-gated, so production builds contain none of this.

use std::cell::Cell;

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
    /// Return an application error after an ordered Graph receipt is durably recorded but before
    /// Router projection/retirement convergence. The ordered recovery driver must finish it.
    FailAfterOrderedCanonicalCommit,
    /// Return an application error after Graph durably retires an ordered mutation but before
    /// Router records the terminal completed state. Recovery must repeat the idempotent retirement
    /// call and finish the Router transition.
    FailAfterOrderedRetirementAck,
    /// Trap immediately after the durable bulk-load Start counter write, before the parent/client
    /// binding insert. IC message rollback must restore the counter.
    TrapAfterBulkStartCounter,
    /// Trap immediately after the durable bulk-load Start parent/client binding insert. IC message
    /// rollback must restore both the parent and the counter.
    TrapAfterBulkStartParent,
    /// Drop a successfully decoded Router→Vector batch response before the caller applies it to
    /// the durable outbox. The armed fault remains active until `test_arm_fault(0)`.
    DropAfterVectorBatchResult,
    /// Drop a successful Vector frontier response after watermark/GC work but before the Router
    /// retires its captured frontier snapshot. The armed fault remains active until clear.
    DropAfterFrontierReply,
}

thread_local! {
    static FAULT: Cell<InjectedFault> = const { Cell::new(InjectedFault::None) };
}

pub(crate) fn arm(fault: InjectedFault) {
    FAULT.with(|f| f.set(fault));
}

/// Map a candid-friendly code to a fault (`0` clears). Unknown codes are rejected by the caller.
pub(crate) fn fault_from_code(code: u8) -> Option<InjectedFault> {
    match code {
        0 => Some(InjectedFault::None),
        1 => Some(InjectedFault::TrapAfterTry),
        2 => Some(InjectedFault::TrapBeforeConfirm),
        4 => Some(InjectedFault::FailAfterOrderedCanonicalCommit),
        5 => Some(InjectedFault::FailAfterOrderedRetirementAck),
        6 => Some(InjectedFault::TrapAfterBulkStartCounter),
        7 => Some(InjectedFault::TrapAfterBulkStartParent),
        8 => Some(InjectedFault::DropAfterVectorBatchResult),
        9 => Some(InjectedFault::DropAfterFrontierReply),
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

pub(crate) fn fail_after_ordered_canonical_commit() -> bool {
    armed() == InjectedFault::FailAfterOrderedCanonicalCommit
}

pub(crate) fn fail_after_ordered_retirement_ack() -> bool {
    armed() == InjectedFault::FailAfterOrderedRetirementAck
}

pub(crate) fn maybe_trap_after_bulk_start_counter() {
    if armed() == InjectedFault::TrapAfterBulkStartCounter {
        ic_cdk::trap("pocket-ic-e2e injected fault: trap after bulk Start counter write");
    }
}

pub(crate) fn maybe_trap_after_bulk_start_parent() {
    if armed() == InjectedFault::TrapAfterBulkStartParent {
        ic_cdk::trap("pocket-ic-e2e injected fault: trap after bulk Start parent insert");
    }
}

/// Return whether a decoded Router→Vector result must be treated as lost before durable outbox
/// application. This is intentionally persistent; only `test_arm_fault(0)` clears it.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "the decoded inter-canister reply seam runs on wasm"
    )
)]
pub(crate) fn drop_after_vector_batch_result() -> bool {
    armed() == InjectedFault::DropAfterVectorBatchResult
}

/// Return whether a successful Vector frontier reply must be treated as lost before exact marker
/// retirement. This is intentionally persistent; only `test_arm_fault(0)` clears it.
pub(crate) fn drop_after_frontier_reply() -> bool {
    armed() == InjectedFault::DropAfterFrontierReply
}
