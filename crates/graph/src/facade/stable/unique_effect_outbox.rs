//! Durable, pinned unique-effect outbox on the graph shard (ADR 0030 §"Canonical owner and staged
//! state").
//!
//! Each unique-affecting canonical segment appends one [`UniqueEffectReceipt`] per effect, keyed by
//! its [`EffectId`]. An effect stays **pinned** (present) until the Router acks its `EffectId`, and
//! the Router acks only after it has durably applied the effect (advanced/removed the reservation).
//! Because un-acked effects are never evicted, a committed claim's `Acquire` is always present while
//! its reservation is non-terminal, so the Router's reclaim proof can treat **absence as
//! authoritative proof of non-commit** — independent of the 9-day graph mutation journal retention
//! (ADR 0027).
//!
//! Append is **idempotent**: `effect_ordinal` is deterministic across replays, so re-executing a
//! segment re-inserts an identical receipt under the same key.

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::{
    ClaimId, EffectId, UniqueAcquireEvidence, UniqueEffectOp, UniqueEffectReceipt,
};
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::ops::Bound as RangeBound;

/// `EffectId` fixed-width stable key: `mutation_id` (8) + `effect_ordinal` (4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EffectKey(pub EffectId);

impl Storable for EffectKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 12];
        out[0..8].copy_from_slice(&self.0.mutation_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.0.effect_ordinal.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mutation_id = MutationId::from_le_bytes(bytes[0..8].try_into().expect("mutation_id"));
        let effect_ordinal = u32::from_le_bytes(bytes[8..12].try_into().expect("effect_ordinal"));
        Self(EffectId::new(mutation_id, effect_ordinal))
    }
}

/// Storable wrapper for the receipt value (candid, versioned envelope).
#[derive(Clone, Debug, candid::CandidType, serde::Serialize, serde::Deserialize)]
enum UniqueEffectStableRecord {
    V1(UniqueEffectReceipt),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxValue(pub UniqueEffectReceipt);

impl Storable for OutboxValue {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&UniqueEffectStableRecord::V1(self.0.clone())).expect("encode unique effect"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&UniqueEffectStableRecord::V1(self.0)).expect("encode unique effect")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), UniqueEffectStableRecord).expect("decode unique effect") {
            UniqueEffectStableRecord::V1(v1) => Self(v1),
        }
    }
}

/// The pinned outbox: `EffectId → UniqueEffectReceipt`. Ordered by `(mutation_id, effect_ordinal)`,
/// so all effects of one mutation form a contiguous range.
pub struct UniqueEffectOutbox<M: Memory> {
    map: StableBTreeMap<EffectKey, OutboxValue, M>,
}

impl<M: Memory> UniqueEffectOutbox<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    /// Appends (pins) one effect, validating the receipt at the write boundary.
    ///
    /// Idempotent under **deterministic** replay: re-appending a byte-identical receipt under the
    /// same `EffectId` is a no-op. A *different* receipt at an existing `EffectId` (non-deterministic
    /// replay or an `effect_ordinal` collision) would silently destroy commit evidence — e.g. an
    /// `Acquire` overwritten by a `Release` — so it **traps** instead. Shape invariants: an `Acquire`
    /// must carry `claim_id = Some` whose `mutation_id` equals the effect's, so `Acquire`-by-`ClaimId`
    /// matching is well-defined.
    pub fn append(&mut self, receipt: UniqueEffectReceipt) {
        if receipt.op == UniqueEffectOp::Acquire {
            match receipt.claim_id {
                Some(claim_id) if claim_id.mutation_id == receipt.effect_id.mutation_id => {}
                Some(claim_id) => panic!(
                    "unique-effect Acquire {:?} claim_id mutation {} != effect mutation {}",
                    receipt.effect_id, claim_id.mutation_id, receipt.effect_id.mutation_id
                ),
                None => panic!(
                    "unique-effect Acquire {:?} must carry a claim_id",
                    receipt.effect_id
                ),
            }
        }
        let key = EffectKey(receipt.effect_id);
        if let Some(existing) = self.map.get(&key) {
            assert!(
                existing.0 == receipt,
                "unique-effect {:?} re-appended with a different receipt (non-deterministic replay \
                 or effect_ordinal collision); refusing to overwrite commit evidence",
                receipt.effect_id
            );
            return;
        }
        self.map.insert(key, OutboxValue(receipt));
    }

    pub fn get(&self, effect_id: EffectId) -> Option<UniqueEffectReceipt> {
        self.map.get(&EffectKey(effect_id)).map(|v| v.0)
    }

    /// All pinned effects of one mutation, in `effect_ordinal` order.
    pub fn effects_for_mutation(&self, mutation_id: MutationId) -> Vec<UniqueEffectReceipt> {
        let lo = EffectKey(EffectId::new(mutation_id, 0));
        // `mutation_id == u64::MAX` is a valid Router-allocated id; `mutation_id + 1` would wrap to
        // the same key and yield an empty range (an Acquire would read as absent → unsafe Cancel),
        // so close the top bound inclusively at the max effect_ordinal.
        let upper = if mutation_id == u64::MAX {
            RangeBound::Included(EffectKey(EffectId::new(u64::MAX, u32::MAX)))
        } else {
            RangeBound::Excluded(EffectKey(EffectId::new(mutation_id + 1, 0)))
        };
        self.map
            .range((RangeBound::Included(lo), upper))
            .map(|entry| entry.value().0)
            .collect()
    }

    /// One page of a mutation's pinned `Release` effects with `effect_ordinal > after_ordinal`, in
    /// ascending order, capped at `limit`. Backs the Router's paginated Release reconciliation
    /// (ADR 0030 slice 5b): an arbitrary-cardinality DELETE/REMOVE can free unbounded values, so
    /// the full set cannot be returned in one IC response. The cursor is the last `effect_ordinal`
    /// the Router has already observed (exclusive); `None` starts at the beginning. Held releases
    /// stay pinned but the cursor advances past them, so reconciliation terminates and recovery
    /// (slice 6) revisits the still-pinned ones.
    pub fn release_effects_page(
        &self,
        mutation_id: MutationId,
        after_ordinal: Option<u32>,
        limit: usize,
    ) -> Vec<UniqueEffectReceipt> {
        let lo = match after_ordinal {
            Some(cursor) => match cursor.checked_add(1) {
                Some(next) => RangeBound::Included(EffectKey(EffectId::new(mutation_id, next))),
                // cursor == u32::MAX: no effect ordinal can exceed it.
                None => return Vec::new(),
            },
            None => RangeBound::Included(EffectKey(EffectId::new(mutation_id, 0))),
        };
        let upper = if mutation_id == u64::MAX {
            RangeBound::Included(EffectKey(EffectId::new(u64::MAX, u32::MAX)))
        } else {
            RangeBound::Excluded(EffectKey(EffectId::new(mutation_id + 1, 0)))
        };
        self.map
            .range((lo, upper))
            .map(|entry| entry.value().0)
            .filter(|r| r.op == UniqueEffectOp::Release)
            .take(limit)
            .collect()
    }

    /// Replicated commit proof: the `EffectId` + `owner_element_id` of the `Acquire` effect matching
    /// `claim_id`, or `None` if no such `Acquire` is pinned. `Acquire` is matched by **`ClaimId`**
    /// (ADR 0030), so an unrelated effect on the same value is never mistaken for this claim's
    /// evidence. The `EffectId` is required so the Router can ack that exact effect after Confirm.
    pub fn acquire_evidence(&self, claim_id: ClaimId) -> Option<UniqueAcquireEvidence> {
        self.effects_for_mutation(claim_id.mutation_id)
            .into_iter()
            .find(|r| r.op == UniqueEffectOp::Acquire && r.claim_id == Some(claim_id))
            .map(|r| UniqueAcquireEvidence {
                effect_id: r.effect_id,
                owner_element_id: r.owner_element_id,
            })
    }

    /// Unpins (acks) one effect by its `EffectId`. No-op if already pruned.
    pub fn ack(&mut self, effect_id: EffectId) {
        self.map.remove(&EffectKey(effect_id));
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }
}
