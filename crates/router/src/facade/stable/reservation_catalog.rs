//! Router-owned cross-shard uniqueness reservation table (ADR 0030).
//!
//! `UNIQUE_RESERVATIONS` is keyed by `(graph_id, constraint_id, encoded_value)` and is the **single
//! enforcement point** for the cross-shard uniqueness negative invariant. Each entry stages a
//! TCC claim: `Reserved` (Try done, canonical write not yet confirmed), `Reclaiming` (a
//! generation-fenced recovery proof is in flight), or `Committed` (canonical `Acquire` confirmed).
//!
//! This module owns the table's schema and the **no-`await` Try** transition (claim-set dedup,
//! read-only preflight, all-or-nothing apply). The remaining transitions — Confirm/Cancel driven by
//! the graph-shard unique-effect outbox, and the generation-fenced reclaim proof — land in later
//! ADR 0030 slices (4–6); this slice deliberately classifies any live `Reserved`/`Reclaiming`
//! holder by another mutation as *in-flight* (retryable) rather than attempting a reclaim, which is
//! the safe subset (it never falsely admits a duplicate).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_gql_ic::MAX_UNIQUE_ENCODED_VALUE_LEN;
use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};
use gleaph_graph_kernel::federation::{ClaimId, ShardId};
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_UNIQUE_RESERVATIONS;
use crate::state::RouterError;

/// Maximum encoded reservation-key byte length: `graph_id` (4) + `constraint_id` (2) + the canonical
/// `encoded_value` bound shared with `gleaph_gql_ic::unique_key`.
const MAX_RESERVATION_KEY_LEN: usize = 6 + MAX_UNIQUE_ENCODED_VALUE_LEN;

/// Reservation key: the exact canonical-encoded value bytes (not a hash) under a constraint.
///
/// Ordering is `graph_id`, then `constraint_id`, then `encoded_value` — `StableBTreeMap` compares
/// keys by this `Ord`, so a `(graph_id, constraint_id)` prefix is a contiguous range.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct UniqueReservationKey {
    pub graph_id: GraphId,
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
}

impl UniqueReservationKey {
    pub fn new(graph_id: GraphId, constraint_id: ConstraintNameId, encoded_value: Vec<u8>) -> Self {
        Self {
            graph_id,
            constraint_id,
            encoded_value,
        }
    }
}

impl Storable for UniqueReservationKey {
    // Bounded so stable memory itself enforces the Slice 2 `encoded_value` ceiling: a malformed
    // caller cannot persist an oversized key. `try_reserve` validates length before any write, so a
    // key reaching here is always within bound.
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: MAX_RESERVATION_KEY_LEN as u32,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(6 + self.encoded_value.len());
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.constraint_id.to_le_bytes());
        out.extend_from_slice(&self.encoded_value);
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let graph_id = GraphId::from_le_bytes(bytes[0..4].try_into().expect("graph_id"));
        let constraint_id =
            ConstraintNameId::from_le_bytes(bytes[4..6].try_into().expect("constraint_id"));
        Self {
            graph_id,
            constraint_id,
            encoded_value: bytes[6..].to_vec(),
        }
    }
}

/// A canister-resolved dispatch target the claim may have committed on.
///
/// `proof_scope` must pin the **`graph_canister`**, not just the graph-local `shard_id`: a shard id
/// can be unregistered and reused by a *different* canister, so a bare shard id would let recovery
/// query the wrong (new) canister, see no `Acquire`, and unsafely Cancel a claim that committed on
/// the old canister (ADR 0030 §Timeout). This captures the same `(shard_id, graph_canister)`
/// identity the dispatch envelope (`RouterMutationShard`) exposes; it deliberately omits that type's
/// mutable execution fields (`completed`, `row_count`, …), which the dispatch record owns and the
/// reservation must not.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, CandidType, Serialize, Deserialize)]
pub(crate) struct ProofShard {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
}

impl ProofShard {
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "constructed from the dispatch envelope in ADR 0030 slice 5"
        )
    )]
    pub fn new(shard_id: ShardId, graph_canister: Principal) -> Self {
        Self {
            shard_id,
            graph_canister,
        }
    }
}

/// Canonicalizes a `proof_scope` to its **set** form — sorted and de-duplicated by
/// `(shard_id, graph_canister)` — so the stored scope and a later replay's scope compare
/// independently of enumeration order. Stored scopes are always normalized at insert time.
fn normalized_scope(scope: &[ProofShard]) -> Vec<ProofShard> {
    let mut out = scope.to_vec();
    out.sort();
    out.dedup();
    out
}

/// Staged lifecycle of a reservation (ADR 0030 §"Prepare / commit / cancel / recovery").
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) enum ReservationState {
    /// Try has reserved the value; the canonical write is not yet confirmed.
    Reserved,
    /// A generation-fenced reclaim proof is in flight; Try fences this value.
    Reclaiming,
    /// The canonical `Acquire` effect has been confirmed.
    Committed,
}

/// One reservation record. `claim` is immutable for the life of the claim; `owner_element_id`,
/// `reclaim_generation`, and `state` evolve through the lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct ReservationRecord {
    pub claim: ClaimId,
    pub state: ReservationState,
    /// Persistent monotone fencing token, retained across `Reclaiming → Reserved` (ABA-safe).
    pub reclaim_generation: u64,
    /// Canonical element that owns the value; `None` until Confirm. `Release` matches on this.
    pub owner_element_id: Option<[u8; 16]>,
    pub reserved_at_ns: u64,
    /// Complete canister-resolved target set the claim may have committed on; the reclaim proof
    /// reads it from here, so a GC'd `RouterMutationRecord` cannot strand recovery.
    pub proof_scope: Vec<ProofShard>,
}

/// Versioned stable envelope (ADR 0007), so the record schema can evolve across upgrades.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum ReservationStableRecord {
    V1(ReservationRecord),
}

impl Storable for ReservationRecord {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&ReservationStableRecord::V1(self.clone())).expect("encode reservation"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ReservationStableRecord::V1(self)).expect("encode reservation")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ReservationStableRecord).expect("decode reservation") {
            ReservationStableRecord::V1(v1) => v1,
        }
    }
}

/// One uniqueness claim a mutation intends to write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReservationClaim {
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
    pub claim_ordinal: u32,
}

/// No-`await` Try (Reserve): claim-set dedup → read-only preflight → all-or-nothing apply.
///
/// On the IC a `Result::Err` does **not** roll back state — only a trap does. So Try must never
/// mutate the table until it knows *every* claim is insertable: it computes the full classification
/// first (mutating nothing), returns `Err` before any write on the first conflict, and only then,
/// in the same message with no intervening `await`, inserts all reservations together.
///
/// Errors:
/// - [`RouterError::UniquenessViolation`] (non-retryable): an intra-mutation duplicate value, a
///   duplicate `claim_ordinal` in the set, a value already `Committed`, a value `Reserved` by a
///   *different claim of the same mutation* (a duplicate surfacing under partial replay), or an
///   idempotent replay whose `proof_scope` disagrees with the stored one (a placement change, which
///   must not silently retarget dispatch).
/// - [`RouterError::UniquenessReservationInFlight`] (retryable): a value held `Reserved` by another
///   live mutation, or `Reclaiming` (fenced — *always*, regardless of which claim owns it) — retry
///   after that saga resolves.
/// - [`RouterError::Internal`] (non-retryable): a defensive backstop if an `encoded_value` exceeds
///   the shared `MAX_UNIQUE_ENCODED_VALUE_LEN`. The public `encode_unique_value` already rejects
///   over-length values, so reaching this means that contract was bypassed.
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "wired into the INSERT write path in ADR 0030 slice 5"
    )
)]
pub(crate) fn try_reserve(
    graph_id: GraphId,
    mutation_id: MutationId,
    claims: &[ReservationClaim],
    proof_scope: &[ProofShard],
    now_ns: u64,
) -> Result<(), RouterError> {
    // Phase 1 — claim set. Reject, deterministically and non-retryably:
    //   (a) duplicate `(constraint_id, encoded_value)` — the same value claimed twice;
    //   (b) duplicate `claim_ordinal` — two values sharing one `ClaimId`, which would make Slice 4's
    //       `Acquire` matching ambiguous and could move one `ClaimId` onto two keys.
    // Also enforce the Slice 2 length bound here, before any table read or write.
    let mut seen_values: BTreeSet<(ConstraintNameId, &[u8])> = BTreeSet::new();
    let mut seen_ordinals: BTreeSet<u32> = BTreeSet::new();
    for claim in claims {
        if claim.encoded_value.len() > MAX_UNIQUE_ENCODED_VALUE_LEN {
            return Err(RouterError::Internal(format!(
                "encoded value for constraint {} is {} bytes, exceeds MAX_UNIQUE_ENCODED_VALUE_LEN \
                 ({MAX_UNIQUE_ENCODED_VALUE_LEN})",
                claim.constraint_id,
                claim.encoded_value.len()
            )));
        }
        if !seen_values.insert((claim.constraint_id, claim.encoded_value.as_slice())) {
            return Err(RouterError::UniquenessViolation(format!(
                "mutation {mutation_id} claims the same value twice for constraint {}",
                claim.constraint_id
            )));
        }
        if !seen_ordinals.insert(claim.claim_ordinal) {
            return Err(RouterError::UniquenessViolation(format!(
                "mutation {mutation_id} reuses claim_ordinal {} across distinct values",
                claim.claim_ordinal
            )));
        }
    }

    // `proof_scope` is a *set*: normalize once so stored scopes are canonical and replay comparison
    // is order-independent.
    let scope = normalized_scope(proof_scope);

    // Phase 2 — preflight (read-only): classify every claim without mutating the table.
    let mut to_insert: Vec<(UniqueReservationKey, ClaimId)> = Vec::with_capacity(claims.len());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
        for claim in claims {
            let claim_id = ClaimId::new(mutation_id, claim.claim_ordinal);
            let key =
                UniqueReservationKey::new(graph_id, claim.constraint_id, claim.encoded_value.clone());
            let Some(existing) = table.get(&key) else {
                to_insert.push((key, claim_id));
                continue;
            };

            // `Reclaiming` fences the value unconditionally — even for *this* claim's own retry. The
            // reclaim proof is mid-flight (it may be about to Cancel based on an outbox-absence read);
            // letting any claim dispatch now reopens the ADR 0030 race where the old claim commits
            // after the absence check and is then Cancelled. Must precede the idempotent-claim path.
            if existing.state == ReservationState::Reclaiming {
                return Err(RouterError::UniquenessReservationInFlight(format!(
                    "value is being reclaimed for constraint {}; retry after the proof resolves",
                    claim.constraint_id
                )));
            }

            if existing.claim == claim_id {
                // Idempotent replay of this exact claim (Reserved or already Committed): no insert.
                // The stored `proof_scope` is authoritative for recovery; if the replay carries a
                // different scope the placement changed under us, so refuse rather than silently
                // dispatch to a target the reservation will not be reconciled against.
                if existing.proof_scope != scope {
                    return Err(RouterError::UniquenessViolation(format!(
                        "claim {claim_id:?} replayed with a different shard scope for constraint {}",
                        claim.constraint_id
                    )));
                }
                continue;
            }

            match existing.state {
                ReservationState::Committed => {
                    return Err(RouterError::UniquenessViolation(format!(
                        "value already committed for constraint {}",
                        claim.constraint_id
                    )));
                }
                ReservationState::Reserved if existing.claim.mutation_id == mutation_id => {
                    return Err(RouterError::UniquenessViolation(format!(
                        "mutation {mutation_id} duplicates a value across claims for constraint {}",
                        claim.constraint_id
                    )));
                }
                ReservationState::Reserved => {
                    return Err(RouterError::UniquenessReservationInFlight(format!(
                        "value reserved by in-flight mutation {} for constraint {}",
                        existing.claim.mutation_id, claim.constraint_id
                    )));
                }
                ReservationState::Reclaiming => unreachable!("handled before the idempotent path"),
            }
        }
        Ok(())
    })?;

    // Phase 3 — apply: no conflict and no `await` since preflight, so the classification still
    // holds. Insert every new reservation together; already-reserved claims need no write.
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        for (key, claim_id) in to_insert {
            table.insert(
                key,
                ReservationRecord {
                    claim: claim_id,
                    state: ReservationState::Reserved,
                    reclaim_generation: 0,
                    owner_element_id: None,
                    reserved_at_ns: now_ns,
                    proof_scope: scope.clone(),
                },
            );
        }
    });

    Ok(())
}

fn graph_id_upper_bound(graph_id: GraphId) -> GraphId {
    GraphId::from_raw(graph_id.raw().saturating_add(1))
}

/// Removes every reservation for a graph (graph teardown). Mirrors the constraint catalog purge.
pub(crate) fn purge_graph_reservations(graph_id: GraphId) {
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let start = UniqueReservationKey::new(graph_id, ConstraintNameId::from_raw(0), Vec::new());
        let end = UniqueReservationKey::new(
            graph_id_upper_bound(graph_id),
            ConstraintNameId::from_raw(0),
            Vec::new(),
        );
        let keys: Vec<_> = table
            .range((Bound::Included(start), Bound::Excluded(end)))
            .map(|entry| entry.key().clone())
            .collect();
        for key in keys {
            table.remove(&key);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(constraint: u16, value: &[u8], ordinal: u32) -> ReservationClaim {
        ReservationClaim {
            constraint_id: ConstraintNameId::from_raw(constraint),
            encoded_value: value.to_vec(),
            claim_ordinal: ordinal,
        }
    }

    /// A single-target proof scope on the default (anonymous) canister.
    fn scope(shard: u32) -> Vec<ProofShard> {
        vec![ProofShard::new(ShardId::new(shard), Principal::anonymous())]
    }

    /// A single-target proof scope on a *distinct* canister (placement-change simulation).
    fn scope_on(shard: u32, canister: Principal) -> Vec<ProofShard> {
        vec![ProofShard::new(ShardId::new(shard), canister)]
    }

    fn reservation_count(graph_id: GraphId) -> usize {
        ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
            let start =
                UniqueReservationKey::new(graph_id, ConstraintNameId::from_raw(0), Vec::new());
            let end = UniqueReservationKey::new(
                graph_id_upper_bound(graph_id),
                ConstraintNameId::from_raw(0),
                Vec::new(),
            );
            table
                .range((Bound::Included(start), Bound::Excluded(end)))
                .count()
        })
    }

    fn lookup(graph_id: GraphId, constraint: u16, value: &[u8]) -> Option<ReservationRecord> {
        ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
            table.get(&UniqueReservationKey::new(
                graph_id,
                ConstraintNameId::from_raw(constraint),
                value.to_vec(),
            ))
        })
    }

    /// Each test uses a unique graph id so the shared thread-local table does not collide.
    fn fresh_graph(seed: u32) -> GraphId {
        GraphId::from_raw(800_000 + seed)
    }

    #[test]
    fn key_storable_roundtrip_preserves_order_fields() {
        let key = UniqueReservationKey::new(
            GraphId::from_raw(7),
            ConstraintNameId::from_raw(3),
            vec![1, 2, 0, 3],
        );
        let decoded = UniqueReservationKey::from_bytes(Cow::Owned(key.clone().into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn record_storable_roundtrip() {
        let record = ReservationRecord {
            claim: ClaimId::new(42, 1),
            state: ReservationState::Reserved,
            reclaim_generation: 0,
            owner_element_id: Some([9u8; 16]),
            reserved_at_ns: 123,
            proof_scope: vec![
                ProofShard::new(ShardId::new(1), Principal::anonymous()),
                ProofShard::new(ShardId::new(2), Principal::management_canister()),
            ],
        };
        let decoded = ReservationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
    }

    #[test]
    fn try_reserve_inserts_all_claims_as_reserved() {
        let g = fresh_graph(1);
        let claims = [claim(1, b"alice", 0), claim(1, b"bob", 1)];
        try_reserve(g, 100, &claims, &scope(1), 1_000).expect("reserve");

        assert_eq!(reservation_count(g), 2);
        let rec = lookup(g, 1, b"alice").expect("alice reserved");
        assert_eq!(rec.state, ReservationState::Reserved);
        assert_eq!(rec.claim, ClaimId::new(100, 0));
        assert_eq!(rec.reclaim_generation, 0);
        assert_eq!(rec.owner_element_id, None);
        assert_eq!(rec.proof_scope, scope(1));
    }

    #[test]
    fn intra_mutation_duplicate_in_claim_set_is_nonretryable_violation() {
        let g = fresh_graph(2);
        let claims = [claim(1, b"dup", 0), claim(1, b"dup", 1)];
        let err = try_reserve(g, 100, &claims, &scope(1), 1_000).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessViolation(_)),
            "{err:?}"
        );
        // All-or-nothing: the first claim must not have been written.
        assert_eq!(reservation_count(g), 0);
    }

    #[test]
    fn committed_value_is_nonretryable_violation() {
        let g = fresh_graph(3);
        try_reserve(g, 100, &[claim(1, b"taken", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"taken", ReservationState::Committed);

        let err = try_reserve(g, 200, &[claim(1, b"taken", 0)], &scope(1), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessViolation(_)),
            "{err:?}"
        );
    }

    #[test]
    fn value_reserved_by_other_live_mutation_is_retryable_in_flight() {
        let g = fresh_graph(4);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let err = try_reserve(g, 200, &[claim(1, b"v", 0)], &scope(1), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessReservationInFlight(_)),
            "{err:?}"
        );
    }

    fn force_state(g: GraphId, value: &[u8], state: ReservationState) {
        ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
            let key = UniqueReservationKey::new(g, ConstraintNameId::from_raw(1), value.to_vec());
            let mut rec = table.get(&key).unwrap();
            rec.state = state;
            table.insert(key, rec);
        });
    }

    #[test]
    fn reclaiming_value_is_retryable_and_fenced() {
        let g = fresh_graph(5);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Reclaiming);
        let err = try_reserve(g, 200, &[claim(1, b"v", 0)], &scope(1), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessReservationInFlight(_)),
            "{err:?}"
        );
    }

    #[test]
    fn reclaiming_fences_even_the_same_claims_own_retry() {
        // Regression: a `Reclaiming` proof may be about to Cancel based on an outbox-absence read.
        // Even an idempotent retry of the *same* ClaimId must be fenced (not treated as
        // already-reserved), or the old claim could commit after the absence check and be Cancelled.
        let g = fresh_graph(11);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Reclaiming);
        let err = try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessReservationInFlight(_)),
            "same-claim retry during Reclaiming must be fenced, got {err:?}"
        );
    }

    #[test]
    fn idempotent_replay_of_same_claim_is_ok_without_double_insert() {
        let g = fresh_graph(6);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("first");
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 2).expect("replay");
        assert_eq!(reservation_count(g), 1);
        // The original reserved_at_ns is preserved (no overwrite on idempotent replay).
        assert_eq!(lookup(g, 1, b"v").unwrap().reserved_at_ns, 1);
    }

    #[test]
    fn same_mutation_different_claim_on_held_value_is_nonretryable_violation() {
        // A value already Reserved by claim_ordinal 0 of mutation 100; a *different* ordinal of the
        // same mutation hitting it (partial-replay interleaving) is a duplicate, not in-flight.
        let g = fresh_graph(7);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let err = try_reserve(g, 100, &[claim(1, b"v", 9)], &scope(1), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessViolation(_)),
            "{err:?}"
        );
    }

    #[test]
    fn conflict_on_later_claim_writes_nothing_from_the_batch() {
        // First reserve "b" by another mutation, then a batch [a, b]: the conflict on "b" must
        // leave "a" unwritten (all-or-nothing apply).
        let g = fresh_graph(8);
        try_reserve(g, 100, &[claim(1, b"b", 0)], &scope(1), 1).expect("reserve b");
        let err = try_reserve(
            g,
            200,
            &[claim(1, b"a", 0), claim(1, b"b", 1)],
            &scope(1),
            2,
        )
        .unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessReservationInFlight(_)),
            "{err:?}"
        );
        assert_eq!(lookup(g, 1, b"a"), None, "a must not be written");
    }

    #[test]
    fn purge_graph_reservations_removes_only_that_graph() {
        let g1 = fresh_graph(9);
        let g2 = fresh_graph(10);
        try_reserve(g1, 1, &[claim(1, b"x", 0)], &scope(1), 1).expect("g1");
        try_reserve(g2, 2, &[claim(1, b"y", 0)], &scope(1), 1).expect("g2");
        purge_graph_reservations(g1);
        assert_eq!(reservation_count(g1), 0);
        assert_eq!(reservation_count(g2), 1);
    }

    #[test]
    fn reused_claim_ordinal_across_distinct_values_is_nonretryable_violation() {
        // Two distinct values under one ordinal would mint the same ClaimId for two reservations,
        // making Slice 4's Acquire matching ambiguous. Rejected before any write.
        let g = fresh_graph(12);
        let claims = [claim(1, b"a", 0), claim(1, b"b", 0)];
        let err = try_reserve(g, 100, &claims, &scope(1), 1).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessViolation(_)),
            "{err:?}"
        );
        assert_eq!(reservation_count(g), 0);
    }

    #[test]
    fn idempotent_replay_with_different_scope_is_rejected() {
        // The stored proof_scope is authoritative for recovery; a replay that resolves to a
        // different canister (shard id reused) must not silently retarget dispatch.
        let g = fresh_graph(13);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let other = Principal::management_canister();
        let err = try_reserve(g, 100, &[claim(1, b"v", 0)], &scope_on(1, other), 2).unwrap_err();
        assert!(
            matches!(err, RouterError::UniquenessViolation(_)),
            "scope mismatch must be rejected, got {err:?}"
        );
        // Original reservation is untouched.
        assert_eq!(lookup(g, 1, b"v").unwrap().proof_scope, scope(1));
    }

    #[test]
    fn replay_with_reordered_or_duplicated_scope_succeeds() {
        // proof_scope is a set: a replay enumerating the same targets in a different order (or with
        // duplicates) must not be mistaken for a placement change.
        let g = fresh_graph(15);
        let a = Principal::anonymous();
        let b = Principal::management_canister();
        let first = vec![
            ProofShard::new(ShardId::new(1), a),
            ProofShard::new(ShardId::new(2), b),
        ];
        try_reserve(g, 100, &[claim(1, b"v", 0)], &first, 1).expect("reserve");

        let reordered_with_dup = vec![
            ProofShard::new(ShardId::new(2), b),
            ProofShard::new(ShardId::new(1), a),
            ProofShard::new(ShardId::new(2), b),
        ];
        try_reserve(g, 100, &[claim(1, b"v", 0)], &reordered_with_dup, 2).expect("replay");
        assert_eq!(reservation_count(g), 1);
        // Stored scope is canonical (sorted, de-duplicated).
        assert_eq!(
            lookup(g, 1, b"v").unwrap().proof_scope,
            vec![
                ProofShard::new(ShardId::new(1), a),
                ProofShard::new(ShardId::new(2), b)
            ]
        );
    }

    #[test]
    fn oversized_encoded_value_is_rejected_before_any_write() {
        let g = fresh_graph(14);
        let oversized = vec![0u8; MAX_UNIQUE_ENCODED_VALUE_LEN + 1];
        let err = try_reserve(g, 100, &[claim(1, &oversized, 0)], &scope(1), 1).unwrap_err();
        assert!(matches!(err, RouterError::Internal(_)), "{err:?}");
        assert_eq!(reservation_count(g), 0);
        // A value exactly at the bound is admitted.
        let at_bound = vec![0u8; MAX_UNIQUE_ENCODED_VALUE_LEN];
        try_reserve(g, 100, &[claim(1, &at_bound, 0)], &scope(1), 1).expect("at-bound admitted");
        assert_eq!(reservation_count(g), 1);
    }
}
