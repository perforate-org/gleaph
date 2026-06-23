//! ADR 0022 Stage 2b prototype: a `seq`-keyed ordered-map backing for one hot
//! labeled bucket.
//!
//! **Evidence-only.** This is NOT wired into [`super::graph::LabeledLaraGraph`];
//! it exists to measure the B-tree tier's delete / scan / point-lookup cost
//! against the shared-leaf slab baselines in `labeled/bench.rs`, so the deferred
//! Stage 2b decision rests on numbers rather than assumption.
//!
//! Design (per ADR 0022 recorded constraints):
//! - Key is `(vertex, label, seq)` where `seq` is a monotonic, never-shifting
//!   per-bucket sequence id — the `slot_index` analog — **not** the target. A
//!   `(vertex, label)` prefix range scan yields one bucket's edges in `seq`
//!   order, which equals insertion order (`OutEdgeOrder::Ascending`); `.rev()`
//!   gives `Descending`. This preserves the order contract and the stable edge
//!   identity that edge-property / alias / posting sidecars key on.
//! - Delete is by `seq` (the realistic delete-by-handle path), O(log d), with no
//!   tombstone and no compaction.
//! - Point lookup by target is O(degree) (target is not the key) — matching the
//!   status quo; an optional `target -> seq` secondary index (not prototyped) is
//!   what would make it O(log d).

use crate::VectorMemory;
use ic_stable_structures::{StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::ops::Bound as RangeBound;

/// `(vertex, label, seq)` key. Field order is the sort order; big-endian bytes
/// keep the encoded order identical to the derived `Ord`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HubEdgeKey {
    pub vertex: u32,
    pub label: u16,
    pub seq: u32,
}

impl Storable for HubEdgeKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&self.vertex.to_be_bytes());
        out.extend_from_slice(&self.label.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self {
            vertex: u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
            label: u16::from_be_bytes(bytes[4..6].try_into().unwrap()),
            seq: u32::from_be_bytes(bytes[6..10].try_into().unwrap()),
        }
    }
}

/// Fixed 10-byte edge value, size-matched to `BenchEdge::BYTES` for a fair node
/// comparison; the target id lives in the first 4 little-endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HubEdgeVal([u8; 10]);

impl HubEdgeVal {
    fn from_target(target: u32) -> Self {
        let mut bytes = [0u8; 10];
        bytes[0..4].copy_from_slice(&target.to_le_bytes());
        Self(bytes)
    }

    fn target(&self) -> u32 {
        u32::from_le_bytes(self.0[0..4].try_into().unwrap())
    }
}

impl Storable for HubEdgeVal {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.as_ref().try_into().expect("HubEdgeVal is 10 bytes"))
    }
}

/// One shared seq-keyed ordered map standing in for the B-tree hub tier.
pub(crate) struct HubBucketTree {
    map: StableBTreeMap<HubEdgeKey, HubEdgeVal, VectorMemory>,
    next_seq: u32,
}

impl HubBucketTree {
    pub(crate) fn new(memory: VectorMemory) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
            next_seq: 0,
        }
    }

    /// Appends an edge, returning the assigned stable `seq` (the `slot_index`
    /// analog). Monotonic and never reused.
    pub(crate) fn insert(&mut self, vertex: u32, label: u16, target: u32) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.checked_add(1).expect("hub seq overflow");
        self.map.insert(
            HubEdgeKey { vertex, label, seq },
            HubEdgeVal::from_target(target),
        );
        seq
    }

    /// Deletes by `seq` (the realistic delete-by-handle path): O(log d), no
    /// tombstone, no compaction. Returns whether an edge was removed.
    pub(crate) fn remove_by_seq(&mut self, vertex: u32, label: u16, seq: u32) -> bool {
        self.map
            .remove(&HubEdgeKey { vertex, label, seq })
            .is_some()
    }

    fn bucket_range(vertex: u32, label: u16) -> (RangeBound<HubEdgeKey>, RangeBound<HubEdgeKey>) {
        let start = HubEdgeKey {
            vertex,
            label,
            seq: 0,
        };
        let end = HubEdgeKey {
            vertex,
            label,
            seq: u32::MAX,
        };
        (RangeBound::Included(start), RangeBound::Included(end))
    }

    /// Visits the bucket's edges in `seq` (= insertion) order with their target.
    pub(crate) fn for_each_ascending<F: FnMut(u32, u32)>(&self, vertex: u32, label: u16, mut f: F) {
        for entry in self.map.range(Self::bucket_range(vertex, label)) {
            f(entry.key().seq, entry.value().target());
        }
    }

    /// Visits the bucket's edges in reverse `seq` order (`OutEdgeOrder::Descending`).
    pub(crate) fn for_each_descending<F: FnMut(u32, u32)>(
        &self,
        vertex: u32,
        label: u16,
        mut f: F,
    ) {
        for entry in self.map.range(Self::bucket_range(vertex, label)).rev() {
            f(entry.key().seq, entry.value().target());
        }
    }

    /// Descending scan that reads **only the key** (never deserializes the value).
    /// Experiment probe: isolates the B-tree traversal + key-deser floor from the
    /// value-deserialization cost, to test whether shrinking/splitting the value
    /// can help scans (ADR 0022 Stage 2b). Bench-only probe (see `labeled/bench.rs`).
    #[cfg(feature = "canbench")]
    pub(crate) fn for_each_descending_key_only<F: FnMut(u32)>(
        &self,
        vertex: u32,
        label: u16,
        mut f: F,
    ) {
        for entry in self.map.range(Self::bucket_range(vertex, label)).rev() {
            f(entry.key().seq);
        }
    }

    /// Finds the `seq` of the first edge to `target` in descending order — the
    /// O(degree) point lookup the status quo also pays (no `target -> seq` index).
    pub(crate) fn find_seq_by_target(&self, vertex: u32, label: u16, target: u32) -> Option<u32> {
        for entry in self.map.range(Self::bucket_range(vertex, label)).rev() {
            if entry.value().target() == target {
                return Some(entry.key().seq);
            }
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> u64 {
        self.map.len()
    }
}

/// Production-faithful narrow variant: value is the bare 4-byte `target`
/// (`Edge::BYTES == 4`; per-edge payloads/properties already live in
/// `EdgePayloadStore`/`EdgePropertyStore`, never in the slab row). Used to test
/// whether shrinking the B-tree value from 10 to 4 bytes recovers scan cost
/// (ADR 0022 Stage 2b). Same `(vertex, label, seq)` key and semantics as
/// [`HubBucketTree`]. Bench-only variant (see `labeled/bench.rs`).
#[cfg(feature = "canbench")]
pub(crate) struct HubTargetTree {
    map: StableBTreeMap<HubEdgeKey, u32, VectorMemory>,
    next_seq: u32,
}

#[cfg(feature = "canbench")]
impl HubTargetTree {
    pub(crate) fn new(memory: VectorMemory) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
            next_seq: 0,
        }
    }

    pub(crate) fn insert(&mut self, vertex: u32, label: u16, target: u32) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.checked_add(1).expect("hub seq overflow");
        self.map.insert(HubEdgeKey { vertex, label, seq }, target);
        seq
    }

    pub(crate) fn remove_by_seq(&mut self, vertex: u32, label: u16, seq: u32) -> bool {
        self.map
            .remove(&HubEdgeKey { vertex, label, seq })
            .is_some()
    }

    pub(crate) fn for_each_descending<F: FnMut(u32, u32)>(
        &self,
        vertex: u32,
        label: u16,
        mut f: F,
    ) {
        for entry in self
            .map
            .range(HubBucketTree::bucket_range(vertex, label))
            .rev()
        {
            f(entry.key().seq, entry.value());
        }
    }

    pub(crate) fn find_seq_by_target(&self, vertex: u32, label: u16, target: u32) -> Option<u32> {
        for entry in self
            .map
            .range(HubBucketTree::bucket_range(vertex, label))
            .rev()
        {
            if entry.value() == target {
                return Some(entry.key().seq);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    const V: u32 = 7;
    const L: u16 = 3;

    fn collect_asc(tree: &HubBucketTree) -> Vec<u32> {
        let mut out = Vec::new();
        tree.for_each_ascending(V, L, |_seq, target| out.push(target));
        out
    }

    fn collect_desc(tree: &HubBucketTree) -> Vec<u32> {
        let mut out = Vec::new();
        tree.for_each_descending(V, L, |_seq, target| out.push(target));
        out
    }

    #[test]
    fn ascending_preserves_insertion_order_not_target_order() {
        let mut tree = HubBucketTree::new(vector_memory());
        // Insert in non-sorted target order; a target-keyed map would reorder.
        for target in [5u32, 3, 9, 1, 7] {
            tree.insert(V, L, target);
        }
        assert_eq!(collect_asc(&tree), vec![5, 3, 9, 1, 7]);
    }

    #[test]
    fn descending_is_reverse_insertion_order() {
        let mut tree = HubBucketTree::new(vector_memory());
        for target in [5u32, 3, 9, 1, 7] {
            tree.insert(V, L, target);
        }
        assert_eq!(collect_desc(&tree), vec![7, 1, 9, 3, 5]);
    }

    #[test]
    fn remove_by_seq_drops_one_edge_and_preserves_order() {
        let mut tree = HubBucketTree::new(vector_memory());
        let seqs: Vec<u32> = [5u32, 3, 9, 1, 7]
            .into_iter()
            .map(|t| tree.insert(V, L, t))
            .collect();
        // Remove the middle insertion (target 9, seq seqs[2]).
        assert!(tree.remove_by_seq(V, L, seqs[2]));
        assert!(
            !tree.remove_by_seq(V, L, seqs[2]),
            "second remove is a no-op"
        );
        assert_eq!(collect_asc(&tree), vec![5, 3, 1, 7]);
        assert_eq!(tree.len(), 4);
    }

    #[test]
    fn find_seq_by_target_returns_descending_first_match() {
        let mut tree = HubBucketTree::new(vector_memory());
        let mut seqs = Vec::new();
        for target in [5u32, 3, 9, 3, 7] {
            seqs.push(tree.insert(V, L, target));
        }
        // Two edges to target 3 (seq 1 and seq 3); descending finds the later one.
        assert_eq!(tree.find_seq_by_target(V, L, 3), Some(seqs[3]));
        assert_eq!(tree.find_seq_by_target(V, L, 9), Some(seqs[2]));
        assert_eq!(tree.find_seq_by_target(V, L, 404), None);
    }

    #[test]
    fn separate_buckets_do_not_bleed() {
        let mut tree = HubBucketTree::new(vector_memory());
        tree.insert(V, L, 1);
        tree.insert(V, L + 1, 2);
        tree.insert(V + 1, L, 3);
        assert_eq!(collect_asc(&tree), vec![1]);
        let mut other = Vec::new();
        tree.for_each_ascending(V, L + 1, |_s, t| other.push(t));
        assert_eq!(other, vec![2]);
    }
}
