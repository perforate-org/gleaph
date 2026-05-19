//! Stable index of forward out-edges whose target is a [`RemoteRefId`].
//!
//! Non-authoritative shards use this to answer federated reverse expand without
//! scanning every vertex's adjacency list.

use gleaph_graph_kernel::entry::RemoteRefId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{BTreeSet, Memory, Storable, storable::Bound};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteForwardInKey {
    remote_ref: RemoteRefId,
    source_vertex_id: u32,
    label_id: u16,
    slot_index: u32,
}

impl RemoteForwardInKey {
    pub fn new(
        remote_ref: RemoteRefId,
        source_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) -> Self {
        Self {
            remote_ref,
            source_vertex_id: u32::from(source_vertex_id),
            label_id,
            slot_index,
        }
    }

    pub fn remote_ref(self) -> RemoteRefId {
        self.remote_ref
    }

    pub fn source_vertex_id(self) -> VertexId {
        VertexId::from(self.source_vertex_id)
    }

    pub fn label_id(self) -> u16 {
        self.label_id
    }

    pub fn slot_index(self) -> u32 {
        self.slot_index
    }
}

impl Storable for RemoteForwardInKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 14,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(14);
        out.extend_from_slice(&self.remote_ref.to_le_bytes());
        out.extend_from_slice(&self.source_vertex_id.to_be_bytes());
        out.extend_from_slice(&self.label_id.to_be_bytes());
        out.extend_from_slice(&self.slot_index.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(
            bytes.len(),
            14,
            "RemoteForwardInKey expects exactly 14 bytes"
        );

        let mut remote = [0; 4];
        let mut source = [0; 4];
        let mut label = [0; 2];
        let mut slot = [0; 4];
        remote.copy_from_slice(&bytes[0..4]);
        source.copy_from_slice(&bytes[4..8]);
        label.copy_from_slice(&bytes[8..10]);
        slot.copy_from_slice(&bytes[10..14]);

        Self {
            remote_ref: RemoteRefId::from_le_bytes(remote),
            source_vertex_id: u32::from_be_bytes(source),
            label_id: u16::from_be_bytes(label),
            slot_index: u32::from_be_bytes(slot),
        }
    }
}

pub struct RemoteForwardInIndex<M: Memory> {
    postings: BTreeSet<RemoteForwardInKey, M>,
}

impl<M: Memory> RemoteForwardInIndex<M> {
    pub fn init(memory: M) -> Self {
        Self {
            postings: BTreeSet::init(memory),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    pub fn insert(
        &mut self,
        remote_ref: RemoteRefId,
        source_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) {
        let key = RemoteForwardInKey::new(remote_ref, source_vertex_id, label_id, slot_index);
        self.postings.insert(key);
    }

    pub fn remove(
        &mut self,
        remote_ref: RemoteRefId,
        source_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) {
        let key = RemoteForwardInKey::new(remote_ref, source_vertex_id, label_id, slot_index);
        self.postings.remove(&key);
    }

    pub fn move_slot(
        &mut self,
        remote_ref: RemoteRefId,
        source_vertex_id: VertexId,
        label_id: u16,
        old_slot_index: u32,
        new_slot_index: u32,
    ) {
        if old_slot_index == new_slot_index {
            return;
        }
        self.remove(remote_ref, source_vertex_id, label_id, old_slot_index);
        self.insert(remote_ref, source_vertex_id, label_id, new_slot_index);
    }

    pub fn for_each_for_remote_ref(
        &self,
        remote_ref: RemoteRefId,
        mut visit: impl FnMut(RemoteForwardInKey),
    ) {
        let start = RemoteForwardInKey::new(remote_ref, VertexId::from(0), 0, 0);
        let end = RemoteForwardInKey::new(
            RemoteRefId::from_raw(remote_ref.raw().saturating_add(1)),
            VertexId::from(0),
            0,
            0,
        );
        for key in self.postings.range(start..end) {
            visit(key);
        }
    }

    pub fn has_postings_for(&self, remote_ref: RemoteRefId) -> bool {
        let start = RemoteForwardInKey::new(remote_ref, VertexId::from(0), 0, 0);
        self.postings
            .range(start..)
            .next()
            .is_some_and(|key| key.remote_ref() == remote_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;

    #[test]
    fn range_scan_lists_only_matching_remote_ref() {
        let mut index = RemoteForwardInIndex::init(DefaultMemoryImpl::default());
        let a = RemoteRefId::from_raw(1);
        let b = RemoteRefId::from_raw(2);
        index.insert(a, VertexId::from(10), 3, 0);
        index.insert(a, VertexId::from(11), 3, 1);
        index.insert(b, VertexId::from(20), 4, 0);

        let mut hits = Vec::new();
        index.for_each_for_remote_ref(a, |key| hits.push(key.source_vertex_id()));
        assert_eq!(hits, vec![VertexId::from(10), VertexId::from(11)]);
    }
}
