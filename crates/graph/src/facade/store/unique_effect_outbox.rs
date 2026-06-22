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

    /// One page of a mutation's pinned `Release` effects (ADR 0030 slice 5b), in `effect_ordinal`
    /// order with ordinal `> after_ordinal`, capped at `limit`. Backs the Router-facing paginated
    /// read so an arbitrary-cardinality DELETE/REMOVE cannot overflow the IC response; the Router
    /// reconciles each page by `owner_element_id`, acks the applied ones, and advances the cursor.
    pub(crate) fn unique_release_effects_page(
        &self,
        mutation_id: MutationId,
        after_ordinal: Option<u32>,
        limit: usize,
    ) -> Vec<UniqueEffectReceipt> {
        UNIQUE_EFFECT_OUTBOX
            .with_borrow(|outbox| outbox.release_effects_page(mutation_id, after_ordinal, limit))
    }

    /// One page of **all** of a mutation's pinned effects (`Acquire` and `Release`) in
    /// `effect_ordinal` order with ordinal `> after_ordinal`, capped at `limit`. Backs the Router's
    /// unified slice-6 effect recovery (Driver 2): unlike [`unique_release_effects_page`] it includes
    /// `Acquire`s, so recovery can discover an orphan `Acquire` (no reservation) as well as
    /// `Release`s. An empty page is the only end-of-stream signal.
    pub(crate) fn unique_effects_page(
        &self,
        mutation_id: MutationId,
        after_ordinal: Option<u32>,
        limit: usize,
    ) -> Vec<UniqueEffectReceipt> {
        UNIQUE_EFFECT_OUTBOX
            .with_borrow(|outbox| outbox.effects_page(mutation_id, after_ordinal, limit))
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

    /// Count of currently pinned (un-acked) unique effects (PocketIC E2E only).
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn e2e_unique_outbox_len(&self) -> u64 {
        UNIQUE_EFFECT_OUTBOX.with_borrow(|outbox| outbox.len())
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
            owner_element_id: vec![owner; 8],
            constraint_id: ConstraintNameId::from_raw(1),
            encoded_value: vec![owner],
            op: UniqueEffectOp::Acquire,
        }
    }

    fn release(mutation_id: u64, ordinal: u32, owner: u8) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(mutation_id, ordinal),
            claim_id: None,
            owner_element_id: vec![owner; 8],
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
                owner_element_id: vec![7u8; 8],
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
                owner_element_id: vec![4u8; 8],
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

    #[test]
    fn release_page_is_cursor_bounded_filtered_and_capped() {
        let store = GraphStore::new();
        let m = 9_000_010;
        // An Acquire occupies ordinal 0; releases occupy 1..=4.
        store.emit_unique_effect(acquire(m, 0, 0, 1));
        for ordinal in 1u32..=4 {
            store.emit_unique_effect(release(m, ordinal, ordinal as u8));
        }

        // First page: from the start, capped at 2, only Release effects (the Acquire is skipped).
        let page = store.unique_release_effects_page(m, None, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].effect_id.effect_ordinal, 1);
        assert_eq!(page[1].effect_id.effect_ordinal, 2);
        assert!(page.iter().all(|r| r.op == UniqueEffectOp::Release));

        // Next page strictly after the last observed ordinal.
        let page = store.unique_release_effects_page(m, Some(2), 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].effect_id.effect_ordinal, 3);
        assert_eq!(page[1].effect_id.effect_ordinal, 4);

        // Cursor past the end is empty (terminates the Router loop).
        assert!(store.unique_release_effects_page(m, Some(4), 2).is_empty());
    }

    #[test]
    fn effects_page_includes_both_ops_cursor_bounded_and_capped() {
        let store = GraphStore::new();
        let m = 9_000_011;
        // Interleave an Acquire (ordinal 0) with releases (1..=3): Driver 2 must see *all* of them.
        store.emit_unique_effect(acquire(m, 0, 0, 1));
        for ordinal in 1u32..=3 {
            store.emit_unique_effect(release(m, ordinal, ordinal as u8));
        }

        // First page from the start, capped at 2, keeps the Acquire (unlike the release-only page).
        let page = store.unique_effects_page(m, None, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].effect_id.effect_ordinal, 0);
        assert_eq!(page[0].op, UniqueEffectOp::Acquire);
        assert_eq!(page[1].effect_id.effect_ordinal, 1);
        assert_eq!(page[1].op, UniqueEffectOp::Release);

        // Next page strictly after the last observed ordinal.
        let page = store.unique_effects_page(m, Some(1), 5);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].effect_id.effect_ordinal, 2);
        assert_eq!(page[1].effect_id.effect_ordinal, 3);

        // Cursor past the end is empty (the only EOF signal).
        assert!(store.unique_effects_page(m, Some(3), 5).is_empty());
    }

    #[test]
    fn effects_page_at_max_mutation_id_is_found() {
        let store = GraphStore::new();
        let m = u64::MAX;
        store.emit_unique_effect(acquire(m, 0, 0, 4));
        let page = store.unique_effects_page(m, None, 8);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].effect_id.effect_ordinal, 0);
    }
}
