//! Autonomous federated-saga recovery driver (ADR 0029 Phase 4).
//!
//! A self-rescheduling one-shot timer (`ic-cdk-timers`) that converges non-terminal
//! federated mutations without a client in the loop. It is armed event-driven from the
//! mutation path (after an idempotent DML leaves a saga non-terminal) and from canister
//! lifecycle hooks (`init` / `post_upgrade`), since timers do not survive an upgrade.
//!
//! Scope is deliberately **projection-only**: each tick scans a bounded slice of the
//! client-mutation journal for recoverable sagas (canonical writes already durable, only
//! label-stats projection lagging) and drives them forward with idempotent,
//! cursor-guarded projection advancement via [`crate::gql::recover_mutation_record`]. The
//! driver never re-dispatches canonical DML — that is the single operation that risks
//! double-apply, and is left to explicit retry-driven recovery surfaced through
//! `mutation_status`. A stuck *routing* reservation is reclaimed separately, by lease
//! expiry on the next retry (see `ROUTING_LEASE_TTL_NS`), not by this timer.
//!
//! Liveness is autonomous; observability is pull-based: a recovered saga becomes visible
//! through `AtLeast(token)` reads succeeding and through the `mutation_status` query.

/// Records examined per tick. Bounds the per-tick instruction cost; the round-robin cursor
/// resumes the scan on the next tick so a large journal is still fully covered.
#[cfg(target_family = "wasm")]
const RECOVERY_SCAN_BUDGET: usize = 16;

/// Delay between ticks while a lap is still in progress (more keyspace to scan).
#[cfg(target_family = "wasm")]
const RECOVERY_FLOOR_DELAY: core::time::Duration = core::time::Duration::from_secs(2);

/// Delay before starting a fresh lap when the previous lap still found recoverable sagas
/// (e.g. a shard whose graph projection had not yet caught up). Backs off so a persistently
/// lagging shard is retried without hot-looping.
#[cfg(target_family = "wasm")]
const RECOVERY_RELAXED_DELAY: core::time::Duration = core::time::Duration::from_secs(30);

#[cfg(target_family = "wasm")]
thread_local! {
    /// The single in-flight recovery timer, or `None` when idle. Rebuilt after upgrade.
    static RECOVERY_TIMER: std::cell::RefCell<Option<ic_cdk_timers::TimerId>> =
        const { std::cell::RefCell::new(None) };
    /// `true` while an async tick is in flight; keeps a concurrent [`arm_if_needed`] from
    /// scheduling an overlapping pass during the tick's awaits.
    static RECOVERY_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Round-robin scan cursor over the client-mutation journal. `None` starts a fresh lap.
    static RECOVERY_CURSOR: std::cell::RefCell<
        Option<crate::facade::stable::label_stats::ClientMutationKey>,
    > = const { std::cell::RefCell::new(None) };
    /// `true` if the lap currently in progress has found at least one recoverable saga; used
    /// to decide whether to start another lap once the cursor wraps.
    static RECOVERY_LAP_FOUND: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Schedules the recovery timer iff one is not already armed or running. Idempotent and
/// self-guarding; safe to call from every mutation site and from lifecycle hooks. A no-op
/// on non-wasm builds, where there is no timer runtime.
pub(crate) fn arm_if_needed() {
    #[cfg(target_family = "wasm")]
    {
        if RECOVERY_RUNNING.with(std::cell::Cell::get) {
            return;
        }
        RECOVERY_TIMER.with_borrow_mut(|slot| {
            if slot.is_none() {
                *slot = Some(schedule(RECOVERY_FLOOR_DELAY));
            }
        });
    }
}

#[cfg(target_family = "wasm")]
fn schedule(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    ic_cdk_timers::set_timer(delay, on_tick())
}

/// Runs one bounded recovery pass, then reschedules per the lap state.
#[cfg(target_family = "wasm")]
async fn on_tick() {
    RECOVERY_TIMER.with_borrow_mut(|slot| *slot = None);
    RECOVERY_RUNNING.with(|r| r.set(true));

    let next = run_recovery_pass().await;

    RECOVERY_RUNNING.with(|r| r.set(false));
    if let Some(delay) = next {
        let id = schedule(delay);
        RECOVERY_TIMER.with_borrow_mut(|slot| *slot = Some(id));
    }
}

/// Scans the next bounded slice of the client-mutation journal and drives any recoverable
/// sagas toward terminal. Returns the reschedule delay (`None` stops the timer until the
/// next mutation re-arms it).
#[cfg(target_family = "wasm")]
async fn run_recovery_pass() -> Option<core::time::Duration> {
    use crate::facade::store::RouterStore;

    let store = RouterStore::new();
    let start = RECOVERY_CURSOR.with_borrow(Clone::clone);
    if start.is_none() {
        // Beginning a fresh lap.
        RECOVERY_LAP_FOUND.with(|f| f.set(false));
    }

    let (keys, last_examined, scanned) =
        store.scan_recoverable_mutations(start.as_ref(), RECOVERY_SCAN_BUDGET);
    if !keys.is_empty() {
        RECOVERY_LAP_FOUND.with(|f| f.set(true));
    }
    for key in keys {
        // Best-effort: a transient failure (e.g. graph briefly unavailable) leaves the saga
        // non-terminal so the next lap retries it.
        let _ = crate::gql::recover_mutation_record(&store, &key).await;
    }

    // Advance the cursor. A short scan (fewer than the budget) means we reached the end of
    // the keyspace, so reset to start a fresh lap next time.
    let lap_complete = scanned < RECOVERY_SCAN_BUDGET as u32;
    let next_cursor = if lap_complete { None } else { last_examined };
    RECOVERY_CURSOR.with_borrow_mut(|c| *c = next_cursor.clone());

    if next_cursor.is_some() {
        // Mid-lap: keep scanning promptly.
        return Some(RECOVERY_FLOOR_DELAY);
    }
    // Lap complete: start another (backed-off) lap only if this lap found work; otherwise
    // stop and let the next mutation re-arm the timer.
    if RECOVERY_LAP_FOUND.with(std::cell::Cell::get) {
        Some(RECOVERY_RELAXED_DELAY)
    } else {
        None
    }
}
