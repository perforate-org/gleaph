//! Timer-driven drain of the deferred LARA maintenance queue (ADR 0020).
//!
//! An adaptive, self-rescheduling one-shot timer (`ic-cdk-timers`) with
//! event-driven re-arm: mutation paths call [`arm_if_needed`] after enqueuing
//! maintenance work, and the canister arms from `init` / `post_upgrade`. No
//! timer is scheduled while the queue is empty; each tick chooses its successor
//! delay from the just-finished pass via [`next_delay`].
//!
//! The queued work is physical reclamation only (tombstones gate reads
//! synchronously), so deferring it never changes query visibility.

#[cfg(target_family = "wasm")]
use super::GraphStore;
#[cfg(any(test, target_family = "wasm"))]
use super::ic_budget::{MAINTENANCE_TIMER_FLOOR_DELAY, MAINTENANCE_TIMER_RELAXED_DELAY};

#[cfg(target_family = "wasm")]
thread_local! {
    /// The single in-flight maintenance timer, or `None` when the queue is
    /// drained. Rebuilt after upgrade (timers do not survive upgrades).
    static MAINTENANCE_TIMER: std::cell::RefCell<Option<ic_cdk_timers::TimerId>> =
        const { std::cell::RefCell::new(None) };
}

/// Reschedule policy from a finished maintenance pass.
///
/// `None` stops the timer (re-armed on the next enqueue); a full budget under
/// remaining backlog drains aggressively at the floor delay; a small tail uses
/// the relaxed delay.
#[cfg(any(test, target_family = "wasm"))]
fn next_delay(
    remaining_queue_len: u64,
    instruction_budget_exhausted: bool,
) -> Option<core::time::Duration> {
    if remaining_queue_len == 0 {
        None
    } else if instruction_budget_exhausted {
        Some(MAINTENANCE_TIMER_FLOOR_DELAY)
    } else {
        Some(MAINTENANCE_TIMER_RELAXED_DELAY)
    }
}

/// Schedules the maintenance timer iff the deferred queue is non-empty and no
/// timer is already armed.
///
/// Idempotent and self-guarding; safe to call from every enqueue site and from
/// canister lifecycle hooks. A no-op on non-wasm builds, where delete/finalize
/// paths drain fully inline.
pub(crate) fn arm_if_needed() {
    #[cfg(target_family = "wasm")]
    {
        if GraphStore::new().maintenance_queue_len() == 0 {
            return;
        }
        MAINTENANCE_TIMER.with_borrow_mut(|slot| {
            if slot.is_none() {
                *slot = Some(schedule(MAINTENANCE_TIMER_FLOOR_DELAY));
            }
        });
    }
}

/// Registers a one-shot timer that runs [`on_tick`]. `ic-cdk-timers` 1.0 takes a
/// future; the tick body is synchronous, so a non-suspending async block wraps it.
#[cfg(target_family = "wasm")]
fn schedule(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    ic_cdk_timers::set_timer(delay, async { on_tick() })
}

/// Runs one budgeted maintenance pass, then reschedules per [`next_delay`].
#[cfg(target_family = "wasm")]
fn on_tick() {
    // The fired one-shot is consumed; clear before running so the reschedule
    // below installs exactly one successor.
    MAINTENANCE_TIMER.with_borrow_mut(|slot| *slot = None);

    let next = match GraphStore::new().run_timer_maintenance_tick() {
        Ok(report) => next_delay(
            report.remaining_queue_len(),
            report.instruction_budget_exhausted,
        ),
        // A Rust-level error leaves the stable queue intact; retry at the floor.
        Err(_) => Some(MAINTENANCE_TIMER_FLOOR_DELAY),
    };

    if let Some(delay) = next {
        let id = schedule(delay);
        MAINTENANCE_TIMER.with_borrow_mut(|slot| *slot = Some(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_stops_the_timer() {
        assert_eq!(next_delay(0, false), None);
        assert_eq!(next_delay(0, true), None);
    }

    #[test]
    fn exhausted_budget_with_backlog_uses_floor_delay() {
        assert_eq!(next_delay(5, true), Some(MAINTENANCE_TIMER_FLOOR_DELAY));
    }

    #[test]
    fn small_tail_uses_relaxed_delay() {
        assert_eq!(next_delay(5, false), Some(MAINTENANCE_TIMER_RELAXED_DELAY));
        assert!(MAINTENANCE_TIMER_RELAXED_DELAY > MAINTENANCE_TIMER_FLOOR_DELAY);
    }
}
