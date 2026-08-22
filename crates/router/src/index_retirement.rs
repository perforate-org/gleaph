//! Resumable drain driver for retired physical posting namespaces (ADR 0023 D6).
//!
//! A `DROP INDEX` removes the Router catalog row before graph-index postings are purged.
//! The durable purge obligation lives in
//! [`crate::facade::stable::index_retirement`]; this module converges it:
//!
//! - **Inline fast path** ([`drain_retirement_inline`]): the `DROP INDEX` ingress drives
//!   bounded steps per pending target until done or a bounded round cap, so small purges
//!   finish inside the DDL call. Any hold leaves durable state behind instead of failing
//!   the already-committed drop.
//! - **Recovery lane** ([`run_index_retirement_pass`]): one bounded step per pending
//!   target per scanned record per tick, with each target's resume cursor persisted in
//!   the record — a stopped target, response loss, or an upgrade resumes exactly where
//!   the previous attempt stopped, with no caller in the loop.
//!
//! Completion contract: a record is deleted only when its last frozen target confirms
//! `done`. PhysicalIndexId allocation is monotonic and never reused, so deletion cannot
//! be resurrected by a later CREATE INDEX.

use candid::Principal;
use gleaph_graph_kernel::federation::IndexPurgeKind;
use gleaph_graph_kernel::federation::{IndexPostingPurgeCursor, IndexPostingPurgeStepResult};
use gleaph_graph_kernel::index::PhysicalIndexId;

#[cfg(test)]
use std::rc::Rc;

use crate::facade::stable::index_retirement::{
    RetiredIndexRecord, RetirementTargetDrain, lookup_retirement, persist_pending,
    scan_retirements_after,
};

/// Bounded rounds of "one purge step per pending target" driven by the inline fast
/// path. Each round issues at most one inter-canister call per target, and every
/// intermediate state is durable, so hitting the cap simply defers the remainder to the
/// recovery lane without losing progress.
const INLINE_DRAIN_ROUND_CAP: usize = 64;

#[cfg(test)]
type StepOverrideFn = Rc<
    dyn Fn(
            Principal,
            &Option<IndexPostingPurgeCursor>,
        ) -> Option<Result<IndexPostingPurgeStepResult, String>>
        + 'static,
>;

#[cfg(test)]
thread_local! {
    static STEP_OVERRIDE: std::cell::RefCell<Option<StepOverrideFn>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_step_override(f: Option<StepOverrideFn>) {
    STEP_OVERRIDE.with_borrow_mut(|slot| *slot = f);
}

async fn purge_step(
    canister: Principal,
    physical_index_id: PhysicalIndexId,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    #[cfg(test)]
    {
        let overridden =
            STEP_OVERRIDE.with_borrow(|slot| slot.as_ref().and_then(|f| f(canister, &resume)));
        if let Some(result) = overridden {
            return result;
        }
    }
    crate::index_sync::admin_purge_property_postings_step(
        canister,
        physical_index_id,
        kind,
        property_id,
        label_id,
        resume,
    )
    .await
}

/// Advances one bounded purge step for one pending target. On success with a next
/// cursor the drain position is persisted; on success with completion the target is
/// removed from pending; on transport failure the target holds its unchanged cursor so
/// a retry replays the same idempotent bounded delete.
async fn advance_one_target(
    physical_index_id: PhysicalIndexId,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    mut drain: RetirementTargetDrain,
) -> Option<RetirementTargetDrain> {
    match purge_step(
        drain.canister,
        physical_index_id,
        kind,
        property_id,
        label_id,
        drain.resume.clone(),
    )
    .await
    {
        Ok(step) => match step.next {
            Some(cursor) => {
                drain.resume = Some(cursor);
                Some(drain)
            }
            None => None,
        },
        Err(_) => Some(drain),
    }
}

/// Advances one bounded purge step per pending target of one retirement record and
/// persists the outcome. Returns `true` if any target remains pending afterwards.
async fn advance_retirement(
    physical_index_id: PhysicalIndexId,
    record: RetiredIndexRecord,
) -> bool {
    let RetiredIndexRecord {
        kind,
        property_id,
        label_id,
        pending,
        ..
    } = record;
    let mut still_pending = Vec::with_capacity(pending.len());
    for drain in pending {
        if let Some(held) =
            advance_one_target(physical_index_id, kind, property_id, label_id, drain).await
        {
            still_pending.push(held);
        }
    }
    persist_pending(physical_index_id, still_pending);
    lookup_retirement(physical_index_id).is_some()
}

/// Drives the record's pending targets with bounded rounds until every target confirms
/// `done`, the record retires, or the round cap defers the remainder to the recovery
/// lane. Never fails: convergence is owned by durable state plus the recovery lane.
pub(crate) async fn drain_retirement_inline(physical_index_id: PhysicalIndexId) {
    for _ in 0..INLINE_DRAIN_ROUND_CAP {
        let Some(record) = lookup_retirement(physical_index_id) else {
            return;
        };
        if !advance_retirement(physical_index_id, record).await {
            return;
        }
    }
}

/// Runs one bounded recovery-lane sweep starting after `cursor`. Returns the next scan
/// cursor (`None` when the retirement keyspace was exhausted — start a fresh lap) and
/// whether any record still needs a later lap.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (retirement-drain lane)"
    )
)]
pub(crate) async fn run_index_retirement_pass(
    cursor: Option<u64>,
    budget: usize,
) -> (Option<u64>, bool) {
    let (rows, last_examined) = scan_retirements_after(cursor, budget);
    let row_count = rows.len();
    let mut found = false;
    for (raw_key, record) in rows {
        let key = PhysicalIndexId::new(raw_key)
            .expect("a stored retirement key is a valid PhysicalIndexId");
        if advance_retirement(key, record).await {
            found = true;
        }
    }
    let lap_complete = row_count < budget;
    let next = if lap_complete { None } else { last_examined };
    (next, found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::index_retirement::{
        RetiredIndexRecord, RetirementTargetDrain, clear_for_test, enqueue_retirement,
        lookup_retirement,
    };
    use gleaph_graph_kernel::entry::GraphId;
    use std::cell::Cell;
    use std::rc::Rc;

    fn phys(raw: u64) -> PhysicalIndexId {
        PhysicalIndexId::new(raw).expect("nonzero physical id")
    }

    fn canister(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    fn record(canisters: &[Principal]) -> RetiredIndexRecord {
        RetiredIndexRecord {
            graph_id: GraphId::from_raw(1),
            kind: IndexPurgeKind::Vertex,
            property_id: 7,
            label_id: 0,
            pending: canisters
                .iter()
                .map(|c| RetirementTargetDrain {
                    canister: *c,
                    resume: None,
                })
                .collect(),
            enqueued_at_ns: 42,
        }
    }

    fn vertex_cursor(raw: u64, key_byte: u8) -> IndexPostingPurgeCursor {
        IndexPostingPurgeCursor::Vertex {
            physical_index_id: phys(raw),
            property_id: 7,
            resume_key: vec![key_byte],
        }
    }

    fn not_done(raw: u64) -> Result<IndexPostingPurgeStepResult, String> {
        Ok(IndexPostingPurgeStepResult {
            next: Some(vertex_cursor(raw, 1)),
            examined: 1,
            removed: 1,
            done: false,
        })
    }

    fn done() -> Result<IndexPostingPurgeStepResult, String> {
        Ok(IndexPostingPurgeStepResult {
            next: None,
            examined: 0,
            removed: 0,
            done: true,
        })
    }

    struct OverrideGuard;
    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            set_step_override(None);
        }
    }

    /// A wrong implementation that deletes the retirement record on transport failure
    /// (or fails the already-committed drop) would lose the purge identity forever.
    #[test]
    fn response_loss_holds_then_next_pass_converges() {
        pollster::block_on(async {
            clear_for_test();
            let _guard = OverrideGuard;
            enqueue_retirement(phys(11), record(&[canister(1)]));
            let attempts = Rc::new(Cell::new(0u32));
            {
                let attempts = attempts.clone();
                set_step_override(Some(Rc::new(move |_canister, _resume| {
                    let attempt = attempts.get();
                    attempts.set(attempt + 1);
                    if attempt == 0 {
                        Some(Err("index canister unreachable".into()))
                    } else {
                        Some(done())
                    }
                })));
            }

            let (next, found) = run_index_retirement_pass(None, 16).await;
            assert_eq!(
                next, None,
                "a single record within budget completes the lap"
            );
            assert!(found, "the held record must request a later lap");
            let held = lookup_retirement(phys(11)).expect("record survives transport failure");
            assert_eq!(held.pending.len(), 1);
            assert_eq!(
                held.pending[0].resume, None,
                "a failed step must hold the unchanged resume cursor"
            );

            let (next, found) = run_index_retirement_pass(None, 16).await;
            assert_eq!(next, None);
            assert!(!found, "the drained record leaves no work behind");
            assert!(
                lookup_retirement(phys(11)).is_none(),
                "confirmed completion must retire the record"
            );
        });
    }

    /// A wrong implementation that restarts every purge from `None` (or never persists
    /// the cursor) would re-scan drained ranges and cannot satisfy this contract.
    #[test]
    fn cursor_persists_across_passes() {
        pollster::block_on(async {
            clear_for_test();
            let _guard = OverrideGuard;
            enqueue_retirement(phys(12), record(&[canister(3)]));
            set_step_override(Some(Rc::new(|_canister, _resume| Some(not_done(12)))));

            let (_, found) = run_index_retirement_pass(None, 16).await;
            assert!(found);
            let record = lookup_retirement(phys(12)).expect("pending work persists");
            assert_eq!(
                record.pending[0].resume,
                Some(vertex_cursor(12, 1)),
                "the returned next cursor becomes the durable drain position"
            );

            let received = Rc::new(std::cell::RefCell::new(Vec::new()));
            {
                let received = received.clone();
                set_step_override(Some(Rc::new(move |_canister, resume| {
                    received.borrow_mut().push(resume.clone());
                    Some(done())
                })));
            }
            let (_, found) = run_index_retirement_pass(None, 16).await;
            assert!(!found);
            assert_eq!(
                *received.borrow(),
                vec![Some(vertex_cursor(12, 1))],
                "the next attempt resumes exactly from the persisted cursor"
            );
            assert!(lookup_retirement(phys(12)).is_none());
        });
    }

    #[test]
    fn multi_target_drain_completes_per_target() {
        pollster::block_on(async {
            clear_for_test();
            let _guard = OverrideGuard;
            enqueue_retirement(phys(13), record(&[canister(1), canister(2)]));
            set_step_override(Some(Rc::new(|target, _resume| {
                if target == canister(1) {
                    Some(done())
                } else {
                    Some(not_done(13))
                }
            })));

            let (_, found) = run_index_retirement_pass(None, 16).await;
            assert!(found);
            let record = lookup_retirement(phys(13)).expect("one target still pending");
            assert_eq!(
                record.pending,
                vec![RetirementTargetDrain {
                    canister: canister(2),
                    resume: Some(vertex_cursor(13, 1)),
                }],
                "only the completed target leaves the pending list"
            );

            set_step_override(Some(Rc::new(|_canister, _resume| Some(done()))));
            let (_, found) = run_index_retirement_pass(None, 16).await;
            assert!(!found);
            assert!(lookup_retirement(phys(13)).is_none());
        });
    }

    #[test]
    fn empty_pending_list_retires_the_record() {
        clear_for_test();
        enqueue_retirement(phys(14), record(&[canister(5)]));
        persist_pending(phys(14), Vec::new());
        assert!(lookup_retirement(phys(14)).is_none());
    }

    #[test]
    fn scan_pagination_resumes_after_last_examined_key() {
        clear_for_test();
        enqueue_retirement(phys(21), record(&[]));
        // phys(20) sorts before phys(21); both records coexist for pagination.
        enqueue_retirement(phys(20), record(&[]));

        let (rows_a, last_a) =
            crate::facade::stable::index_retirement::scan_retirements_after(None, 1);
        assert_eq!(rows_a.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![20]);
        assert_eq!(last_a, Some(20));

        let (rows_b, last_b) =
            crate::facade::stable::index_retirement::scan_retirements_after(last_a, 1);
        assert_eq!(rows_b.iter().map(|(k, _)| *k).collect::<Vec<_>>(), vec![21]);
        assert_eq!(last_b, Some(21));

        let (rows_c, last_c) =
            crate::facade::stable::index_retirement::scan_retirements_after(last_b, 1);
        assert!(rows_c.is_empty());
        assert_eq!(last_c, None);
    }
}
