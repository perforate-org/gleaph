//! Cross-shard uniqueness unified effect recovery — ADR 0030 slice 6, Driver 2.
//!
//! Driver 1 ([`crate::reclaim`]) is reservation-driven: it converges reservations the live
//! Try/Confirm path left unresolved. But some pinned effects own **no** reservation Driver 1 can
//! find — a `Release` (the releasing mutation differs from the original `Acquire`) and an *orphan*
//! `Acquire` whose reservation is gone. Driver 2 is the discovery-driven complement: it drains the
//! [`crate::facade::stable::unique_effect_pending`] index (registered before every unique dispatch),
//! enumerates each shard's still-pinned effects for the mutation, and reconciles them.
//!
//! Per discovery row `(graph, mutation, shard) → PendingEffectRecord`:
//!
//! - **Quarantine backoff**: a `Quarantined` row whose `next_retry_ns` is in the future is skipped
//!   without counting as lap work, so an unresolvable orphan never hot-loops the timer.
//! - **Termination gate**: the owning `RouterMutationRecord` (resolved via the row's `client_key`,
//!   GC-pinned while the row exists) must be **terminal** — its effect generation finished. A
//!   missing or still-non-terminal record is *held* (the only safe orphan classification is one made
//!   after the mutation can emit no more effects).
//! - **Drain** (terminal): page through every un-acked effect (`Acquire` and `Release`), paged by
//!   `effect_ordinal` cursor — a **short page is not EOF; only an empty page is**:
//!   - `Release` ⇒ durably reconcile the reservation **first** ([`RouterStore::release_unique_effect`]);
//!     ack only on a durable free, else hold (Release-before-Acquire) for a later lap.
//!   - `Acquire` **with** a reservation ⇒ never ack here; delegate to Driver 1 (leave it pinned).
//!   - `Acquire` **without** a reservation ⇒ an orphan: never ack, quarantine the row with a
//!     persistent diagnostic and a long backoff, keeping the row and its evidence.
//! - **Removal**: only after the termination gate passed **and** a fresh `cursor=None` re-scan comes
//!   back empty (every effect acked) is the row removed — which un-pins the owning record for GC.

use crate::facade::stable::unique_effect_pending::{
    PendingEffectRow, PendingEffectState, UniqueEffectPendingKey,
};
use crate::facade::store::RouterStore;
use crate::graph_client::{ack_unique_effects, read_unique_mutation_effects};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{EffectId, UniqueEffectOp, UniqueEffectReceipt};

/// Effects pulled per drain page. The shard clamps to its own hard maximum, so this is an upper
/// bound; a short page does not imply the last page.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer (Driver 2)")
)]
const EFFECT_RECON_PAGE: u32 = 256;

/// Re-check backoff for a quarantined orphan `Acquire`. Far longer than a normal recovery lap: an
/// orphan has no reservation to converge it, so it needs operator/diagnostic attention, not a busy
/// retry. A bounded re-check (rather than never) lets recovery notice if the value's reservation is
/// later legitimately re-created.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer (Driver 2)")
)]
const ORPHAN_RECHECK_BACKOFF_NS: u64 = 60 * 60 * 1_000_000_000;

/// What draining one page of a shard's effects concluded (the decision core, no I/O).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PageOutcome {
    /// Effects the Router has durably applied and may now ack (unpin).
    pub acks: Vec<EffectId>,
    /// An orphan `Acquire` (no reservation) was seen — the row must be quarantined, never acked.
    pub orphan: bool,
    /// At least one effect stays pinned (a held `Release` or a delegated `Acquire`), so the row
    /// cannot be removed yet.
    pub held: bool,
}

/// Decision core of the drain (pure of I/O, store-backed, unit-tested): classify one page of a
/// mutation's pinned effects, applying the **durable reservation update first** for each `Release`
/// (so the ack that follows is always preceded by a durable state change). Returns the effects to
/// ack plus whether an orphan was seen / anything stays pinned.
pub(crate) fn process_effect_page(
    store: &RouterStore,
    graph_id: GraphId,
    page: &[UniqueEffectReceipt],
) -> PageOutcome {
    let mut outcome = PageOutcome::default();
    for effect in page {
        match effect.op {
            UniqueEffectOp::Release => {
                // Durably reconcile the reservation first; ack only on a proven free.
                if store.release_unique_effect(
                    graph_id,
                    effect.constraint_id,
                    &effect.encoded_value,
                    &effect.owner_element_id,
                ) {
                    outcome.acks.push(effect.effect_id);
                } else {
                    outcome.held = true;
                }
            }
            UniqueEffectOp::Acquire => {
                // A pinned `Acquire` is never acked by Driver 2: with a reservation it is Driver 1's
                // to confirm/ack; without one it is an orphan kept as evidence.
                outcome.held = true;
                match effect.claim_id {
                    Some(claim)
                        if store.reservation_exists_for_claim(
                            graph_id,
                            effect.constraint_id,
                            &effect.encoded_value,
                            claim,
                        ) =>
                    {
                        // Delegated to Driver 1; leave pinned.
                    }
                    _ => outcome.orphan = true,
                }
            }
        }
    }
    outcome
}

/// Whether reconciling a row left work that should be retried on a later lap (drives the timer's
/// re-lap decision). `Settled` (drained+removed, or an orphan just parked) needs nothing; `Pending`
/// (held/non-terminal/unreachable) needs a prompt re-lap; `Skipped { wake_at_ns }` is inside a
/// quarantine backoff — it needs no *prompt* re-lap, but the timer must still wake at `wake_at_ns`
/// to re-check it (otherwise an all-quarantined keyspace would stop the timer forever).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowOutcome {
    Skipped { wake_at_ns: u64 },
    Settled,
    Pending,
}

/// Reconcile one discovery row end to end: gate on quarantine backoff and mutation termination, drain
/// the shard's effects (paging until an empty page), ack the freed ones, then remove the row iff a
/// fresh `cursor=None` re-scan is empty — else quarantine an orphan or leave it pending.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer (Driver 2); the core is unit-tested"
    )
)]
async fn reconcile_row(store: &RouterStore, row: PendingEffectRow, now: u64) -> RowOutcome {
    let key = row.key;
    let record = row.record;

    // Quarantine backoff: do not even count an un-due orphan as lap work — this is the timer
    // hot-loop guard for an orphan that has no reservation to converge it. The deadline is surfaced
    // so the timer can still wake at `next_retry_ns` to re-check it.
    if record.state == PendingEffectState::Quarantined && now < record.next_retry_ns {
        return RowOutcome::Skipped {
            wake_at_ns: record.next_retry_ns,
        };
    }

    // Termination gate: only a mutation whose effect generation has finished can be drained, and
    // only then can an absent reservation be classified as an orphan. The owning record must be the
    // **same mutation** (`record.as_v1().mutation_id == key.mutation_id`) and terminal; a missing record
    // (should not happen while the row GC-pins it), a same-client-key retry that recycled the record
    // onto a different mutation, or a still-non-terminal one is held.
    match store.mutation_terminal_for(&record.client_key, key.mutation_id) {
        Some(true) => {}
        Some(false) | None => return RowOutcome::Pending,
    }

    let canister = record.canister;
    let mut cursor: Option<u32> = None;
    let mut orphan_seen = false;
    loop {
        let page = match read_unique_mutation_effects(
            canister,
            key.mutation_id,
            cursor,
            EFFECT_RECON_PAGE,
        )
        .await
        {
            Ok(page) => page,
            // Unreachable shard: hold the row for the next lap.
            Err(_) => return RowOutcome::Pending,
        };
        // Only an empty page is end-of-stream: the shard clamps `limit` to its own hard cap, so a
        // short page does not imply the last page.
        let Some(last_ordinal) = page.last().map(|effect| effect.effect_id.effect_ordinal) else {
            break;
        };
        let outcome = process_effect_page(store, key.graph_id, &page);
        if !outcome.acks.is_empty() {
            let _ = ack_unique_effects(canister, outcome.acks).await;
        }
        orphan_seen |= outcome.orphan;
        cursor = Some(last_ordinal);
    }

    // Removal requires a fresh `cursor=None` re-scan that is empty — proving every effect was acked,
    // not merely that the cursor walked off the end past still-pinned effects.
    match read_unique_mutation_effects(canister, key.mutation_id, None, 1).await {
        Ok(remaining) if remaining.is_empty() => {
            store.remove_pending_unique_effect(key.graph_id, key.mutation_id, key.shard_id);
            RowOutcome::Settled
        }
        Ok(_) => {
            if orphan_seen {
                store.quarantine_pending_unique_effect(
                    key.graph_id,
                    key.mutation_id,
                    key.shard_id,
                    now.saturating_add(ORPHAN_RECHECK_BACKOFF_NS),
                    orphan_diagnostic(key.mutation_id, key.shard_id),
                );
                // Parked with a backoff: not urgent lap work (the quarantine gate handles re-check).
                RowOutcome::Settled
            } else {
                // A held `Release` or a delegated `Acquire` waiting on Driver 1: retry next lap.
                RowOutcome::Pending
            }
        }
        // Re-scan unreachable: hold.
        Err(_) => RowOutcome::Pending,
    }
}

#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer (Driver 2)")
)]
fn orphan_diagnostic(
    mutation_id: u64,
    shard_id: gleaph_graph_kernel::federation::ShardId,
) -> String {
    format!(
        "orphan unique Acquire: mutation {mutation_id} pinned an Acquire on shard {shard_id} with \
         no Router reservation; it cannot be safely confirmed or freed and is quarantined for \
         diagnostics (ADR 0030 slice 6, Driver 2)"
    )
}

/// Outcome of one bounded effect-recovery sweep.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer (Driver 2)")
)]
pub(crate) struct EffectPassOutcome {
    /// Next scan cursor (`None` when the keyspace was exhausted — start a fresh lap).
    pub next_cursor: Option<UniqueEffectPendingKey>,
    /// At least one row needs a prompt re-lap (held/non-terminal/unreachable).
    pub found: bool,
    /// Earliest `next_retry_ns` among rows skipped inside their quarantine backoff this sweep, so the
    /// timer can wake to re-check them even when no row needs a prompt re-lap.
    pub earliest_wake_ns: Option<u64>,
}

/// Run one bounded effect-recovery sweep starting after `cursor`.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer (Driver 2)")
)]
pub(crate) async fn run_effect_recovery_pass(
    cursor: Option<UniqueEffectPendingKey>,
    budget: usize,
    now: u64,
) -> EffectPassOutcome {
    let store = RouterStore::new();
    let (rows, last_examined, scanned) =
        crate::facade::stable::unique_effect_pending::scan(cursor.as_ref(), budget);
    let mut found = false;
    let mut earliest_wake_ns: Option<u64> = None;
    for row in rows {
        match reconcile_row(&store, row, now).await {
            RowOutcome::Pending => found = true,
            RowOutcome::Skipped { wake_at_ns } => {
                earliest_wake_ns = Some(match earliest_wake_ns {
                    Some(current) => current.min(wake_at_ns),
                    None => wake_at_ns,
                });
            }
            RowOutcome::Settled => {}
        }
    }
    let lap_complete = scanned < budget as u32;
    let next_cursor = if lap_complete { None } else { last_examined };
    EffectPassOutcome {
        next_cursor,
        found,
        earliest_wake_ns,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::ROUTER_MUTATION_BY_CLIENT_KEY;
    use crate::facade::stable::label_stats::{ClientMutationKey, RouterMutationRecord};
    use crate::facade::stable::reservation_catalog::{self, ProofShard, ReservationClaim};
    use crate::facade::stable::unique_effect_pending;
    use candid::Principal;
    use gleaph_graph_kernel::entry::ConstraintNameId;
    use gleaph_graph_kernel::federation::{ClaimId, EffectId, ShardId};

    fn constraint() -> ConstraintNameId {
        ConstraintNameId::from_raw(5)
    }

    fn graph(seed: u32) -> GraphId {
        GraphId::from_raw(930_000 + seed)
    }

    fn acquire(mid: u64, ordinal: u32, value: &[u8], owner: u8) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(mid, ordinal),
            claim_id: Some(ClaimId::new(mid, 0)),
            owner_element_id: vec![owner; 8],
            constraint_id: constraint(),
            encoded_value: value.to_vec(),
            op: UniqueEffectOp::Acquire,
        }
    }

    fn release(mid: u64, ordinal: u32, value: &[u8], owner: u8) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(mid, ordinal),
            claim_id: None,
            owner_element_id: vec![owner; 8],
            constraint_id: constraint(),
            encoded_value: value.to_vec(),
            op: UniqueEffectOp::Release,
        }
    }

    fn seed_reservation(g: GraphId, mid: u64, value: &[u8], state_committed: bool) {
        reservation_catalog::try_reserve(
            g,
            mid,
            &[ReservationClaim {
                constraint_id: constraint(),
                encoded_value: value.to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            0,
        )
        .expect("seed reservation");
        if state_committed {
            reservation_catalog::confirm_reservation(
                g,
                ClaimId::new(mid, 0),
                constraint(),
                value,
                vec![1u8; 8],
                EffectId::new(mid, 0),
            );
        }
    }

    #[test]
    fn page_release_freed_is_acked() {
        let store = RouterStore::new();
        let g = graph(1);
        let mid = 9_500_001;
        // A committed reservation owned by element 1; the Release for that owner frees it.
        seed_reservation(g, mid, b"v", true);
        reservation_catalog::clear_acquire_ack(g, constraint(), b"v", ClaimId::new(mid, 0));

        let outcome = process_effect_page(&store, g, &[release(mid, 1, b"v", 1)]);
        assert_eq!(outcome.acks, vec![EffectId::new(mid, 1)]);
        assert!(!outcome.held);
        assert!(!outcome.orphan);
    }

    #[test]
    fn page_release_held_is_not_acked() {
        let store = RouterStore::new();
        let g = graph(2);
        let mid = 9_500_002;
        // A still-`Reserved` reservation: the Release is held (Release-before-Acquire).
        seed_reservation(g, mid, b"v", false);

        let outcome = process_effect_page(&store, g, &[release(mid, 1, b"v", 1)]);
        assert!(outcome.acks.is_empty());
        assert!(outcome.held);
        assert!(!outcome.orphan);
    }

    #[test]
    fn page_acquire_with_reservation_is_delegated_not_acked() {
        let store = RouterStore::new();
        let g = graph(3);
        let mid = 9_500_003;
        seed_reservation(g, mid, b"v", false);

        let outcome = process_effect_page(&store, g, &[acquire(mid, 0, b"v", 1)]);
        assert!(
            outcome.acks.is_empty(),
            "delegated to Driver 1, never acked"
        );
        assert!(outcome.held);
        assert!(!outcome.orphan, "a reservation exists — not an orphan");
    }

    #[test]
    fn page_acquire_without_reservation_is_orphan() {
        let store = RouterStore::new();
        let g = graph(4);
        let mid = 9_500_004;
        // No reservation seeded.
        let outcome = process_effect_page(&store, g, &[acquire(mid, 0, b"v", 1)]);
        assert!(outcome.acks.is_empty(), "an orphan is never acked");
        assert!(outcome.held);
        assert!(outcome.orphan);
    }

    #[test]
    fn page_acquire_with_other_claims_reservation_is_orphan() {
        let store = RouterStore::new();
        let g = graph(5);
        let mid = 9_500_005;
        // A reservation for the same value but held by a *different* mutation/claim does not cover
        // this Acquire's claim — still an orphan.
        seed_reservation(g, 9_500_999, b"v", false);

        let outcome = process_effect_page(&store, g, &[acquire(mid, 0, b"v", 1)]);
        assert!(outcome.orphan);
    }

    fn insert_record(key: &ClientMutationKey, mid: u64, terminal: bool) {
        let mut record = RouterMutationRecord::new(mid, 0, b"fp".to_vec());
        record.as_v1_mut().routing_in_progress = false;
        if terminal {
            // A completed canonical shard makes the record terminal (completed).
            record.as_v1_mut().completed_row_count = Some(0);
        }
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(key.clone(), record);
        });
    }

    fn pending_row(g: GraphId, mid: u64, key: &ClientMutationKey) -> PendingEffectRow {
        unique_effect_pending::register(
            g,
            mid,
            ShardId::new(0),
            Principal::anonymous(),
            key.clone(),
        );
        let (rows, _n, _s) = unique_effect_pending::scan(None, 4096);
        rows.into_iter()
            .find(|r| r.key.graph_id == g && r.key.mutation_id == mid)
            .expect("row")
    }

    #[test]
    fn reconcile_holds_when_mutation_non_terminal() {
        let store = RouterStore::new();
        let g = graph(6);
        let mid = 9_500_006;
        let key = ClientMutationKey::new(Principal::anonymous(), g, "k6".to_string());
        insert_record(&key, mid, false);
        let row = pending_row(g, mid, &key);

        let outcome = futures::executor::block_on(reconcile_row(&store, row, 0));
        assert_eq!(
            outcome,
            RowOutcome::Pending,
            "non-terminal mutation is held"
        );
        // The row survives for a later lap.
        assert!(unique_effect_pending::lookup(g, mid, ShardId::new(0)).is_some());
    }

    #[test]
    fn reconcile_holds_when_record_missing() {
        let store = RouterStore::new();
        let g = graph(7);
        let mid = 9_500_007;
        let key = ClientMutationKey::new(Principal::anonymous(), g, "k7".to_string());
        // No record inserted.
        let row = pending_row(g, mid, &key);

        let outcome = futures::executor::block_on(reconcile_row(&store, row, 0));
        assert_eq!(outcome, RowOutcome::Pending, "a missing record is held");
        assert!(unique_effect_pending::lookup(g, mid, ShardId::new(0)).is_some());
    }

    #[test]
    fn reconcile_skips_quarantined_within_backoff() {
        let store = RouterStore::new();
        let g = graph(8);
        let mid = 9_500_008;
        let key = ClientMutationKey::new(Principal::anonymous(), g, "k8".to_string());
        insert_record(&key, mid, true);
        unique_effect_pending::register(
            g,
            mid,
            ShardId::new(0),
            Principal::anonymous(),
            key.clone(),
        );
        unique_effect_pending::quarantine(g, mid, ShardId::new(0), 10_000, "orphan".to_string());
        let (rows, _n, _s) = unique_effect_pending::scan(None, 4096);
        let row = rows
            .into_iter()
            .find(|r| r.key.graph_id == g && r.key.mutation_id == mid)
            .expect("row");

        // now (5_000) < next_retry_ns (10_000): skipped without lap work, no shard call attempted,
        // and the deadline is surfaced so the timer can wake to re-check.
        let outcome = futures::executor::block_on(reconcile_row(&store, row, 5_000));
        assert_eq!(outcome, RowOutcome::Skipped { wake_at_ns: 10_000 });
    }

    #[test]
    fn reconcile_holds_when_record_mutation_id_differs() {
        let store = RouterStore::new();
        let g = graph(9);
        let mid = 9_500_009;
        let key = ClientMutationKey::new(Principal::anonymous(), g, "k9".to_string());
        // The record under this client key is a *different*, terminal mutation (a same-key retry
        // recycled it). It must not prove the pending mutation `mid` terminal.
        insert_record(&key, mid + 1, true);
        let row = pending_row(g, mid, &key);

        let outcome = futures::executor::block_on(reconcile_row(&store, row, 0));
        assert_eq!(
            outcome,
            RowOutcome::Pending,
            "a record recycled onto another mutation cannot prove this one terminal"
        );
        assert!(unique_effect_pending::lookup(g, mid, ShardId::new(0)).is_some());
    }
}
