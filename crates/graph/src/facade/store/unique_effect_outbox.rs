//! GraphStore access to the pinned unique-effect outbox (ADR 0030 slice 4).
//!
//! The emit path ([`emit_unique_effect`](GraphStore::emit_unique_effect)) is appended *inside* the
//! canonical write segment by the INSERT/DELETE write path (wired in slice 5). The read/ack paths
//! back the Router-facing endpoints: a replicated `Acquire` proof read and per-effect ack.

use super::super::stable::UNIQUE_EFFECT_OUTBOX;
use super::GraphStore;
use gleaph_graph_kernel::federation::{
    ClaimId, EffectId, UniqueAcquireEvidence, UniqueEffectReceipt,
};
use gleaph_graph_kernel::plan_exec::MutationId;

impl GraphStore {
    /// Appends (pins) one unique effect. Must run in the same atomic segment as the canonical write
    /// it evidences. Idempotent across deterministic replays (same `EffectId`).
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "appended from the canonical write segment in ADR 0030 slice 5"
        )
    )]
    pub(crate) fn emit_unique_effect(&self, receipt: UniqueEffectReceipt) {
        UNIQUE_EFFECT_OUTBOX.with_borrow_mut(|outbox| outbox.append(receipt));
    }

    /// Replicated commit proof for a claim: `Some({ effect_id, owner_element_id })` iff a matching
    /// `Acquire` effect is pinned, else `None` (authoritative non-commit while the reservation is
    /// non-terminal). The `effect_id` lets the Router ack that exact effect after Confirm.
    pub(crate) fn unique_acquire_evidence(
        &self,
        claim_id: ClaimId,
    ) -> Option<UniqueAcquireEvidence> {
        UNIQUE_EFFECT_OUTBOX.with_borrow(|outbox| outbox.acquire_evidence(claim_id))
    }

    /// All pinned effects of a mutation, in `effect_ordinal` order (recovery / audit).
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "consumed by the recovery reconciler in ADR 0030 slice 6"
        )
    )]
    pub(crate) fn unique_effects_for_mutation(
        &self,
        mutation_id: MutationId,
    ) -> Vec<UniqueEffectReceipt> {
        UNIQUE_EFFECT_OUTBOX.with_borrow(|outbox| outbox.effects_for_mutation(mutation_id))
    }

    /// Unpins (acks) effects by `EffectId`. The Router calls this only after durably applying each
    /// effect; acking one effect never unpins a sibling.
    pub(crate) fn ack_unique_effects(&self, effect_ids: impl IntoIterator<Item = EffectId>) {
        UNIQUE_EFFECT_OUTBOX.with_borrow_mut(|outbox| {
            for effect_id in effect_ids {
                outbox.ack(effect_id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::ConstraintNameId;
    use gleaph_graph_kernel::federation::UniqueEffectOp;

    fn acquire(
        mutation_id: u64,
        ordinal: u32,
        claim_ordinal: u32,
        owner: u8,
    ) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(mutation_id, ordinal),
            claim_id: Some(ClaimId::new(mutation_id, claim_ordinal)),
            owner_element_id: [owner; 16],
            constraint_id: ConstraintNameId::from_raw(1),
            encoded_value: vec![owner],
            op: UniqueEffectOp::Acquire,
        }
    }

    fn release(mutation_id: u64, ordinal: u32, owner: u8) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(mutation_id, ordinal),
            claim_id: None,
            owner_element_id: [owner; 16],
            constraint_id: ConstraintNameId::from_raw(1),
            encoded_value: vec![owner],
            op: UniqueEffectOp::Release,
        }
    }

    #[test]
    fn acquire_evidence_matches_by_claim_id_and_carries_effect_id() {
        let store = GraphStore::new();
        let m = 9_000_001;
        // Acquire's effect_ordinal (3) differs from its claim_ordinal (0): the Router must learn the
        // effect_id from the proof, it cannot derive it.
        store.emit_unique_effect(acquire(m, 3, 0, 7));
        assert_eq!(
            store.unique_acquire_evidence(ClaimId::new(m, 0)),
            Some(UniqueAcquireEvidence {
                effect_id: EffectId::new(m, 3),
                owner_element_id: [7u8; 16],
            })
        );
        // A different claim_ordinal of the same mutation does not match.
        assert_eq!(store.unique_acquire_evidence(ClaimId::new(m, 1)), None);
    }

    #[test]
    fn absent_acquire_is_none() {
        let store = GraphStore::new();
        assert_eq!(
            store.unique_acquire_evidence(ClaimId::new(9_000_002, 0)),
            None
        );
    }

    #[test]
    fn acquire_evidence_for_max_mutation_id_is_found() {
        // u64::MAX is a valid Router-allocated mutation id; the range upper bound must include it.
        let store = GraphStore::new();
        let m = u64::MAX;
        store.emit_unique_effect(acquire(m, 0, 0, 4));
        assert_eq!(
            store.unique_acquire_evidence(ClaimId::new(m, 0)),
            Some(UniqueAcquireEvidence {
                effect_id: EffectId::new(m, 0),
                owner_element_id: [4u8; 16],
            })
        );
    }

    #[test]
    #[should_panic(expected = "different receipt")]
    fn appending_a_different_receipt_at_an_existing_effect_id_traps() {
        let store = GraphStore::new();
        let m = 9_000_006;
        store.emit_unique_effect(acquire(m, 0, 0, 1));
        // Same EffectId, but a Release — would destroy the Acquire commit evidence.
        store.emit_unique_effect(release(m, 0, 1));
    }

    #[test]
    #[should_panic(expected = "must carry a claim_id")]
    fn acquire_without_claim_id_traps() {
        let store = GraphStore::new();
        let mut bad = acquire(9_000_007, 0, 0, 1);
        bad.claim_id = None;
        store.emit_unique_effect(bad);
    }

    #[test]
    #[should_panic(expected = "!= effect mutation")]
    fn acquire_with_mismatched_mutation_id_traps() {
        let store = GraphStore::new();
        let mut bad = acquire(9_000_008, 0, 0, 1);
        bad.claim_id = Some(ClaimId::new(9_000_009, 0));
        store.emit_unique_effect(bad);
    }

    #[test]
    fn ack_unpins_only_the_named_effect() {
        let store = GraphStore::new();
        let m = 9_000_003;
        // One mutation emits Acquire(new) + Release(old) with distinct effect_ordinals.
        store.emit_unique_effect(acquire(m, 0, 0, 1));
        store.emit_unique_effect(release(m, 1, 2));
        assert_eq!(store.unique_effects_for_mutation(m).len(), 2);

        // Acking the Acquire leaves the Release pinned.
        store.ack_unique_effects([EffectId::new(m, 0)]);
        let remaining = store.unique_effects_for_mutation(m);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].op, UniqueEffectOp::Release);
        assert_eq!(store.unique_acquire_evidence(ClaimId::new(m, 0)), None);
    }

    #[test]
    fn append_is_idempotent_on_replay() {
        let store = GraphStore::new();
        let m = 9_000_004;
        store.emit_unique_effect(acquire(m, 0, 0, 5));
        store.emit_unique_effect(acquire(m, 0, 0, 5));
        assert_eq!(store.unique_effects_for_mutation(m).len(), 1);
    }

    #[test]
    fn effects_for_mutation_is_scoped_and_ordered() {
        let store = GraphStore::new();
        let m = 9_000_005;
        store.emit_unique_effect(release(m, 2, 3));
        store.emit_unique_effect(acquire(m, 0, 0, 1));
        // A neighbouring mutation's effect must not leak into the range.
        store.emit_unique_effect(acquire(m + 1, 0, 0, 9));

        let effects = store.unique_effects_for_mutation(m);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].effect_id.effect_ordinal, 0);
        assert_eq!(effects[1].effect_id.effect_ordinal, 2);
    }
}
