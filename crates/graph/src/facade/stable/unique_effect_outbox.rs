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

use gleaph_graph_kernel::entry::ConstraintNameId;
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

/// A receipt as stored, under its `EffectId` key: everything but the id.
///
/// ```text
/// layout         u8   the [`OutboxLayout`], 1; a reader rejects any other
/// flags          u8   bit 0: Release (else Acquire); bit 1: a claim; bit 2: the claim's
///                     mutation is not the effect's, and follows
/// constraint_id  u16  LE
/// claim mutation u64  LE, only with bit 2
/// claim ordinal  u32  LE, only with bit 1
/// owner length   u8, then the owner element id's bytes
/// encoded value  the rest
/// ```
///
/// An `Acquire`'s claim is always of its own mutation, so it takes its ordinal alone. Candid,
/// which this replaced, cost about 150K instructions to decode a receipt
/// (`unique_outbox_acquire_proof_read_256` decodes a mutation's every effect).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxValue {
    op: UniqueEffectOp,
    claim: Option<StoredClaim>,
    owner_element_id: Vec<u8>,
    constraint_id: ConstraintNameId,
    encoded_value: Vec<u8>,
}

/// A receipt's claim as stored: of the effect's own mutation, by ordinal, or of another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredClaim {
    Own(u32),
    Other(ClaimId),
}

/// The layouts a stored receipt's first byte names (ADR 0135): one current layout. A later one
/// adds a variant and its reader, which turns the older bytes into the current [`OutboxValue`];
/// writers write the current layout only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum OutboxLayout {
    V1 = 1,
}

impl OutboxLayout {
    const CURRENT: Self = Self::V1;
}

impl TryFrom<u8> for OutboxLayout {
    /// The byte, naming no layout.
    type Error = u8;

    fn try_from(byte: u8) -> Result<Self, u8> {
        match byte {
            1 => Ok(Self::V1),
            other => Err(other),
        }
    }
}

const RELEASE: u8 = 1;
const CLAIMED: u8 = 1 << 1;
const OTHER_MUTATION: u8 = 1 << 2;

impl OutboxValue {
    fn of(receipt: UniqueEffectReceipt) -> Self {
        let claim = receipt.claim_id.map(|claim| {
            if claim.mutation_id == receipt.effect_id.mutation_id {
                StoredClaim::Own(claim.claim_ordinal)
            } else {
                StoredClaim::Other(claim)
            }
        });
        Self {
            op: receipt.op,
            claim,
            owner_element_id: receipt.owner_element_id,
            constraint_id: receipt.constraint_id,
            encoded_value: receipt.encoded_value,
        }
    }

    /// The receipt stored under `effect_id`.
    fn receipt(self, effect_id: EffectId) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id,
            claim_id: self.claim.map(|claim| match claim {
                StoredClaim::Own(ordinal) => ClaimId::new(effect_id.mutation_id, ordinal),
                StoredClaim::Other(claim) => claim,
            }),
            owner_element_id: self.owner_element_id,
            constraint_id: self.constraint_id,
            encoded_value: self.encoded_value,
            op: self.op,
        }
    }

    /// Whether this is the `Acquire` of `claim`, an effect of the claim's own mutation.
    fn acquires(&self, claim: ClaimId) -> bool {
        self.op == UniqueEffectOp::Acquire
            && self.claim == Some(StoredClaim::Own(claim.claim_ordinal))
    }
}

impl Storable for OutboxValue {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(21 + self.owner_element_id.len() + self.encoded_value.len());
        out.push(OutboxLayout::CURRENT as u8);
        self.encode_v1(&mut out);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let (&layout, body) = bytes
            .split_first()
            .expect("unique effect truncated: missing layout version");
        match OutboxLayout::try_from(layout)
            .unwrap_or_else(|version| panic!("unknown unique effect layout version {version}"))
        {
            OutboxLayout::V1 => Self::decode_v1(body),
        }
    }
}

impl OutboxValue {
    /// Appends the [`OutboxLayout::V1`] body, as the type's documentation lays it out.
    fn encode_v1(&self, out: &mut Vec<u8>) {
        let owner_len =
            u8::try_from(self.owner_element_id.len()).expect("an owner element id fits 255 bytes");
        let mut flags = if self.op == UniqueEffectOp::Release {
            RELEASE
        } else {
            0
        };
        let flags_at = out.len();
        out.push(0);
        out.extend_from_slice(&self.constraint_id.raw().to_le_bytes());
        match self.claim {
            None => {}
            Some(StoredClaim::Own(ordinal)) => {
                flags |= CLAIMED;
                out.extend_from_slice(&ordinal.to_le_bytes());
            }
            Some(StoredClaim::Other(claim)) => {
                flags |= CLAIMED | OTHER_MUTATION;
                out.extend_from_slice(&claim.mutation_id.to_le_bytes());
                out.extend_from_slice(&claim.claim_ordinal.to_le_bytes());
            }
        }
        if let Some(byte) = out.get_mut(flags_at) {
            *byte = flags;
        }
        out.push(owner_len);
        out.extend_from_slice(&self.owner_element_id);
        out.extend_from_slice(&self.encoded_value);
    }

    /// Reads an [`OutboxLayout::V1`] body.
    fn decode_v1(body: &[u8]) -> Self {
        let mut rest = body;
        let mut take = |len: usize| -> &[u8] {
            let (head, tail) = rest.split_at_checked(len).expect("unique effect truncated");
            rest = tail;
            head
        };
        let flags = take(1)[0];
        let constraint_id =
            ConstraintNameId::from_raw(u16::from_le_bytes(take(2).try_into().expect("constraint")));
        let claim = (flags & CLAIMED != 0).then(|| {
            let mutation = (flags & OTHER_MUTATION != 0)
                .then(|| MutationId::from_le_bytes(take(8).try_into().expect("claim mutation")));
            let ordinal = u32::from_le_bytes(take(4).try_into().expect("claim ordinal"));
            match mutation {
                Some(mutation_id) => StoredClaim::Other(ClaimId::new(mutation_id, ordinal)),
                None => StoredClaim::Own(ordinal),
            }
        });
        let owner_len = usize::from(take(1)[0]);
        let owner_element_id = take(owner_len).to_vec();
        Self {
            op: if flags & RELEASE != 0 {
                UniqueEffectOp::Release
            } else {
                UniqueEffectOp::Acquire
            },
            claim,
            owner_element_id,
            constraint_id,
            encoded_value: rest.to_vec(),
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
                existing.receipt(receipt.effect_id) == receipt,
                "unique-effect {:?} re-appended with a different receipt (non-deterministic replay \
                 or effect_ordinal collision); refusing to overwrite commit evidence",
                receipt.effect_id
            );
            return;
        }
        self.map.insert(key, OutboxValue::of(receipt));
    }

    pub fn get(&self, effect_id: EffectId) -> Option<UniqueEffectReceipt> {
        self.map
            .get(&EffectKey(effect_id))
            .map(|value| value.receipt(effect_id))
    }

    /// All pinned effects of one mutation, in `effect_ordinal` order.
    pub fn effects_for_mutation(&self, mutation_id: MutationId) -> Vec<UniqueEffectReceipt> {
        let (lo, upper) = mutation_range(mutation_id);
        self.map
            .range((RangeBound::Included(lo), upper))
            .map(|entry| {
                let (EffectKey(effect_id), value) = entry.into_pair();
                value.receipt(effect_id)
            })
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
        let (_, upper) = mutation_range(mutation_id);
        self.map
            .range((lo, upper))
            .map(|entry| {
                let (EffectKey(effect_id), value) = entry.into_pair();
                value.receipt(effect_id)
            })
            .filter(|r| r.op == UniqueEffectOp::Release)
            .take(limit)
            .collect()
    }

    /// One page of **all** of a mutation's pinned effects (both `Acquire` and `Release`) with
    /// `effect_ordinal > after_ordinal`, in ascending order, capped at `limit`. Backs the Router's
    /// unified slice-6 effect recovery (Driver 2), which must discover every un-acked effect — an
    /// orphan `Acquire` no reservation can find as well as `Release`s. Same cursor contract as
    /// [`release_effects_page`]: the cursor is the last `effect_ordinal` already observed (exclusive),
    /// `None` starts at the beginning, and an empty page is the only end-of-stream signal.
    pub fn effects_page(
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
        let (_, upper) = mutation_range(mutation_id);
        self.map
            .range((lo, upper))
            .map(|entry| {
                let (EffectKey(effect_id), value) = entry.into_pair();
                value.receipt(effect_id)
            })
            .take(limit)
            .collect()
    }

    /// Replicated commit proof: the `EffectId` + `owner_element_id` of the `Acquire` effect matching
    /// `claim_id`, or `None` if no such `Acquire` is pinned. `Acquire` is matched by **`ClaimId`**
    /// (ADR 0030), so an unrelated effect on the same value is never mistaken for this claim's
    /// evidence. The `EffectId` is required so the Router can ack that exact effect after Confirm.
    pub fn acquire_evidence(&self, claim_id: ClaimId) -> Option<UniqueAcquireEvidence> {
        let (lo, upper) = mutation_range(claim_id.mutation_id);
        self.map
            .range((RangeBound::Included(lo), upper))
            .find_map(|entry| {
                let (EffectKey(effect_id), value) = entry.into_pair();
                let acquires = value.acquires(claim_id);
                acquires.then_some(UniqueAcquireEvidence {
                    effect_id,
                    owner_element_id: value.owner_element_id,
                })
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

/// The keys of `mutation_id`'s effects: from its first, and the bound past its last.
/// `mutation_id == u64::MAX` is a valid Router-allocated id; `mutation_id + 1` would wrap to the
/// same key and yield an empty range (an Acquire would read as absent → unsafe Cancel), so its top
/// bound closes inclusively at the max effect_ordinal.
fn mutation_range(mutation_id: MutationId) -> (EffectKey, RangeBound<EffectKey>) {
    let lo = EffectKey(EffectId::new(mutation_id, 0));
    let upper = if mutation_id == u64::MAX {
        RangeBound::Included(EffectKey(EffectId::new(
            MutationId::new(u64::MAX),
            u32::MAX,
        )))
    } else {
        RangeBound::Excluded(EffectKey(EffectId::new(mutation_id + 1, 0)))
    };
    (lo, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(
        op: UniqueEffectOp,
        claim_id: Option<ClaimId>,
        owner: &[u8],
        value: &[u8],
    ) -> UniqueEffectReceipt {
        UniqueEffectReceipt {
            effect_id: EffectId::new(MutationId::new(7), 3),
            claim_id,
            owner_element_id: owner.to_vec(),
            constraint_id: ConstraintNameId::from_raw(513),
            encoded_value: value.to_vec(),
            op,
        }
    }

    /// Every shape a receipt takes reads back as written: an Acquire's own claim, a Release
    /// with another mutation's claim or none, vertex and edge owners, empty and long values.
    #[test]
    fn stored_receipts_read_back_whole() {
        let own = ClaimId::new(MutationId::new(7), 9);
        let other = ClaimId::new(MutationId::new(u64::MAX), u32::MAX);
        let long = vec![0xAB; 4096];
        for receipt in [
            receipt(UniqueEffectOp::Acquire, Some(own), &[1; 8], b"v"),
            receipt(UniqueEffectOp::Release, Some(other), &[2; 12], &long),
            receipt(UniqueEffectOp::Release, Some(own), &[], b""),
            receipt(UniqueEffectOp::Release, None, &[3; 12], b"x"),
        ] {
            let bytes = OutboxValue::of(receipt.clone()).into_bytes();
            let read = OutboxValue::from_bytes(Cow::Owned(bytes)).receipt(receipt.effect_id);
            assert_eq!(read, receipt);
        }
    }

    /// An Acquire of its own claim takes the flags, constraint, ordinal and owner, and the value.
    #[test]
    fn an_acquire_is_packed() {
        let acquire = receipt(
            UniqueEffectOp::Acquire,
            Some(ClaimId::new(MutationId::new(7), 9)),
            &[1; 8],
            b"value",
        );
        assert_eq!(
            OutboxValue::of(acquire).into_bytes().len(),
            1 + 1 + 2 + 4 + 1 + 8 + 5
        );
    }

    /// A record of another layout version fails closed rather than misreading.
    #[test]
    #[should_panic(expected = "unknown unique effect layout version")]
    fn another_layout_version_is_rejected() {
        let mut bytes =
            OutboxValue::of(receipt(UniqueEffectOp::Release, None, &[1; 8], b"v")).into_bytes();
        bytes[0] = OutboxLayout::CURRENT as u8 + 1;
        let _ = OutboxValue::from_bytes(Cow::Owned(bytes));
    }
}
