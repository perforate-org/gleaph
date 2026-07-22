//! Cross-shard uniqueness reclaim reconciler — ADR 0030 slice 6, Driver 1.
//!
//! Converges reservations the live Try/Confirm path left unresolved after a crash or a lost reply,
//! using only the **safe subset**: the authority to free a value is never a timeout, it is a
//! generation-fenced proof read of the reservation's `proof_scope`. Each tick the recovery timer
//! drives a bounded slice of the reservation table through [`run_reclaim_pass`].
//!
//! Per candidate (work discovery is [`crate::facade::store::RouterStore::scan_unique_reclaim_candidates`]):
//!
//! - **`Reserved` past TTL / `Reclaiming`**: `begin`/`resume` the reclaim fence (no `await`), read
//!   the `Acquire` proof across `proof_scope` under that fence, then under the fence apply exactly
//!   one outcome:
//!   - *Acquire present* on any scope shard ⇒ the canonical write committed: `Reclaiming@g →
//!     Committed`, decrement the non-terminal count, then ack (unpin) the `Acquire` and clear.
//!   - *every scope shard reachable and Acquire absent* ⇒ a cancel is permitted only with the
//!     **second** condition too — the owning mutation is irreversibly terminally-failed, so no
//!     in-flight canonical dispatch can still commit. Terminal-fail + `cancel` + count-decrement run
//!     in one no-`await` region; if the mutation cannot be terminal-failed (a canonical shard
//!     completed, it re-routed, or its reverse-index row is gone) we `hold` instead.
//!   - *any scope shard unreachable* ⇒ `hold` (revert `Reclaiming@g → Reserved`, keep `g`).
//! - **`Committed` with a pending `Acquire` ack**: the commit is durable but the ack was never
//!   confirmed. *Acquire present* ⇒ re-ack and clear; *every scope shard absent* ⇒ the ack landed
//!   but its reply was lost, so clear the pending marker; *any unreachable* ⇒ hold.

use candid::Principal;
use gleaph_graph_kernel::federation::{ClaimId, EffectId, UniqueAcquireEvidence};

use crate::facade::stable::reservation_catalog::{
    ProofShard, ReclaimCandidate, ReclaimTicket, ReservationState, UniqueReservationKey,
};
use crate::facade::store::RouterStore;
use crate::graph_client::{ack_unique_effects, read_unique_effect_proof};

/// What a `proof_scope` read concluded for one claim (ADR 0030 slice 6). The cancel authority is the
/// conjunction of [`ProofVerdict::AllAbsent`] **and** an irreversibly terminally-failed mutation;
/// neither alone is sufficient.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProofVerdict {
    /// At least one reachable scope shard holds the claim's `Acquire`: the canonical write committed.
    Present {
        evidence: UniqueAcquireEvidence,
        target: Principal,
    },
    /// Every scope shard was reachable and reported the `Acquire` absent.
    AllAbsent,
    /// At least one scope shard was unreachable, so absence cannot be concluded — hold.
    Inconclusive,
}

/// An ack the caller must send (await) and, on success, clear: the `Acquire` `effect_id` on `target`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AckRequest {
    pub target: Principal,
    pub effect_id: EffectId,
}

/// Classify the per-shard proof responses for one claim into a [`ProofVerdict`] (pure; the I/O is in
/// [`read_proof_verdict`]). Enforces the negative invariant strictly: [`ProofVerdict::AllAbsent`] is
/// returned **only** when every scope shard returned an *explicit* `{ claim_id, acquire: None }` row
/// for this claim. Anything weaker is [`ProofVerdict::Inconclusive`] — an empty scope, an
/// unreachable shard (`None` response), or a success response missing the claim's row (a partial or
/// malformed proof) — so an incomplete negative can never authorize a Cancel or an ack-reply-lost
/// clear. `Some(proofs)` is a reachable shard's response; `None` is unreachable.
fn classify_proof_responses(
    claim: ClaimId,
    responses: &[(
        Principal,
        Option<Vec<gleaph_graph_kernel::federation::UniqueAcquireProof>>,
    )],
) -> ProofVerdict {
    if responses.is_empty() {
        // An empty proof scope proves nothing — never an absence.
        return ProofVerdict::Inconclusive;
    }
    let mut all_explicitly_absent = true;
    for (target, response) in responses {
        match response {
            Some(proofs) => match proofs.iter().find(|proof| proof.claim_id == claim) {
                // A row for the claim: present `Acquire` wins; explicit `acquire: None` is a genuine
                // per-shard absence (the loop continues to require every shard to be absent).
                Some(proof) => {
                    if let Some(evidence) = &proof.acquire {
                        return ProofVerdict::Present {
                            evidence: evidence.clone(),
                            target: *target,
                        };
                    }
                }
                // Reachable, but no row for the claim: an incomplete negative, not an absence.
                None => all_explicitly_absent = false,
            },
            // Unreachable shard: cannot conclude absence.
            None => all_explicitly_absent = false,
        }
    }
    if all_explicitly_absent {
        ProofVerdict::AllAbsent
    } else {
        ProofVerdict::Inconclusive
    }
}

/// Read the claim's `Acquire` proof from every `proof_scope` shard, then classify (see
/// [`classify_proof_responses`]). An errored read becomes a `None` (unreachable) response.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer; resolvers are unit-tested"
    )
)]
async fn read_proof_verdict(scope: &[ProofShard], claim: ClaimId) -> ProofVerdict {
    let mut responses = Vec::with_capacity(scope.len());
    for shard in scope {
        let response = read_unique_effect_proof(shard.graph_canister, vec![claim])
            .await
            .ok();
        responses.push((shard.graph_canister, response));
    }
    classify_proof_responses(claim, &responses)
}

/// Apply a reclaim proof's verdict to a `Reserved`/`Reclaiming` reservation under its fence (no
/// `await`). Returns an [`AckRequest`] when the reservation just committed and its still-pinned
/// `Acquire` must be acked; `None` when the reservation was cancelled or held.
pub(crate) fn resolve_reclaim(
    store: &RouterStore,
    key: &UniqueReservationKey,
    ticket: &ReclaimTicket,
    verdict: &ProofVerdict,
) -> Option<AckRequest> {
    let mutation_id = ticket.claim.mutation_id;
    match verdict {
        ProofVerdict::Present { evidence, target } => {
            // The canonical write committed. Commit + decrement the non-terminal count in one
            // no-`await` region; the ack is sent afterward and re-driven (Committed+pending) on
            // failure, so a crash between commit and ack never strands the effect.
            if store.apply_unique_reclaim_commit(
                key.graph_id,
                key.constraint_id,
                &key.encoded_value,
                ticket.claim,
                ticket.generation,
                evidence.owner_element_id.clone(),
                evidence.effect_id,
            ) {
                store.release_unique_reservation_slot(mutation_id);
                Some(AckRequest {
                    target: *target,
                    effect_id: evidence.effect_id,
                })
            } else {
                // Fence diverged (the reservation moved on); nothing to ack — the next lap re-reads.
                None
            }
        }
        ProofVerdict::AllAbsent => {
            // Absence is only half the cancel authority; the other half is an irreversible terminal
            // failure. The atomic primitive preflights every condition (owner + count, reservation
            // fence, record eligibility) before any mutation, then applies terminal-fail + cancel +
            // count-decrement as one all-or-nothing region. If it declines (missing reverse row, the
            // dispatch is no longer uncommitted, or the fence diverged), nothing changed — hold.
            let error = format!(
                "cross-shard uniqueness reservation reclaimed: mutation {mutation_id} produced no \
                 Acquire on any proof-scope shard and its canonical dispatch never committed; the \
                 mutation is terminally failed (ADR 0030 slice 6)"
            );
            if !store.reclaim_cancel_uncommitted(
                key.graph_id,
                key.constraint_id,
                &key.encoded_value,
                ticket.claim,
                ticket.generation,
                error,
            ) {
                store.hold_unique_reclaim(
                    key.graph_id,
                    key.constraint_id,
                    &key.encoded_value,
                    ticket.claim,
                    ticket.generation,
                );
            }
            None
        }
        ProofVerdict::Inconclusive => {
            store.hold_unique_reclaim(
                key.graph_id,
                key.constraint_id,
                &key.encoded_value,
                ticket.claim,
                ticket.generation,
            );
            None
        }
    }
}

/// Resolve a `Committed` reservation whose `Acquire` ack is still pending (no `await`). *Present* ⇒
/// re-ack ([`AckRequest`]) then clear; *AllAbsent* ⇒ the ack landed but the reply was lost, so clear
/// the pending marker now; *Inconclusive* ⇒ hold.
pub(crate) fn resolve_committed_pending(
    store: &RouterStore,
    key: &UniqueReservationKey,
    claim: ClaimId,
    verdict: &ProofVerdict,
) -> Option<AckRequest> {
    match verdict {
        ProofVerdict::Present { evidence, target } => Some(AckRequest {
            target: *target,
            effect_id: evidence.effect_id,
        }),
        ProofVerdict::AllAbsent => {
            store.clear_unique_acquire_ack(
                key.graph_id,
                key.constraint_id,
                &key.encoded_value,
                claim,
            );
            None
        }
        ProofVerdict::Inconclusive => None,
    }
}

/// Reconcile one candidate end to end: discover the fence, read the proof, resolve, then (if a
/// commit/re-ack resulted) ack the effect and clear its pending marker.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(
        dead_code,
        reason = "driven by the wasm recovery timer; resolvers are unit-tested"
    )
)]
async fn reconcile_candidate(store: &RouterStore, candidate: ReclaimCandidate) {
    let key = candidate.key.clone();
    match candidate.state {
        ReservationState::Committed => {
            let verdict = read_proof_verdict(&candidate.proof_scope, candidate.claim).await;
            if let Some(ack) = resolve_committed_pending(store, &key, candidate.claim, &verdict)
                && ack_unique_effects(ack.target, vec![ack.effect_id])
                    .await
                    .is_ok()
            {
                store.clear_unique_acquire_ack(
                    key.graph_id,
                    key.constraint_id,
                    &key.encoded_value,
                    candidate.claim,
                );
            }
        }
        ReservationState::Reserved | ReservationState::Reclaiming => {
            let ticket = match candidate.state {
                ReservationState::Reserved => {
                    store.begin_unique_reclaim(key.graph_id, key.constraint_id, &key.encoded_value)
                }
                _ => {
                    store.resume_unique_reclaim(key.graph_id, key.constraint_id, &key.encoded_value)
                }
            };
            // The entry changed between scan and now (e.g. Confirm landed): nothing to reclaim.
            let Some(ticket) = ticket else {
                return;
            };
            let verdict = read_proof_verdict(&ticket.proof_scope, ticket.claim).await;
            if let Some(ack) = resolve_reclaim(store, &key, &ticket, &verdict)
                && ack_unique_effects(ack.target, vec![ack.effect_id])
                    .await
                    .is_ok()
            {
                store.clear_unique_acquire_ack(
                    key.graph_id,
                    key.constraint_id,
                    &key.encoded_value,
                    ticket.claim,
                );
            }
        }
    }
}

/// Run one bounded reclaim sweep starting after `cursor`. Returns the next cursor (`None` when the
/// keyspace was exhausted, i.e. start a fresh lap) and whether any candidate was found this sweep.
#[cfg_attr(
    not(target_family = "wasm"),
    allow(dead_code, reason = "driven by the wasm recovery timer")
)]
pub(crate) async fn run_reclaim_pass(
    cursor: Option<UniqueReservationKey>,
    budget: usize,
    now: u64,
) -> (Option<UniqueReservationKey>, bool) {
    let store = RouterStore::new();
    let (candidates, last_examined, scanned) =
        store.scan_unique_reclaim_candidates(cursor.as_ref(), budget, now);
    let found = !candidates.is_empty();
    for candidate in candidates {
        reconcile_candidate(&store, candidate).await;
    }
    let lap_complete = scanned < budget as u32;
    let next = if lap_complete { None } else { last_examined };
    (next, found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::label_stats::{
        ClientMutationKey, RouterMutationRecord, RouterMutationShardV1,
    };
    use crate::facade::stable::reservation_catalog;
    use crate::facade::stable::{ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_UNIQUE_RESERVATIONS};
    use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};
    use gleaph_graph_kernel::federation::{ShardId, UniqueAcquireProof};

    const CONSTRAINT: u16 = 5;

    fn constraint() -> ConstraintNameId {
        ConstraintNameId::from_raw(CONSTRAINT)
    }

    fn graph(seed: u32) -> GraphId {
        GraphId::from_raw(920_000 + seed)
    }

    fn res_key(g: GraphId, value: &[u8]) -> UniqueReservationKey {
        UniqueReservationKey::new(g, constraint(), value.to_vec())
    }

    fn lookup(g: GraphId, value: &[u8]) -> Option<reservation_catalog::ReservationRecord> {
        ROUTER_UNIQUE_RESERVATIONS.with_borrow(|t| t.get(&res_key(g, value)))
    }

    fn client_key(g: GraphId, mid: u64) -> ClientMutationKey {
        ClientMutationKey::new(Principal::anonymous(), g, format!("ck-{mid}"))
    }

    fn insert_envelope_record(key: &ClientMutationKey, mid: u64, completed_shard: bool) {
        let mut record = RouterMutationRecord::new(mid, 0, b"fp".to_vec());
        record.as_v1_mut().routing_in_progress = false;
        let mut shard = RouterMutationShardV1::new(ShardId::new(0), Principal::anonymous(), None);
        shard.set_completed(completed_shard);
        record.as_v1_mut().payload =
            crate::facade::stable::label_stats::RouterMutationPayloadV1::Scalar {
                shards: vec![shard],
            };
        ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|m| {
            m.insert(key.clone(), record);
        });
    }

    /// A Reserved reservation + its reverse-index row + an owning mutation record (envelope present,
    /// canonical shard `completed_shard`), mirroring a post-Try state. Returns the proof scope.
    fn setup_reserved(
        store: &RouterStore,
        g: GraphId,
        mid: u64,
        value: &[u8],
        completed_shard: bool,
    ) -> Vec<ProofShard> {
        let scope = vec![ProofShard::new(ShardId::new(0), Principal::anonymous())];
        reservation_catalog::try_reserve(
            g,
            mid,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: value.to_vec(),
                claim_ordinal: 0,
            }],
            &scope,
            0,
        )
        .expect("seed reserved");
        store.apply_reservation_slots(mid, &client_key(g, mid), 1);
        insert_envelope_record(&client_key(g, mid), mid, completed_shard);
        scope
    }

    fn evidence(mid: u64) -> UniqueAcquireEvidence {
        UniqueAcquireEvidence {
            effect_id: EffectId::new(mid, 0),
            owner_element_id: vec![7u8; 8],
        }
    }

    #[test]
    fn reclaim_present_proof_commits_decrements_and_requests_ack() {
        let store = RouterStore::new();
        let g = graph(1);
        let mid = 9_400_001;
        setup_reserved(&store, g, mid, b"v", false);
        let ticket = store
            .begin_unique_reclaim(g, constraint(), b"v")
            .expect("begin");

        let target = Principal::anonymous();
        let verdict = ProofVerdict::Present {
            evidence: evidence(mid),
            target,
        };
        let ack = resolve_reclaim(&store, &res_key(g, b"v"), &ticket, &verdict);

        assert_eq!(
            ack,
            Some(AckRequest {
                target,
                effect_id: EffectId::new(mid, 0)
            })
        );
        let record = lookup(g, b"v").expect("still present");
        assert_eq!(record.state, ReservationState::Committed);
        assert_eq!(record.pending_acquire_ack, Some(EffectId::new(mid, 0)));
        // Committed reservations no longer pin the record: the count was decremented to zero.
        assert!(store.reservation_index_client_key(mid).is_none());
    }

    #[test]
    fn reclaim_all_absent_terminalizable_cancels_and_decrements() {
        let store = RouterStore::new();
        let g = graph(2);
        let mid = 9_400_002;
        setup_reserved(&store, g, mid, b"v", false);
        let ticket = store
            .begin_unique_reclaim(g, constraint(), b"v")
            .expect("begin");

        let ack = resolve_reclaim(&store, &res_key(g, b"v"), &ticket, &ProofVerdict::AllAbsent);

        assert_eq!(ack, None);
        assert!(lookup(g, b"v").is_none(), "reservation cancelled");
        assert!(
            store.reservation_index_client_key(mid).is_none(),
            "count drained"
        );
        let record = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|m| m.get(&client_key(g, mid)))
            .expect("record");
        assert!(
            record.as_v1().terminal_failure.is_some(),
            "the mutation is terminally failed"
        );
    }

    #[test]
    fn reclaim_all_absent_with_completed_shard_holds() {
        let store = RouterStore::new();
        let g = graph(3);
        let mid = 9_400_003;
        // A completed canonical shard makes the dispatch non-uncommitted: terminal-fail is refused.
        setup_reserved(&store, g, mid, b"v", true);
        let ticket = store
            .begin_unique_reclaim(g, constraint(), b"v")
            .expect("begin");

        let ack = resolve_reclaim(&store, &res_key(g, b"v"), &ticket, &ProofVerdict::AllAbsent);

        assert_eq!(ack, None);
        let record = lookup(g, b"v").expect("still present");
        assert_eq!(
            record.state,
            ReservationState::Reserved,
            "held, not cancelled"
        );
        assert_eq!(
            record.reclaim_generation, ticket.generation,
            "generation kept"
        );
        assert!(
            store.reservation_index_client_key(mid).is_some(),
            "still pinned"
        );
        let mutation = ROUTER_MUTATION_BY_CLIENT_KEY
            .with_borrow(|m| m.get(&client_key(g, mid)))
            .expect("record");
        assert!(
            mutation.as_v1().terminal_failure.is_none(),
            "not terminally failed"
        );
    }

    #[test]
    fn reclaim_all_absent_missing_reverse_row_holds() {
        let store = RouterStore::new();
        let g = graph(4);
        let mid = 9_400_004;
        // Reservation only — no reverse-index row, so the owner cannot be resolved.
        let scope = vec![ProofShard::new(ShardId::new(0), Principal::anonymous())];
        reservation_catalog::try_reserve(
            g,
            mid,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"v".to_vec(),
                claim_ordinal: 0,
            }],
            &scope,
            0,
        )
        .expect("seed");
        let ticket = store
            .begin_unique_reclaim(g, constraint(), b"v")
            .expect("begin");

        let ack = resolve_reclaim(&store, &res_key(g, b"v"), &ticket, &ProofVerdict::AllAbsent);

        assert_eq!(ack, None);
        assert_eq!(
            lookup(g, b"v").expect("present").state,
            ReservationState::Reserved,
            "held: a missing reverse row cannot prove terminal failure"
        );
    }

    #[test]
    fn reclaim_inconclusive_holds() {
        let store = RouterStore::new();
        let g = graph(5);
        let mid = 9_400_005;
        setup_reserved(&store, g, mid, b"v", false);
        let ticket = store
            .begin_unique_reclaim(g, constraint(), b"v")
            .expect("begin");

        let ack = resolve_reclaim(
            &store,
            &res_key(g, b"v"),
            &ticket,
            &ProofVerdict::Inconclusive,
        );

        assert_eq!(ack, None);
        let record = lookup(g, b"v").expect("present");
        assert_eq!(record.state, ReservationState::Reserved, "held");
        assert!(
            store.reservation_index_client_key(mid).is_some(),
            "still pinned"
        );
    }

    #[test]
    fn committed_pending_present_requests_reack_without_clearing() {
        let store = RouterStore::new();
        let g = graph(6);
        let mid = 9_400_006;
        setup_reserved(&store, g, mid, b"v", false);
        let claim = ClaimId::new(mid, 0);
        assert_eq!(
            reservation_catalog::confirm_reservation(
                g,
                claim,
                constraint(),
                b"v",
                vec![7u8; 8],
                EffectId::new(mid, 0),
            ),
            reservation_catalog::ConfirmOutcome::FreshlyCommitted
        );

        let target = Principal::anonymous();
        let verdict = ProofVerdict::Present {
            evidence: evidence(mid),
            target,
        };
        let ack = resolve_committed_pending(&store, &res_key(g, b"v"), claim, &verdict);

        assert_eq!(
            ack,
            Some(AckRequest {
                target,
                effect_id: EffectId::new(mid, 0)
            })
        );
        // The clear is the caller's job, only after the ack succeeds: still pending here.
        assert_eq!(
            lookup(g, b"v").expect("present").pending_acquire_ack,
            Some(EffectId::new(mid, 0))
        );
    }

    #[test]
    fn committed_pending_all_absent_clears_ack_reply_lost() {
        let store = RouterStore::new();
        let g = graph(7);
        let mid = 9_400_007;
        setup_reserved(&store, g, mid, b"v", false);
        let claim = ClaimId::new(mid, 0);
        reservation_catalog::confirm_reservation(
            g,
            claim,
            constraint(),
            b"v",
            vec![7u8; 8],
            EffectId::new(mid, 0),
        );

        let ack =
            resolve_committed_pending(&store, &res_key(g, b"v"), claim, &ProofVerdict::AllAbsent);

        assert_eq!(ack, None);
        assert_eq!(
            lookup(g, b"v").expect("present").pending_acquire_ack,
            None,
            "ack-reply-lost clears the pending marker"
        );
    }

    #[test]
    fn committed_pending_inconclusive_holds() {
        let store = RouterStore::new();
        let g = graph(8);
        let mid = 9_400_008;
        setup_reserved(&store, g, mid, b"v", false);
        let claim = ClaimId::new(mid, 0);
        reservation_catalog::confirm_reservation(
            g,
            claim,
            constraint(),
            b"v",
            vec![7u8; 8],
            EffectId::new(mid, 0),
        );

        let ack = resolve_committed_pending(
            &store,
            &res_key(g, b"v"),
            claim,
            &ProofVerdict::Inconclusive,
        );

        assert_eq!(ack, None);
        assert_eq!(
            lookup(g, b"v").expect("present").pending_acquire_ack,
            Some(EffectId::new(mid, 0)),
            "held: pending marker preserved"
        );
    }

    fn proof_row(mid: u64, acquire: Option<UniqueAcquireEvidence>) -> UniqueAcquireProof {
        UniqueAcquireProof {
            claim_id: ClaimId::new(mid, 0),
            acquire,
        }
    }

    #[test]
    fn classify_empty_scope_is_inconclusive() {
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &[]),
            ProofVerdict::Inconclusive
        );
    }

    #[test]
    fn classify_explicit_absent_on_all_shards_is_all_absent() {
        let t = Principal::anonymous();
        let responses = vec![
            (t, Some(vec![proof_row(1, None)])),
            (t, Some(vec![proof_row(1, None)])),
        ];
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &responses),
            ProofVerdict::AllAbsent
        );
    }

    #[test]
    fn classify_missing_claim_row_is_inconclusive_not_absent() {
        let t = Principal::anonymous();
        // A success response that simply lacks our claim's row is an incomplete negative.
        let responses = vec![(t, Some(Vec::new()))];
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &responses),
            ProofVerdict::Inconclusive
        );
        // A response carrying only a *different* claim's row is likewise not our absence.
        let responses = vec![(t, Some(vec![proof_row(999, None)]))];
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &responses),
            ProofVerdict::Inconclusive
        );
    }

    #[test]
    fn classify_unreachable_shard_is_inconclusive() {
        let t = Principal::anonymous();
        let responses = vec![(t, Some(vec![proof_row(1, None)])), (t, None)];
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &responses),
            ProofVerdict::Inconclusive
        );
    }

    #[test]
    fn classify_present_acquire_wins_even_with_other_inconclusive_shards() {
        let present = Principal::management_canister();
        let evidence = evidence(1);
        let responses = vec![
            (Principal::anonymous(), None),
            (present, Some(vec![proof_row(1, Some(evidence.clone()))])),
        ];
        assert_eq!(
            classify_proof_responses(ClaimId::new(1, 0), &responses),
            ProofVerdict::Present {
                evidence,
                target: present
            }
        );
    }

    #[test]
    fn scan_selects_overdue_reserved_reclaiming_and_committed_pending() {
        let store = RouterStore::new();
        let g = graph(9);
        let now = reservation_catalog::UNIQUE_RESERVATION_TTL_NS * 2;

        // Fresh Reserved (within TTL) — excluded.
        reservation_catalog::try_reserve(
            g,
            9_410_001,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"fresh".to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            now,
        )
        .expect("fresh");

        // Overdue Reserved — included.
        reservation_catalog::try_reserve(
            g,
            9_410_002,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"overdue".to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            0,
        )
        .expect("overdue");

        // Reclaiming (any age) — included.
        reservation_catalog::try_reserve(
            g,
            9_410_003,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"reclaiming".to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            now,
        )
        .expect("reclaiming");
        reservation_catalog::begin_reclaim(g, constraint(), b"reclaiming").expect("begin");

        // Committed + pending — included.
        reservation_catalog::try_reserve(
            g,
            9_410_004,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"pending".to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            now,
        )
        .expect("committed");
        reservation_catalog::confirm_reservation(
            g,
            ClaimId::new(9_410_004, 0),
            constraint(),
            b"pending",
            vec![7u8; 8],
            EffectId::new(9_410_004, 0),
        );

        // Committed, ack cleared (no pending) — excluded.
        reservation_catalog::try_reserve(
            g,
            9_410_005,
            &[reservation_catalog::ReservationClaim {
                constraint_id: constraint(),
                encoded_value: b"acked".to_vec(),
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
            now,
        )
        .expect("acked");
        reservation_catalog::confirm_reservation(
            g,
            ClaimId::new(9_410_005, 0),
            constraint(),
            b"acked",
            vec![7u8; 8],
            EffectId::new(9_410_005, 0),
        );
        reservation_catalog::clear_acquire_ack(
            g,
            constraint(),
            b"acked",
            ClaimId::new(9_410_005, 0),
        );

        let (candidates, _next, _scanned) = store.scan_unique_reclaim_candidates(None, 4096, now);
        let mut values: Vec<Vec<u8>> = candidates
            .into_iter()
            .filter(|c| c.key.graph_id == g)
            .map(|c| c.key.encoded_value)
            .collect();
        values.sort();
        assert_eq!(
            values,
            vec![
                b"overdue".to_vec(),
                b"pending".to_vec(),
                b"reclaiming".to_vec()
            ]
        );
    }
}
