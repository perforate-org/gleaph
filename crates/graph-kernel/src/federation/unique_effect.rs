//! Graph-shard unique-effect receipts — durable evidence of cross-shard uniqueness commits
//! (ADR 0030 §"Canonical owner and staged state").
//!
//! A unique-affecting canonical segment appends one [`UniqueEffectReceipt`] **per individual
//! effect** to the graph shard's pinned unique-effect outbox. The outbox is identified and acked at
//! **effect** granularity (not claim granularity), because one segment can emit several effects
//! (an update emits `Acquire(new)` *and* `Release(old)`; a `DELETE` emits one `Release` per owned
//! constraint). Each effect is pinned until the Router acks its [`EffectId`], so for any
//! not-yet-terminal reservation a committed claim's `Acquire` is *guaranteed present* — effect
//! **absence is authoritative proof of non-commit**, decoupled from the 9-day-evicting graph
//! mutation journal ([ADR 0027](0027-graph-mutation-journal-retention.md)).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::entry::ConstraintNameId;
use crate::federation::ClaimId;
use crate::plan_exec::MutationId;

/// Immutable identity of one effect within a mutation. `effect_ordinal` is deterministic across
/// replays, so a re-executed segment re-emits the same `EffectId` (idempotent append).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct EffectId {
    pub mutation_id: MutationId,
    pub effect_ordinal: u32,
}

impl EffectId {
    pub const fn new(mutation_id: MutationId, effect_ordinal: u32) -> Self {
        Self {
            mutation_id,
            effect_ordinal,
        }
    }
}

/// Direction of a unique effect. `Acquire` claims a value for an element; `Release` frees the value
/// a (possibly different) mutation previously held.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum UniqueEffectOp {
    Acquire,
    Release,
}

/// Evidence that a claim's `Acquire` is pinned: the matching effect's `EffectId` (so the Router can
/// ack *that* effect after Confirm — `claim_ordinal` and `effect_ordinal` are distinct concepts and
/// neither derives the other) plus the canonical `owner_element_id`.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct UniqueAcquireEvidence {
    pub effect_id: EffectId,
    pub owner_element_id: Vec<u8>,
}

/// Replicated commit-proof answer for one claim (Router reclaim proof, ADR 0030 §Timeout).
/// `acquire` is `Some` iff a matching `Acquire` effect is pinned on this shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct UniqueAcquireProof {
    pub claim_id: ClaimId,
    pub acquire: Option<UniqueAcquireEvidence>,
}

/// One pinned unique-effect receipt.
///
/// Matching rules (ADR 0030): an `Acquire` is matched by **`claim_id`** (so an old acked-but-unpruned
/// `Acquire` for a prior claim on the same value is never mistaken for newer commit evidence); a
/// `Release` is matched by **`owner_element_id`** (the producing mutation differs from the original
/// `Acquire`, so its `claim_id` is audit-only / `None`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct UniqueEffectReceipt {
    pub effect_id: EffectId,
    /// `Some` for `Acquire` (the reserving claim); audit-only / `None` for `Release`.
    pub claim_id: Option<ClaimId>,
    /// Canonical element that owns the value: the exact encoded element-id bytes (a vertex id is 8
    /// bytes, an edge id 12). Stored variable-length rather than a fixed buffer so different element
    /// kinds never collide under an opaque padding convention; the Router treats it as an opaque tag
    /// (`Acquire` Confirm stamps it; `Release` matches on it).
    pub owner_element_id: Vec<u8>,
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
    pub op: UniqueEffectOp,
}
