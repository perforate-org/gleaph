//! Test-only (`pocket-ic-e2e`) Graph fault injection.
//!
//! The Router's Confirm reads the replicated `Acquire` proof, moves the reservation
//! `Reserved → Committed` (stamping `pending_acquire_ack`), then acks (unpins) the effect. This seam
//! lets the failure-injection e2e suite trap **inside the ack** so the effect stays pinned and the
//! reservation keeps its `pending_acquire_ack` — reproducing the Confirm→ack boundary (and keeping a
//! pinned `Acquire` durable across the 9-day mutation-journal eviction window) that the Router-side
//! `test_fault` cannot reach. The armed flag is a committed heap flag set by its own `e2e_*` ingress;
//! it survives a trap in a later message and is cleared by re-arming with `0`.
//!
//! The ordered-write seam models a reply lost only after an atomic ordered mutation and its
//! postings are durable. It leaves a one-shot reject marker so an incorrect redispatch is directly
//! observable. Both seams are heap-only and reset on Graph upgrade.
//!
//! Compiled only under `pocket-ic-e2e`; call sites in `canister::handlers` are `#[cfg]`-gated, so
//! production builds contain none of this.

use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InjectedFault {
    None,
    /// Trap in `ack_unique_effects`. The Router's Confirm has already durably moved the reservation
    /// `Reserved → Committed` and stamped `pending_acquire_ack`; the ack rejection leaves the
    /// `Acquire` pinned, so slice-6 recovery must re-ack it and clear the pending marker.
    TrapOnUniqueAck,
}

thread_local! {
    static FAULT: Cell<InjectedFault> = const { Cell::new(InjectedFault::None) };
    static ORDERED_FAULT: Cell<OrderedFaultState> = const { Cell::new(OrderedFaultState::new()) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OrderedFaultState {
    response_loss_armed: bool,
    reject_next_ordered_entry: bool,
    dispatch_count: u64,
}

impl OrderedFaultState {
    const fn new() -> Self {
        Self {
            response_loss_armed: false,
            reject_next_ordered_entry: false,
            dispatch_count: 0,
        }
    }
}

pub(crate) fn arm(fault: InjectedFault) {
    FAULT.with(|f| f.set(fault));
}

/// Arm the one-shot ordered response-loss seam and reset its observables.
pub(crate) fn arm_ordered_response_loss() {
    ORDERED_FAULT.with(|state| {
        state.set(OrderedFaultState {
            response_loss_armed: true,
            ..OrderedFaultState::new()
        })
    });
}

/// Clear the ordered response-loss seam and reset its observables.
pub(crate) fn clear_ordered_response_loss() {
    ORDERED_FAULT.with(|state| state.set(OrderedFaultState::new()));
}

/// Count one atomic ordered handler entry and consume a one-shot reject marker, if present.
pub(crate) fn begin_ordered_dispatch() -> bool {
    ORDERED_FAULT.with(|state| {
        let mut current = state.get();
        current.dispatch_count = current.dispatch_count.saturating_add(1);
        let reject = current.reject_next_ordered_entry;
        current.reject_next_ordered_entry = false;
        state.set(current);
        reject
    })
}

/// Consume the one-shot post-commit response-loss arm and leave the reject-next marker armed.
pub(crate) fn inject_ordered_response_loss() -> bool {
    ORDERED_FAULT.with(|state| {
        let mut current = state.get();
        if !current.response_loss_armed {
            return false;
        }
        current.response_loss_armed = false;
        current.reject_next_ordered_entry = true;
        state.set(current);
        true
    })
}

pub(crate) fn ordered_dispatch_state() -> (u64, bool) {
    ORDERED_FAULT.with(|state| {
        let current = state.get();
        (current.dispatch_count, current.reject_next_ordered_entry)
    })
}

/// Map a candid-friendly code to a fault (`0` clears). Unknown codes are rejected by the caller.
pub(crate) fn fault_from_code(code: u8) -> Option<InjectedFault> {
    match code {
        0 => Some(InjectedFault::None),
        1 => Some(InjectedFault::TrapOnUniqueAck),
        _ => None,
    }
}

fn armed() -> InjectedFault {
    FAULT.with(Cell::get)
}

/// Trap if [`InjectedFault::TrapOnUniqueAck`] is armed (call at the top of `ack_unique_effects`).
pub(crate) fn maybe_trap_on_unique_ack() {
    if armed() == InjectedFault::TrapOnUniqueAck {
        ic_cdk::trap("pocket-ic-e2e injected fault: trap on unique-effect ack");
    }
}
