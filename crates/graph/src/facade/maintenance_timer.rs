//! Timer-driven drain of the deferred LARA maintenance queue (ADR 0020).
//!
//! An adaptive, self-rescheduling one-shot timer (`ic-cdk-timers`) with
//! event-driven re-arm: mutation paths call [`arm_if_needed`] after enqueuing
//! maintenance work, and the canister arms from `init` / `post_upgrade`. No
//! timer is scheduled while the queue is empty; each tick chooses its successor
//! delay from the just-finished pass via [`next_delay`].
//!
//! The queued work is physical reclamation, but `CompactVertexEdgeSpan`
//! re-keys live edges (their `slot_index` changes), so a tick that touches an
//! indexed edge must also re-key the corresponding index postings. The tick is
//! therefore asynchronous (ADR 0023 P2): it fetches the router-sourced indexed
//! catalog, installs it ephemerally for the pass, runs the compaction, and
//! flushes the postings the compaction observers enqueued — all in one tick. If
//! the router catalog is unavailable the pass is deferred (retried at the floor
//! delay) rather than run blind, which would re-key the store without re-keying
//! the index and silently diverge the two.

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
    /// `true` while an async tick is in flight. The tick spans awaits (router
    /// catalog fetch, posting flush) during which the timer slot is cleared, so
    /// this flag keeps a concurrent enqueue's [`arm_if_needed`] from scheduling a
    /// duplicate, overlapping pass.
    static MAINTENANCE_RUNNING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
        let store = GraphStore::new();
        if store.maintenance_queue_len() == 0
            && store.repair_journal_is_empty()
            && store.derived_index_outbox_is_empty()
        {
            return;
        }
        if MAINTENANCE_RUNNING.with(std::cell::Cell::get) {
            // A pass is already draining; it reschedules its own successor.
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
/// future; the tick is asynchronous (router catalog fetch + posting flush).
#[cfg(target_family = "wasm")]
fn schedule(delay: core::time::Duration) -> ic_cdk_timers::TimerId {
    ic_cdk_timers::set_timer(delay, on_tick())
}

/// Runs one budgeted maintenance pass, then reschedules per [`next_delay`].
#[cfg(target_family = "wasm")]
async fn on_tick() {
    // Consume the fired one-shot and mark the async pass in flight so a
    // concurrent enqueue does not arm a duplicate while we await the router /
    // index. The reschedule below installs exactly one successor.
    MAINTENANCE_TIMER.with_borrow_mut(|slot| *slot = None);
    MAINTENANCE_RUNNING.with(|r| r.set(true));

    let next = run_maintenance_pass().await;

    MAINTENANCE_RUNNING.with(|r| r.set(false));
    if let Some(delay) = next {
        let id = schedule(delay);
        MAINTENANCE_TIMER.with_borrow_mut(|slot| *slot = Some(id));
    }
}

/// Fetches the catalog, runs one compaction pass under it, and flushes the
/// postings it re-keyed. Returns the reschedule delay (`None` stops the timer).
#[cfg(target_family = "wasm")]
async fn run_maintenance_pass() -> Option<core::time::Duration> {
    let store = GraphStore::new();

    // Hold the router-sourced catalog for the whole pass: the compaction
    // observers consult it while enqueuing posting re-keys, and the flush below
    // must run under the same view. Deferring on an unavailable router avoids a
    // blind pass that re-keys the store but not the index (ADR 0023 P2).
    let _catalog_guard = match acquire_catalog_guard(&store).await {
        Ok(guard) => guard,
        Err(()) => return Some(MAINTENANCE_TIMER_FLOOR_DELAY),
    };

    let report = store.run_timer_maintenance_tick();
    flush_and_repair(&store).await;

    let base = match report {
        Ok(report) => next_delay(
            report.remaining_queue_len(),
            report.instruction_budget_exhausted,
        ),
        // A Rust-level error leaves the stable queue intact; retry at the floor.
        Err(_) => Some(MAINTENANCE_TIMER_FLOOR_DELAY),
    };
    // Keep ticking while the durable repair journal still holds failed-flush
    // postings (ADR 0023 D5); a persistently unavailable index backs off to the
    // relaxed delay rather than hot-looping.
    match base {
        Some(delay) => Some(delay),
        None if !store.repair_journal_is_empty() || !store.derived_index_outbox_is_empty() => {
            Some(MAINTENANCE_TIMER_RELAXED_DELAY)
        }
        None => None,
    }
}

/// Installs the router-sourced indexed catalog for the pass. `Ok(None)` means the
/// shard is not federated (no index to keep consistent); `Err(())` means the
/// router was unreachable and the pass must be deferred.
#[cfg(target_family = "wasm")]
async fn acquire_catalog_guard(
    store: &GraphStore,
) -> Result<Option<crate::index::catalog_context::CatalogGuard>, ()> {
    let Some(routing) = store.federation_routing() else {
        return Ok(None);
    };
    let graph_name = store.logical_graph_name().unwrap_or_default();
    match crate::index::federation_routing::fetch_indexed_catalog(
        routing.router_canister,
        &graph_name,
    )
    .await
    {
        Ok(catalog) => Ok(Some(crate::index::catalog_context::enter(catalog))),
        Err(_) => Err(()),
    }
}

/// Flushes the postings the compaction observers enqueued during this pass and
/// re-applies any durable repair-journal entries, in the same tick, so the index
/// converges with the re-keyed store. A no-op on a non-federated shard.
#[cfg(target_family = "wasm")]
async fn flush_and_repair(store: &GraphStore) {
    let Some(routing) = store.federation_routing() else {
        return;
    };
    let client = crate::index::ic::IcPropertyIndexClient {
        index_principal: routing.index_canister,
        shard_id: routing.shard_id,
    };
    let ix = &client as &dyn crate::index::lookup::PropertyIndexLookup;
    let vector_client = routing
        .vector_index_canister
        .map(|vector_principal| crate::index::vector_ic::IcVectorIndexClient { vector_principal });
    let vx = vector_client
        .as_ref()
        .map(|c| c as &dyn crate::index::vector_lookup::VectorIndexLookup);
    if !store.repair_journal_is_empty() {
        // Durable repair entries are older than the volatile queues. Append pending work to the
        // journal before draining so it cannot overtake an older entry; contiguous compatible
        // entries are then delivered by the existing batch drain in one inter-canister call.
        let mut pending = crate::index::pending::take_pending_as_repair();
        pending.extend(crate::index::vector_pending::take_pending_as_repair());
        if !pending.is_empty() {
            store.repair_journal_append(0, pending);
        }
        let _ = crate::index::repair_journal::drain_once(ix, vx).await;
        return;
    }

    if !store.derived_index_outbox_is_empty() {
        let _ = crate::index::repair_journal::drain_outbox_once(ix, vx).await;
        if !store.derived_index_outbox_is_empty() {
            return;
        }
    }

    if store.repair_journal_is_empty() {
        let _ = crate::index::pending::flush_all_pending(Some(ix), None).await;
        let _ = crate::index::vector_pending::flush_pending(vx, None).await;
    }
    let _ = crate::index::repair_journal::drain_once(ix, vx).await;
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
