//! Test-only (`pocket-ic-e2e`) fault injection for the ADR 0030 graph-shard unique-effect ack path.
//!
//! The Router's Confirm reads the replicated `Acquire` proof, moves the reservation
//! `Reserved → Committed` (stamping `pending_acquire_ack`), then acks (unpins) the effect. This seam
//! lets the failure-injection e2e suite trap **inside the ack** so the effect stays pinned and the
//! reservation keeps its `pending_acquire_ack` — reproducing the Confirm→ack boundary (and keeping a
//! pinned `Acquire` durable across the 9-day mutation-journal eviction window) that the Router-side
//! `test_fault` cannot reach. The armed flag is a committed heap flag set by its own `e2e_*` ingress;
//! it survives a trap in a later message and is cleared by re-arming with `0`.
//!
//! Compiled only under `pocket-ic-e2e`; the call site in `canister::handlers` is `#[cfg]`-gated, so
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
}

pub(crate) fn arm(fault: InjectedFault) {
    FAULT.with(|f| f.set(fault));
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
