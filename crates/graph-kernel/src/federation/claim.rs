//! Immutable identity of a single uniqueness claim within a mutation (ADR 0030).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::plan_exec::MutationId;

/// Immutable identity of one uniqueness claim produced by a mutation.
///
/// `ClaimId` carries **no element id**: the identity computed at Try (before the canonical element
/// exists) is therefore byte-identical to the one stamped on the graph shard's `Acquire` receipt,
/// so Try-time and commit-time identities always match (ADR 0030 §"Canonical owner and staged
/// state"). `owner_element_id` is tracked separately on the reservation, not folded into identity.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct ClaimId {
    pub mutation_id: MutationId,
    /// Deterministic position of the claim within its mutation. First cut: the qualifying
    /// `INSERT`'s plan position — exactly one constrained element per INSERT (ADR 0030 Scope). The
    /// general `(plan_position, input_row_ordinal)` form belongs to the deferred multi-row protocol.
    pub claim_ordinal: u32,
}

impl ClaimId {
    pub const fn new(mutation_id: MutationId, claim_ordinal: u32) -> Self {
        Self {
            mutation_id,
            claim_ordinal,
        }
    }
}
