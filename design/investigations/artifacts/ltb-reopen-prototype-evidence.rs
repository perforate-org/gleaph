//! ADR 0088 Gate 4 (LTB reopen envelope) evidence-only prototype.
//!
//! **Evidence-only.** Not wired into [`super::graph::LabeledLaraGraph`]. Models
//! the LTB block-store reopen walk so we can measure `ins/block` against the
//! `bench_lara_free_span_store_reopen_*` precedent at the declared free-count
//! envelope. Mirrors `tree_csr_prototype.rs`'s evidence-only contract.
//!
//! Layout (matches ADR 0088 §1 + §8):
//! - Free list is an intrusive linked list of `BlockId`s, terminated by
//!   `NULL_BLOCK = u32::MAX`.
//! - Each free block carries a `kind` (1 byte) and a `next_free` (u32) — the
//!   only fields validated at reopen. Pop rewrites `kind` before returning
//!   the id.
//! - Reopen walks up to `min(free_count, declared envelope)` ids with bounds,
//!   kind, and cycle checks (ADR 0088 §8).

use crate::VectorMemory;
use ic_stable_structures::{StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

const NULL_BLOCK: u32 = u32::MAX;

/// Block id (dense u32).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlockId(pub(crate) u32);

impl Storable for BlockId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self(u32::from_le_bytes(
            bytes[0..4].try_into().expect("BlockId is 4 bytes"),
        ))
    }
}

/// Block kind (1 byte): 0 Free, 1 Edge. Other variants in ADR 0088 are not
/// exercised by the reopen walk (they live behind a root array; only Free
/// blocks are visible to the free list).
const KIND_FREE: u8 = 0;
const KIND_EDGE: u8 = 1;

/// Per-block record stored at reopen time. The reopen walk reads only `kind`
/// and `next_free`; the rest is metadata for the pop-time guard.
#[derive(Clone, Copy, Debug)]
struct BlockRecord {
    kind: u8,
    next_free: u32,
}

impl Storable for BlockRecord {
    const BOUND: Bound = Bound::Bounded {
        max_size: 5,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(5);
        out.push(self.kind);
        out.extend_from_slice(&self.next_free.to_le_bytes());
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self {
            kind: bytes[0],
            next_free: u32::from_le_bytes(bytes[1..5].try_into().expect("BlockRecord is 5 bytes")),
        }
    }
}

type BlockMap = StableBTreeMap<BlockId, BlockRecord, VectorMemory>;

/// Minimal LTB-shaped free list. Stores the intrusive list head (u32) plus a
/// `BlockMap<BlockId, BlockRecord>` keyed by block id.
#[allow(
    dead_code,
    reason = "All methods are exercised by benches and tests; allow until wired."
)]
pub(crate) struct LtbFreeList {
    blocks: BlockMap,
    free_head: u32,
    free_count: u32,
    tail_next: u32,
}

#[allow(
    dead_code,
    reason = "All methods are exercised by benches and tests; allow until wired."
)]
impl LtbFreeList {
    pub(crate) fn new(memory: VectorMemory) -> Self {
        Self {
            blocks: StableBTreeMap::init(memory),
            free_head: NULL_BLOCK,
            free_count: 0,
            tail_next: 0,
        }
    }

    pub(crate) fn free_count(&self) -> u32 {
        self.free_count
    }

    /// Mint a new block at `tail_next`, returning its id. Idempotent to the
    /// `BlockId` domain: ids are never reused.
    pub(crate) fn mint_edge(&mut self) -> BlockId {
        let id = self.tail_next;
        self.tail_next = self
            .tail_next
            .checked_add(1)
            .expect("ltb: block id overflow");
        self.blocks.insert(
            BlockId(id),
            BlockRecord {
                kind: KIND_EDGE,
                next_free: NULL_BLOCK,
            },
        );
        BlockId(id)
    }

    /// Release a previously-minted edge block: rewrite `kind = Free`, push
    /// the id onto the free list, and bump the free count.
    pub(crate) fn release(&mut self, id: BlockId) {
        debug_assert!(id.0 < self.tail_next, "release: id out of range");
        let next_free = self.free_head;
        self.blocks.insert(
            id,
            BlockRecord {
                kind: KIND_FREE,
                next_free,
            },
        );
        self.free_head = id.0;
        self.free_count = self
            .free_count
            .checked_add(1)
            .expect("ltb: free_count overflow");
    }

    /// Pop-time guard (ADR 0088 §8): pop the head, rewrite `kind`, return the
    /// id. Fails closed if the list is empty.
    pub(crate) fn pop(&mut self) -> Option<BlockId> {
        if self.free_head == NULL_BLOCK {
            return None;
        }
        let id = BlockId(self.free_head);
        let record = self.blocks.get(&id).expect("ltb: missing free head");
        debug_assert_eq!(record.kind, KIND_FREE, "ltb: head is not Free");
        self.free_head = record.next_free;
        self.free_count = self
            .free_count
            .checked_sub(1)
            .expect("ltb: free_count underflow");
        self.blocks.insert(
            id,
            BlockRecord {
                kind: KIND_EDGE,
                next_free: NULL_BLOCK,
            },
        );
        Some(id)
    }

    /// Pop, re-mint, and re-pop. Confirms the free-list rewrite guard (kind
    /// rewrite prevents handing out a block twice). Used by the
    /// `pop_guard_*` benches and unit tests.
    pub(crate) fn pop_remint_repop(&mut self) -> Option<(BlockId, BlockId)> {
        let first = self.pop()?;
        // Re-mint without going through the free list: simulate reuse by
        // releasing-then-popping the same id.
        self.release(first);
        let second = self.pop()?;
        Some((first, second))
    }
}

/// Reopen walk (ADR 0088 §8). Walks up to `min(free_count, envelope)` ids
/// from the free-list head, checking bounds, kind, and cycle.
pub(crate) struct LtbReopenWalk<'a> {
    free_list: &'a LtbFreeList,
    envelope: u32,
}

impl<'a> LtbReopenWalk<'a> {
    pub(crate) fn new(free_list: &'a LtbFreeList, envelope: u32) -> Self {
        Self {
            free_list,
            envelope,
        }
    }

    /// Run the reopen walk. Returns the number of ids visited.
    pub(crate) fn run(&self) -> u32 {
        let limit = self.free_list.free_count.min(self.envelope);
        let mut visited = 0u32;
        let mut current = self.free_list.free_head;
        let mut steps = 0u32;
        // The walk is bounded by both `limit` (declared envelope) and `steps`
        // (cycle defense: the free-list length cannot exceed `free_count`).
        while current != NULL_BLOCK && visited < limit && steps < self.free_list.free_count + 1 {
            let id = BlockId(current);
            let record = self
                .free_list
                .blocks
                .get(&id)
                .expect("ltb reopen: missing block id");
            debug_assert_eq!(record.kind, KIND_FREE, "ltb reopen: kind != Free");
            current = record.next_free;
            visited += 1;
            steps += 1;
        }
        visited
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    #[test]
    fn fresh_free_list_is_empty() {
        let mut ltb = LtbFreeList::new(vector_memory());
        assert_eq!(ltb.free_count(), 0);
        assert!(ltb.pop().is_none());
    }

    #[test]
    fn mint_release_pop_round_trip() {
        let mut ltb = LtbFreeList::new(vector_memory());
        let a = ltb.mint_edge();
        let b = ltb.mint_edge();
        ltb.release(a);
        ltb.release(b);
        assert_eq!(ltb.free_count(), 2);
        let popped = ltb.pop().expect("pop");
        assert_eq!(popped, b, "LIFO: last released pops first");
        let popped = ltb.pop().expect("pop");
        assert_eq!(popped, a);
        assert!(ltb.pop().is_none());
    }

    #[test]
    fn pop_remint_repop_yields_same_id() {
        let mut ltb = LtbFreeList::new(vector_memory());
        let a = ltb.mint_edge();
        ltb.release(a);
        let (first, second) = ltb.pop_remint_repop().expect("remint");
        assert_eq!(first, second);
        assert_eq!(first, a);
    }

    #[test]
    fn reopen_walk_visits_all_free_entries_under_envelope() {
        let mut ltb = LtbFreeList::new(vector_memory());
        for _ in 0..10u32 {
            let id = ltb.mint_edge();
            ltb.release(id);
        }
        let walk = LtbReopenWalk::new(&ltb, 1024);
        assert_eq!(walk.run(), 10, "all free entries visited");
    }

    #[test]
    fn reopen_walk_respects_envelope() {
        let mut ltb = LtbFreeList::new(vector_memory());
        for _ in 0..100u32 {
            let id = ltb.mint_edge();
            ltb.release(id);
        }
        let walk = LtbReopenWalk::new(&ltb, 10);
        assert_eq!(walk.run(), 10, "envelope caps walk length");
    }
}
