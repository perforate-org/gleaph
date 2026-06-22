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
use gleaph_graph_kernel::federation::{ClaimId, EffectId, ShardId};
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};

use crate::facade::stable::ROUTER_UNIQUE_RESERVATIONS;
use crate::state::RouterError;

/// Maximum encoded reservation-key byte length: `graph_id` (4) + `constraint_id` (2) + the canonical
/// `encoded_value` bound shared with `gleaph_gql_ic::unique_key`.
const MAX_RESERVATION_KEY_LEN: usize = 6 + MAX_UNIQUE_ENCODED_VALUE_LEN;

/// Minimum age before a `Reserved` reservation is *eligible* for a reclaim proof (ADR 0030
/// §Timeout). The TTL only gates **eligibility** — it is never the authority to cancel, which comes
/// solely from the generation-fenced proof (terminal-`Failed` mutation **and** every `proof_scope`
/// shard reachable and reporting the `Acquire` absent). It is set well above the routing-lease
/// window ([`ROUTING_LEASE_TTL_NS`], its documented lower bound) plus the canonical dispatch round
/// trip, so a slow-but-live saga is never needlessly reclaimed while still in flight.
pub(crate) const UNIQUE_RESERVATION_TTL_NS: u64 = 30 * 60 * 1_000_000_000;

// The eligibility window must dominate the routing lease: while a routing lease can still be live (or
// freshly reclaimed for retry), reclaiming the value's reservation would be wasted work.
const _: () = assert!(UNIQUE_RESERVATION_TTL_NS >= crate::facade::store::ROUTING_LEASE_TTL_NS);

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
    /// Canonical element that owns the value (its exact encoded element-id bytes); `None` until
    /// Confirm. `Release` matches on this.
    pub owner_element_id: Option<Vec<u8>>,
    pub reserved_at_ns: u64,
    /// Complete canister-resolved target set the claim may have committed on; the reclaim proof
    /// reads it from here, so a GC'd `RouterMutationRecord` cannot strand recovery.
    pub proof_scope: Vec<ProofShard>,
    /// The `Acquire` effect proven at `→ Committed` whose ack (unpin) has **not** been confirmed.
    /// Set atomically with the `→ Committed` transition (normal Confirm and reclaim commit) and
    /// cleared only after the ack succeeds, so a Router crash between commit and ack leaves a durable
    /// trail: a `Committed` record with `Some(..)` is the slice-6 reconciler's re-discovery handle
    /// for the still-pinned `Acquire`. `None` once acked (or for records that never committed).
    #[serde(default)]
    pub pending_acquire_ack: Option<EffectId>,
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
/// On success returns the number of **fresh** reservations inserted (idempotent replays insert
/// nothing). The caller uses this to bump the non-terminal reservation count for `mutation_id` by
/// exactly the fresh count (ADR 0030 slice 6 reverse index); the count overflow must be preflighted
/// by the caller *before* this apply, since this apply itself is infallible.
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
pub(crate) fn try_reserve(
    graph_id: GraphId,
    mutation_id: MutationId,
    claims: &[ReservationClaim],
    proof_scope: &[ProofShard],
    now_ns: u64,
) -> Result<u32, RouterError> {
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
    let fresh = to_insert.len() as u32;
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
                    pending_acquire_ack: None,
                },
            );
        }
    });

    Ok(fresh)
}

/// Outcome of a [`confirm_reservation`] attempt (ADR 0030). Distinguishes the **fresh** transition
/// — the only one that leaves the non-terminal reservation set — from an idempotent re-confirm, so
/// the caller decrements its mutation's non-terminal count exactly once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfirmOutcome {
    /// `Reserved → Committed` happened now: ack the `Acquire` **and** decrement the non-terminal
    /// count for this reservation's mutation.
    FreshlyCommitted,
    /// Already `Committed` by this claim (idempotent replay): ack the `Acquire` again so a previously
    /// failed ack is retried, but the count was already decremented on the fresh transition.
    AlreadyCommitted,
    /// Missing record, a different claim's reservation, or `Reclaiming`: no ack, no count change.
    NotApplicable,
}

/// Confirm: `Reserved → Committed`, stamping the canonical `owner_element_id` (ADR 0030 slice 5).
///
/// Runs **after** the shard's canonical write is durable and its `Acquire` proof has been read, so
/// it must be best-effort and idempotent: the canonical write cannot be rolled back, and a crashed
/// Router resumes the same Confirm on replay. It therefore never traps and only ever advances its
/// own `Reserved` claim:
/// - missing record, or a record owned by a *different* claim: a no-op (`false`) — the reservation
///   was reclaimed/superseded; the slice-6 recovery reconciler owns that case, not Confirm.
/// - `Reclaiming`: left untouched ([`ConfirmOutcome::NotApplicable`]) — a generation-fenced reclaim
///   proof is authoritative.
/// - already `Committed` by this claim: idempotent ([`ConfirmOutcome::AlreadyCommitted`]); the owner
///   and `pending_acquire_ack` are not rewritten (an earlier ack may already have cleared the latter
///   — re-acking is harmless, but it must **not** decrement the non-terminal count again).
/// - `Reserved` by this claim: transitions to `Committed` ([`ConfirmOutcome::FreshlyCommitted`]),
///   stamping `owner_element_id` **and** `pending_acquire_ack = Some(effect_id)` in the same stable
///   write, so a crash before the ack leaves the still-pinned `Acquire` re-discoverable (a
///   `Committed` record with a pending ack). This is the only outcome that leaves the non-terminal
///   set, so the caller decrements the reservation's mutation count **only** on it.
pub(crate) fn confirm_reservation(
    graph_id: GraphId,
    claim_id: ClaimId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    owner_element_id: Vec<u8>,
    effect_id: EffectId,
) -> ConfirmOutcome {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(mut record) = table.get(&key) else {
            return ConfirmOutcome::NotApplicable;
        };
        if record.claim != claim_id {
            return ConfirmOutcome::NotApplicable;
        }
        match record.state {
            ReservationState::Committed => ConfirmOutcome::AlreadyCommitted,
            ReservationState::Reclaiming => ConfirmOutcome::NotApplicable,
            ReservationState::Reserved => {
                record.state = ReservationState::Committed;
                record.owner_element_id = Some(owner_element_id);
                record.pending_acquire_ack = Some(effect_id);
                table.insert(key, record);
                ConfirmOutcome::FreshlyCommitted
            }
        }
    })
}

/// Clear `pending_acquire_ack` after the `Acquire` effect's ack (unpin) has succeeded (ADR 0030
/// slice 6). Idempotent and fenced: only a `Committed` record owned by `claim_id` is cleared, so a
/// stale callback from a superseded claim cannot suppress a live record's re-ack. Returns `true` iff
/// it cleared a previously-pending ack (i.e. the reconciler may stop re-discovering this `Acquire`).
pub(crate) fn clear_acquire_ack(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    claim_id: ClaimId,
) -> bool {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(mut record) = table.get(&key) else {
            return false;
        };
        if record.claim != claim_id
            || record.state != ReservationState::Committed
            || record.pending_acquire_ack.is_none()
        {
            return false;
        }
        record.pending_acquire_ack = None;
        table.insert(key, record);
        true
    })
}

/// A captured fence for an in-flight generation-fenced reclaim proof (ADR 0030 §Timeout). The proof
/// reads `proof_scope` for a `ClaimId`-matched `Acquire`, then applies its outcome **only** if the
/// entry is still `Reclaiming` with `reclaim_generation == generation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimTicket {
    pub claim: ClaimId,
    pub generation: u64,
    pub proof_scope: Vec<ProofShard>,
}

/// Begin a reclaim proof for a `Reserved` value (ADR 0030 §Timeout step 1): atomically and with **no
/// `await`**, checked-increment the persistent `reclaim_generation` to a fresh `g` and set
/// `Reclaiming`, capturing the fence the proof re-checks against. While `Reclaiming`, Try fences the
/// value, so no claim can commit during the proof. Returns `None` (abort the proof) when the entry
/// is absent or not `Reserved` (already `Reclaiming`/`Committed`), or — defensively — when the
/// generation would overflow `u64` (astronomically unreachable; leaves the entry untouched).
pub(crate) fn begin_reclaim(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
) -> Option<ReclaimTicket> {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let mut record = table.get(&key)?;
        if record.state != ReservationState::Reserved {
            return None;
        }
        let generation = record.reclaim_generation.checked_add(1)?;
        record.reclaim_generation = generation;
        record.state = ReservationState::Reclaiming;
        let ticket = ReclaimTicket {
            claim: record.claim,
            generation,
            proof_scope: record.proof_scope.clone(),
        };
        table.insert(key, record);
        Some(ticket)
    })
}

/// Resume an interrupted reclaim proof (ADR 0030 §Timeout): the entry is already `Reclaiming` (e.g.
/// the Router crashed mid-proof, or the prior proof held), so return its **current** fence without
/// bumping the generation — the in-flight proof never lost the fence. `None` if not `Reclaiming`.
pub(crate) fn resume_reclaim(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
) -> Option<ReclaimTicket> {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
        let record = table.get(&key)?;
        if record.state != ReservationState::Reclaiming {
            return None;
        }
        Some(ReclaimTicket {
            claim: record.claim,
            generation: record.reclaim_generation,
            proof_scope: record.proof_scope,
        })
    })
}

/// Read-only check that the entry is `Reclaiming`, owned by `claim_id`, at `reclaim_generation == g`
/// — exactly the fence [`cancel_reclaim`] enforces (ADR 0030 slice 6). The atomic cancel preflights
/// this **before** any record mutation, so a later `cancel_reclaim` in the same no-`await` region is
/// guaranteed to apply: terminal-failure is never recorded for a reservation that cannot be removed.
pub(crate) fn is_reclaiming_at(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    claim_id: ClaimId,
    g: u64,
) -> bool {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
        table.get(&key).is_some_and(|record| {
            record.claim == claim_id
                && record.state == ReservationState::Reclaiming
                && record.reclaim_generation == g
        })
    })
}

/// Apply a reclaim proof's **commit** outcome under the fence (ADR 0030 §Timeout step 5): only if the
/// entry is still `Reclaiming` with `reclaim_generation == g` and owned by `claim_id`, transition to
/// `Committed` and stamp `owner_element_id`. Returns `true` iff applied (the caller may then ack the
/// `Acquire`). A diverged generation/state or foreign claim → `false` (discard this proof's result).
pub(crate) fn apply_reclaim_commit(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    claim_id: ClaimId,
    g: u64,
    owner_element_id: Vec<u8>,
    effect_id: EffectId,
) -> bool {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(mut record) = table.get(&key) else {
            return false;
        };
        if record.claim != claim_id
            || record.state != ReservationState::Reclaiming
            || record.reclaim_generation != g
        {
            return false;
        }
        record.state = ReservationState::Committed;
        record.owner_element_id = Some(owner_element_id);
        // Same contract as normal Confirm: the proven `Acquire` is still pinned until the reconciler
        // acks it, so record it as pending in the same write that commits.
        record.pending_acquire_ack = Some(effect_id);
        table.insert(key, record);
        true
    })
}

/// Apply a reclaim proof's **cancel** outcome under the fence (ADR 0030 §Timeout step 6): only if the
/// entry is still `Reclaiming` with `reclaim_generation == g`, remove the reservation, freeing the
/// value. Returns `true` iff removed. This enforces the proof's full fence — `Reclaiming`, owned by
/// `claim_id`, at `reclaim_generation == g` — but **not** the safety predicate; the caller must
/// already have established the latter (the owning mutation is terminally `Failed` **and** every
/// `proof_scope` shard was reachable and reported the `Acquire` absent).
///
/// The `claim_id` match is load-bearing for ABA safety: `reclaim_generation` resets to 0 on a fresh
/// `try_reserve`, so a *new* reservation B at the same key can re-reach the same generation `g` a
/// deleted reservation A's in-flight proof captured. Without the claim check, A's delayed Cancel
/// callback (`A.claim`, `g`) would match and wrongly remove B; the differing `ClaimId` rejects it.
pub(crate) fn cancel_reclaim(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    claim_id: ClaimId,
    g: u64,
) -> bool {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(record) = table.get(&key) else {
            return false;
        };
        if record.claim == claim_id
            && record.state == ReservationState::Reclaiming
            && record.reclaim_generation == g
        {
            table.remove(&key);
            true
        } else {
            false
        }
    })
}

/// Release the reclaim fence without resolving (ADR 0030 §Timeout step 7): an unreachable/unknown
/// shard, or a non-terminal/missing owning mutation, means the proof cannot safely commit or cancel.
/// Revert `Reclaiming@g → Reserved` **keeping** `reclaim_generation = g` (never reset), so the next
/// proof runs under a strictly higher generation. Fenced on `claim_id` + `Reclaiming@g` for the same
/// ABA reason as [`cancel_reclaim`] — a stale callback from a deleted-then-recreated key must not
/// revert the live reservation. No-op (`false`) unless the full fence matches.
pub(crate) fn hold_reclaim(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    claim_id: ClaimId,
    g: u64,
) -> bool {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(mut record) = table.get(&key) else {
            return false;
        };
        if record.claim == claim_id
            && record.state == ReservationState::Reclaiming
            && record.reclaim_generation == g
        {
            record.state = ReservationState::Reserved;
            table.insert(key, record);
            true
        } else {
            false
        }
    })
}

/// A reservation the slice-6 reclaim reconciler (Driver 1) must act on, captured by the no-`await`
/// work-discovery scan so the async proof/ack phase has every field it needs without re-reading
/// (the record may change between scan and apply; each transition re-checks its own fence).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReclaimCandidate {
    pub key: UniqueReservationKey,
    pub claim: ClaimId,
    pub state: ReservationState,
    pub reclaim_generation: u64,
    pub proof_scope: Vec<ProofShard>,
    pub pending_acquire_ack: Option<EffectId>,
}

/// Bounded, cursor-based work discovery for the reclaim reconciler (ADR 0030 slice 6, Driver 1).
/// Scans up to `budget` reservations after `start_after` across the whole keyspace and returns those
/// needing reconciliation, plus the last key examined (the next cursor) and the count scanned.
///
/// A reservation needs reconciliation when it is:
/// - `Reserved` and at least [`UNIQUE_RESERVATION_TTL_NS`] old — eligible for a fresh reclaim proof
///   (the TTL gates *eligibility* only; the cancel authority is the generation-fenced proof);
/// - `Reclaiming` (any age) — a prior proof began (and may have been interrupted by a crash); it
///   must be resumed under its existing fence so the value is never left fenced forever;
/// - `Committed` with a `pending_acquire_ack` (any age) — the `→ Committed` write is durable but the
///   `Acquire` ack was never confirmed, so the still-pinned effect must be re-acked or its
///   ack-reply-lost cleared.
///
/// Read-only: it never mutates the table, so the cursor can advance even when a candidate is later
/// held; the next lap re-discovers anything still needing work.
pub(crate) fn scan_reclaim_candidates(
    start_after: Option<&UniqueReservationKey>,
    budget: usize,
    now: u64,
) -> (Vec<ReclaimCandidate>, Option<UniqueReservationKey>, u32) {
    let mut scanned: u32 = 0;
    let mut last_key: Option<UniqueReservationKey> = None;
    let mut candidates: Vec<ReclaimCandidate> = Vec::new();
    ROUTER_UNIQUE_RESERVATIONS.with_borrow(|table| {
        let lower = match start_after {
            Some(key) => Bound::Excluded(key.clone()),
            None => Bound::Unbounded,
        };
        for entry in table.range((lower, Bound::Unbounded)).take(budget) {
            let key = entry.key().clone();
            let record = entry.value();
            scanned += 1;
            let needs = match record.state {
                ReservationState::Reserved => {
                    now.saturating_sub(record.reserved_at_ns) >= UNIQUE_RESERVATION_TTL_NS
                }
                ReservationState::Reclaiming => true,
                ReservationState::Committed => record.pending_acquire_ack.is_some(),
            };
            if needs {
                candidates.push(ReclaimCandidate {
                    key: key.clone(),
                    claim: record.claim,
                    state: record.state,
                    reclaim_generation: record.reclaim_generation,
                    proof_scope: record.proof_scope.clone(),
                    pending_acquire_ack: record.pending_acquire_ack,
                });
            }
            last_key = Some(key);
        }
    });
    (candidates, last_key, scanned)
}

/// Outcome of reconciling one `Release` effect against the reservation table (ADR 0030 slice 5b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReleaseOutcome {
    /// The `Release` was durably applied (or was a proven no-op): the Router may **ack** the effect.
    /// Covers a removed `Committed` reservation whose owner matched, a value that no longer has a
    /// reservation, and a stale `Release` whose value a *different* element has since taken over.
    Applied,
    /// The `Release` must be **held (not acked)**: the value's reservation is still `Reserved`/
    /// `Reclaiming` or its owner is not yet determined, so acking now could let a pending `Acquire`
    /// re-create a `Committed` reservation for an already-deleted element (the Release-before-Acquire
    /// hazard). The slice-6 recovery reconciler retries it after the `Acquire` is reconciled.
    Held,
}

/// Release: remove the `Committed` reservation a deleted/removed element owned, matched by
/// `owner_element_id` (ADR 0030 slice 5b). Best-effort and idempotent like Confirm — the canonical
/// delete is already durable and cannot be rolled back, so this never traps and never errors:
/// - **missing record** → `Applied` (nothing reserved; the value is already free — re-ack-safe);
/// - **`Reserved`/`Reclaiming`**, or `Committed` with no stamped owner → `Held` (Release-before-
///   Acquire: the owner is undetermined; do not ack, reconcile the `Acquire` first);
/// - **`Committed`, owner == effect owner, `Acquire` still pending ack** → `Held` (removing the
///   reservation now would orphan the still-pinned `Acquire`, whose only re-discovery handle is this
///   record; ack the `Acquire` first, which clears `pending_acquire_ack`, then this Release applies);
/// - **`Committed`, owner == effect owner, `Acquire` acked** → remove the reservation, `Applied`;
/// - **`Committed`, owner != effect owner** → a different element took the value over; the `Release`
///   is stale → `Applied` (no-op ack), leaving the live reservation intact.
pub(crate) fn release_reservation(
    graph_id: GraphId,
    constraint_id: ConstraintNameId,
    encoded_value: &[u8],
    owner_element_id: &[u8],
) -> ReleaseOutcome {
    let key = UniqueReservationKey::new(graph_id, constraint_id, encoded_value.to_vec());
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let Some(record) = table.get(&key) else {
            return ReleaseOutcome::Applied;
        };
        match record.state {
            ReservationState::Reserved | ReservationState::Reclaiming => ReleaseOutcome::Held,
            ReservationState::Committed => match record.owner_element_id.as_deref() {
                None => ReleaseOutcome::Held,
                Some(owner) if owner == owner_element_id => {
                    if record.pending_acquire_ack.is_some() {
                        ReleaseOutcome::Held
                    } else {
                        table.remove(&key);
                        ReleaseOutcome::Applied
                    }
                }
                Some(_) => ReleaseOutcome::Applied,
            },
        }
    })
}

/// Exclusive upper bound of one graph's key range. `graph_id` is the most-significant key
/// component, so `[(graph_id, ..), (graph_id + 1, ..))` covers exactly that graph. At
/// `GraphId::MAX` there is no `graph_id + 1`; the bound must be `Unbounded` — a saturating `+1`
/// would collapse to `(MAX, ..)` and yield an empty range, silently skipping the max graph.
fn graph_range_upper(graph_id: GraphId) -> Bound<UniqueReservationKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(UniqueReservationKey::new(
            GraphId::from_raw(next),
            ConstraintNameId::from_raw(0),
            Vec::new(),
        )),
        None => Bound::Unbounded,
    }
}

/// Removes every reservation for a graph (graph teardown). Mirrors the constraint catalog purge.
pub(crate) fn purge_graph_reservations(graph_id: GraphId) {
    ROUTER_UNIQUE_RESERVATIONS.with_borrow_mut(|table| {
        let start = UniqueReservationKey::new(graph_id, ConstraintNameId::from_raw(0), Vec::new());
        let keys: Vec<_> = table
            .range((Bound::Included(start), graph_range_upper(graph_id)))
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
            table
                .range((Bound::Included(start), graph_range_upper(graph_id)))
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
            owner_element_id: Some(vec![9u8; 8]),
            reserved_at_ns: 123,
            proof_scope: vec![
                ProofShard::new(ShardId::new(1), Principal::anonymous()),
                ProofShard::new(ShardId::new(2), Principal::management_canister()),
            ],
            pending_acquire_ack: Some(EffectId::new(42, 3)),
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
    fn purge_and_count_cover_the_max_graph_id() {
        // Regression: a saturating `graph_id + 1` upper bound collapses to an empty range at
        // GraphId::MAX, so the count/purge range scans would skip the max graph (leaking
        // reservations on teardown). The `Unbounded` upper bound covers it.
        let g = GraphId::from_raw(u32::MAX);
        try_reserve(g, 1, &[claim(1, b"x", 0)], &scope(1), 1).expect("reserve on max graph");
        assert_eq!(
            reservation_count(g),
            1,
            "reservation on the max graph must be counted, not skipped"
        );
        purge_graph_reservations(g);
        assert_eq!(reservation_count(g), 0);
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

    /// The constraint id used by the `claim(1, …)` helper, for `confirm_reservation` calls.
    fn cid() -> ConstraintNameId {
        ConstraintNameId::from_raw(1)
    }

    /// A representative `Acquire` effect id stamped at `→ Committed`.
    fn eid() -> EffectId {
        EffectId::new(100, 0)
    }

    #[test]
    fn confirm_transitions_reserved_to_committed_and_stamps_owner() {
        let g = fresh_graph(20);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");

        let owner = vec![7u8; 8];
        let committed =
            confirm_reservation(g, ClaimId::new(100, 0), cid(), b"v", owner.clone(), eid());
        assert_eq!(
            committed,
            ConfirmOutcome::FreshlyCommitted,
            "matching Reserved claim must freshly commit"
        );

        let rec = lookup(g, 1, b"v").expect("record");
        assert_eq!(rec.state, ReservationState::Committed);
        assert_eq!(rec.owner_element_id, Some(owner));
        assert_eq!(
            rec.pending_acquire_ack,
            Some(eid()),
            "Confirm stamps the Acquire as pending-ack until it is unpinned"
        );
    }

    #[test]
    fn confirm_on_missing_record_is_a_noop_false() {
        let g = fresh_graph(21);
        // No reservation exists for this value.
        let confirmed = confirm_reservation(
            g,
            ClaimId::new(100, 0),
            cid(),
            b"absent",
            vec![1u8; 8],
            eid(),
        );
        assert_eq!(confirmed, ConfirmOutcome::NotApplicable);
        assert_eq!(reservation_count(g), 0);
    }

    #[test]
    fn confirm_with_mismatched_claim_is_a_noop_false() {
        let g = fresh_graph(22);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");

        // A different mutation's claim id must not commit this reservation.
        let confirmed =
            confirm_reservation(g, ClaimId::new(999, 0), cid(), b"v", vec![1u8; 8], eid());
        assert_eq!(
            confirmed,
            ConfirmOutcome::NotApplicable,
            "foreign claim must not confirm"
        );
        let rec = lookup(g, 1, b"v").expect("record");
        assert_eq!(rec.state, ReservationState::Reserved, "left untouched");
        assert_eq!(rec.owner_element_id, None);
    }

    #[test]
    fn confirm_on_reclaiming_is_fenced_false() {
        let g = fresh_graph(23);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Reclaiming);

        // A generation-fenced reclaim proof is authoritative; Confirm must not override it.
        let confirmed =
            confirm_reservation(g, ClaimId::new(100, 0), cid(), b"v", vec![1u8; 8], eid());
        assert_eq!(
            confirmed,
            ConfirmOutcome::NotApplicable,
            "Reclaiming must fence Confirm"
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().state,
            ReservationState::Reclaiming
        );
    }

    #[test]
    fn confirm_is_idempotent_and_does_not_rewrite_owner() {
        let g = fresh_graph(24);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let first_owner = vec![7u8; 8];
        assert_eq!(
            confirm_reservation(
                g,
                ClaimId::new(100, 0),
                cid(),
                b"v",
                first_owner.clone(),
                eid()
            ),
            ConfirmOutcome::FreshlyCommitted
        );

        // A replayed Confirm (e.g. different evidence bytes) is idempotent: AlreadyCommitted (still
        // ack-able so a previously failed ack retries), owner kept, count not decremented again.
        let confirmed =
            confirm_reservation(g, ClaimId::new(100, 0), cid(), b"v", vec![9u8; 8], eid());
        assert_eq!(
            confirmed,
            ConfirmOutcome::AlreadyCommitted,
            "replay of an already-committed claim is idempotent"
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().owner_element_id,
            Some(first_owner),
            "owner is not rewritten on idempotent replay"
        );
    }

    /// Reserve then Confirm a value to a known owner, and clear the Acquire-ack pending flag to model
    /// the steady state a `Release` reconciles against (the Acquire was already unpinned).
    fn reserve_and_commit(g: GraphId, value: &[u8], owner: &[u8]) {
        try_reserve(g, 100, &[claim(1, value, 0)], &scope(1), 1).expect("reserve");
        assert_eq!(
            confirm_reservation(g, ClaimId::new(100, 0), cid(), value, owner.to_vec(), eid()),
            ConfirmOutcome::FreshlyCommitted
        );
        assert!(clear_acquire_ack(g, cid(), value, ClaimId::new(100, 0)));
    }

    #[test]
    fn release_removes_committed_reservation_when_owner_matches() {
        let g = fresh_graph(30);
        let owner = vec![7u8; 8];
        reserve_and_commit(g, b"v", &owner);

        assert_eq!(
            release_reservation(g, cid(), b"v", &owner),
            ReleaseOutcome::Applied
        );
        assert!(
            lookup(g, 1, b"v").is_none(),
            "owner-matched release must remove the reservation"
        );
    }

    #[test]
    fn release_with_different_owner_is_stale_noop_ack_and_keeps_reservation() {
        // The value was taken over by a different element; the old element's Release must not remove
        // the live reservation, but it is stale and safely ack-able.
        let g = fresh_graph(31);
        let live_owner = vec![9u8; 8];
        reserve_and_commit(g, b"v", &live_owner);

        let stale_owner = vec![1u8; 8];
        assert_eq!(
            release_reservation(g, cid(), b"v", &stale_owner),
            ReleaseOutcome::Applied
        );
        let rec = lookup(g, 1, b"v").expect("reservation kept");
        assert_eq!(rec.owner_element_id, Some(live_owner), "live owner intact");
    }

    #[test]
    fn release_on_missing_reservation_is_applied() {
        // Nothing reserved (already released or never claimed) → the value is free; re-ack-safe.
        let g = fresh_graph(32);
        assert_eq!(
            release_reservation(g, cid(), b"absent", &[1u8; 8]),
            ReleaseOutcome::Applied
        );
    }

    #[test]
    fn release_is_held_while_reservation_is_reserved() {
        // Release-before-Acquire: the value's Acquire is not yet Confirmed (owner undetermined), so
        // the Release must be held — acking now could let the pending Acquire re-create the
        // reservation for an already-deleted element.
        let g = fresh_graph(33);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");

        assert_eq!(
            release_reservation(g, cid(), b"v", &[7u8; 8]),
            ReleaseOutcome::Held
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().state,
            ReservationState::Reserved,
            "held release must not touch the reservation"
        );
    }

    #[test]
    fn release_is_held_while_reservation_is_reclaiming() {
        let g = fresh_graph(34);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Reclaiming);

        assert_eq!(
            release_reservation(g, cid(), b"v", &[7u8; 8]),
            ReleaseOutcome::Held
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().state,
            ReservationState::Reclaiming
        );
    }

    #[test]
    fn begin_reclaim_fences_reserved_and_bumps_generation() {
        let g = fresh_graph(40);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");

        let ticket = begin_reclaim(g, cid(), b"v").expect("reserved is reclaimable");
        assert_eq!(ticket.claim, ClaimId::new(100, 0));
        assert_eq!(ticket.generation, 1, "generation 0 -> 1");
        assert_eq!(ticket.proof_scope, scope(1));
        let rec = lookup(g, 1, b"v").expect("record");
        assert_eq!(rec.state, ReservationState::Reclaiming);
        assert_eq!(rec.reclaim_generation, 1);
    }

    #[test]
    fn begin_reclaim_aborts_when_not_reserved() {
        let g = fresh_graph(41);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Committed);
        assert_eq!(
            begin_reclaim(g, cid(), b"v"),
            None,
            "Committed not reclaimable"
        );
        assert_eq!(
            begin_reclaim(g, cid(), b"absent"),
            None,
            "missing not reclaimable"
        );
    }

    #[test]
    fn resume_reclaim_returns_current_fence_without_bumping() {
        let g = fresh_graph(42);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let first = begin_reclaim(g, cid(), b"v").expect("begin");
        // Crash/restart: the entry is still Reclaiming@1; resume must reuse generation 1.
        let resumed = resume_reclaim(g, cid(), b"v").expect("resume");
        assert_eq!(resumed.generation, first.generation);
        assert_eq!(resumed.claim, first.claim);
        assert_eq!(
            lookup(g, 1, b"v").unwrap().reclaim_generation,
            1,
            "resume must not bump the generation"
        );
        assert_eq!(resume_reclaim(g, cid(), b"absent"), None);
    }

    #[test]
    fn apply_reclaim_commit_under_fence_transitions_and_stamps_owner() {
        let g = fresh_graph(43);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let t = begin_reclaim(g, cid(), b"v").expect("begin");
        let owner = vec![7u8; 8];
        assert!(apply_reclaim_commit(
            g,
            cid(),
            b"v",
            t.claim,
            t.generation,
            owner.clone(),
            eid()
        ));
        let rec = lookup(g, 1, b"v").expect("record");
        assert_eq!(rec.state, ReservationState::Committed);
        assert_eq!(rec.owner_element_id, Some(owner));
        assert_eq!(
            rec.pending_acquire_ack,
            Some(eid()),
            "reclaim commit stamps the Acquire as pending-ack, same as normal Confirm"
        );
    }

    #[test]
    fn apply_reclaim_commit_discarded_when_generation_advanced() {
        // ABA fence: a stale proof from generation g must not apply after a newer proof bumped it.
        let g = fresh_graph(44);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let stale = begin_reclaim(g, cid(), b"v").expect("begin g=1");
        // Hold reverts to Reserved keeping g=1; a fresh begin bumps to g=2.
        assert!(hold_reclaim(g, cid(), b"v", stale.claim, stale.generation));
        let fresh = begin_reclaim(g, cid(), b"v").expect("begin g=2");
        assert_eq!(fresh.generation, 2);

        assert!(
            !apply_reclaim_commit(
                g,
                cid(),
                b"v",
                stale.claim,
                stale.generation,
                vec![1u8; 8],
                eid()
            ),
            "stale generation must be discarded"
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().state,
            ReservationState::Reclaiming,
            "still under the fresh proof"
        );
    }

    #[test]
    fn cancel_reclaim_removes_only_under_matching_fence() {
        let g = fresh_graph(45);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let t = begin_reclaim(g, cid(), b"v").expect("begin");
        // Wrong generation does not cancel.
        assert!(!cancel_reclaim(g, cid(), b"v", t.claim, t.generation + 1));
        assert!(lookup(g, 1, b"v").is_some());
        // Matching generation removes the reservation (value freed).
        assert!(cancel_reclaim(g, cid(), b"v", t.claim, t.generation));
        assert!(lookup(g, 1, b"v").is_none());
    }

    #[test]
    fn hold_reclaim_reverts_to_reserved_keeping_generation() {
        let g = fresh_graph(46);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        let t = begin_reclaim(g, cid(), b"v").expect("begin");
        assert!(hold_reclaim(g, cid(), b"v", t.claim, t.generation));
        let rec = lookup(g, 1, b"v").expect("record");
        assert_eq!(
            rec.state,
            ReservationState::Reserved,
            "reverted to Reserved"
        );
        assert_eq!(
            rec.reclaim_generation, t.generation,
            "generation retained, never reset (ABA-safe)"
        );
        // A subsequent begin bumps strictly higher.
        assert_eq!(begin_reclaim(g, cid(), b"v").unwrap().generation, 2);
    }

    #[test]
    fn cancel_reclaim_rejects_stale_claim_after_delete_and_re_reserve() {
        // ABA across reservation identities: A is reclaimed/cancelled, then B reserves the *same*
        // value and reaches the *same* generation (reset to 0 on re-reserve). A's delayed Cancel
        // callback (A.claim, g) must not remove B; only the matching claim+generation may.
        let g = fresh_graph(47);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve A");
        let a = begin_reclaim(g, cid(), b"v").expect("begin A");
        assert!(
            cancel_reclaim(g, cid(), b"v", a.claim, a.generation),
            "A removed"
        );

        // B re-reserves the freed value; a fresh begin reaches the same generation as A's proof.
        try_reserve(g, 200, &[claim(1, b"v", 0)], &scope(1), 2).expect("reserve B");
        let b = begin_reclaim(g, cid(), b"v").expect("begin B");
        assert_eq!(
            a.generation, b.generation,
            "same generation, different claim"
        );
        assert_ne!(a.claim, b.claim);

        assert!(
            !cancel_reclaim(g, cid(), b"v", a.claim, a.generation),
            "A's stale callback must not cancel B under the matching generation"
        );
        assert!(lookup(g, 1, b"v").is_some(), "B intact");
        assert!(
            cancel_reclaim(g, cid(), b"v", b.claim, b.generation),
            "B's own fence cancels B"
        );
    }

    #[test]
    fn hold_reclaim_rejects_stale_claim_after_delete_and_re_reserve() {
        let g = fresh_graph(48);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve A");
        let a = begin_reclaim(g, cid(), b"v").expect("begin A");
        assert!(
            cancel_reclaim(g, cid(), b"v", a.claim, a.generation),
            "A removed"
        );

        try_reserve(g, 200, &[claim(1, b"v", 0)], &scope(1), 2).expect("reserve B");
        let b = begin_reclaim(g, cid(), b"v").expect("begin B");
        assert_eq!(a.generation, b.generation);

        assert!(
            !hold_reclaim(g, cid(), b"v", a.claim, a.generation),
            "A's stale callback must not revert B"
        );
        assert_eq!(
            lookup(g, 1, b"v").unwrap().state,
            ReservationState::Reclaiming,
            "B stays under its own proof"
        );
        assert!(
            hold_reclaim(g, cid(), b"v", b.claim, b.generation),
            "B's own fence holds B"
        );
    }

    #[test]
    fn clear_acquire_ack_clears_only_matching_committed_claim() {
        let g = fresh_graph(49);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        assert_eq!(
            confirm_reservation(g, ClaimId::new(100, 0), cid(), b"v", vec![7u8; 8], eid()),
            ConfirmOutcome::FreshlyCommitted
        );
        assert_eq!(lookup(g, 1, b"v").unwrap().pending_acquire_ack, Some(eid()));

        // A foreign claim must not clear a live record's pending ack.
        assert!(!clear_acquire_ack(g, cid(), b"v", ClaimId::new(999, 0)));
        assert_eq!(lookup(g, 1, b"v").unwrap().pending_acquire_ack, Some(eid()));

        // The owning claim clears it; a second clear is an idempotent no-op (already cleared).
        assert!(clear_acquire_ack(g, cid(), b"v", ClaimId::new(100, 0)));
        assert_eq!(lookup(g, 1, b"v").unwrap().pending_acquire_ack, None);
        assert!(!clear_acquire_ack(g, cid(), b"v", ClaimId::new(100, 0)));
    }

    #[test]
    fn release_is_held_while_acquire_ack_pending_then_applies_after_clear() {
        // The owner matches, but the Acquire is still pinned (pending ack). Removing the reservation
        // now would orphan that Acquire (its only re-discovery handle is this record), so the Release
        // is held until the Acquire is acked (which clears the pending flag), then it applies.
        let g = fresh_graph(50);
        let owner = vec![7u8; 8];
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        assert_eq!(
            confirm_reservation(g, ClaimId::new(100, 0), cid(), b"v", owner.clone(), eid()),
            ConfirmOutcome::FreshlyCommitted
        );

        assert_eq!(
            release_reservation(g, cid(), b"v", &owner),
            ReleaseOutcome::Held,
            "owner-matched Release is held while the Acquire is still pending ack"
        );
        assert!(lookup(g, 1, b"v").is_some(), "reservation kept while held");

        assert!(clear_acquire_ack(g, cid(), b"v", ClaimId::new(100, 0)));
        assert_eq!(
            release_reservation(g, cid(), b"v", &owner),
            ReleaseOutcome::Applied,
            "after the Acquire is acked, the Release applies and removes the reservation"
        );
        assert!(lookup(g, 1, b"v").is_none());
    }

    #[test]
    fn release_is_held_when_committed_owner_is_undetermined() {
        // Defensive: a Committed record without a stamped owner cannot be matched, so the Release is
        // held rather than removing a reservation whose owner is unknown.
        let g = fresh_graph(35);
        try_reserve(g, 100, &[claim(1, b"v", 0)], &scope(1), 1).expect("reserve");
        force_state(g, b"v", ReservationState::Committed);

        assert_eq!(
            release_reservation(g, cid(), b"v", &[7u8; 8]),
            ReleaseOutcome::Held
        );
        assert!(
            lookup(g, 1, b"v").is_some(),
            "kept while owner undetermined"
        );
    }
}
