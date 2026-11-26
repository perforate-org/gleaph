//! An edge half as a reader receives it: its neighbour, its position in the bucket, its tagged
//! label and its inline property bytes.
//!
//! The store keeps the neighbour as a node id and the bytes in the label's value pages;
//! `facade::topology` builds this value from them (ADR 0136, Decisions 2 and 4). `target` is a
//! [`VertexRef`], local or remote; its 30-bit payload is the shard's vertex bound
//! ([`super::vertex_ref::MAX_PAYLOAD_VERTEX_ID`]).

use super::EdgeInlinePropertyBytes;
use super::remote_vertex_id::EdgeTarget;
use super::vertex_id::VertexId;
use super::vertex_ref::VertexRef;
use std::hash::{Hash, Hasher};

mod id;
mod meta;

pub use id::EdgeSlotIndex;
pub use meta::EdgeMeta;

/// An edge half read from the store. Identity is topology only: the inline property bytes are
/// left out of equality and hashing.
#[derive(Clone, Debug)]
pub struct Edge {
    pub target: VertexRef,
    pub edge_slot_index: EdgeSlotIndex,
    pub label_id: u16,
    /// The inline property bytes the read attached.
    /// Not part of topology identity: excluded from PartialEq/Hash.
    pub inline_property: EdgeInlinePropertyBytes,
}

impl PartialEq for Edge {
    /// Topology-only equality: inline property bytes are excluded.
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.edge_slot_index == other.edge_slot_index
            && self.label_id == other.label_id
    }
}

impl Eq for Edge {}

impl Hash for Edge {
    /// Topology-only hash. Inline property bytes are intentionally excluded.
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.target.hash(state);
        self.edge_slot_index.hash(state);
        self.label_id.hash(state);
    }
}

impl Edge {
    /// The neighbor's local id.
    #[inline]
    pub fn neighbor_vid(&self) -> VertexId {
        self.target.local_id()
    }

    /// The edge's stored inline property bytes.
    #[inline]
    pub fn edge_inline_property_bytes(&self) -> &[u8] {
        self.inline_property.as_slice()
    }

    /// The edge with `bytes` as its inline property bytes.
    #[inline]
    #[must_use]
    pub fn with_stored_inline_property_bytes(self, bytes: &[u8]) -> Self {
        Self {
            inline_property: EdgeInlinePropertyBytes::from_slice(bytes),
            ..self
        }
    }

    /// The edge's slot within its bucket.
    #[inline]
    pub fn edge_slot_index_raw(&self) -> u32 {
        self.edge_slot_index.raw()
    }

    /// Resolves the stored neighbor as a local or remote [`EdgeTarget`].
    #[inline]
    pub fn edge_target(&self) -> Option<EdgeTarget> {
        self.target.edge_target()
    }

    /// Returns a copy with the given inline property bytes attached.
    #[inline]
    pub fn with_inline_property_bytes(&self, bytes: &[u8]) -> Self {
        Self {
            inline_property: EdgeInlinePropertyBytes::from_slice(bytes),
            ..self.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;

    fn test_edge(slot: u32, label_id: u16) -> Edge {
        Edge {
            target: VertexRef::local(VertexId::from(1)),
            edge_slot_index: EdgeSlotIndex::from_raw(slot),
            label_id,
            inline_property: EdgeInlinePropertyBytes::EMPTY,
        }
    }

    fn hash(edge: &Edge) -> u64 {
        let mut hasher = DefaultHasher::new();
        edge.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn edge_identity_includes_label_id_and_slot_index() {
        let base = test_edge(1, 2);
        let different_slot = test_edge(2, 2);
        let different_label = test_edge(1, 3);

        assert_ne!(base, different_slot);
        assert_ne!(base, different_label);
        assert_ne!(hash(&base), hash(&different_slot));
        assert_ne!(hash(&base), hash(&different_label));
    }

    #[test]
    fn edge_identity_is_topology_only() {
        let topology = Edge {
            target: VertexRef::local(VertexId::from(1)),
            edge_slot_index: EdgeSlotIndex::from_raw(5),
            label_id: 7,
            inline_property: EdgeInlinePropertyBytes::EMPTY,
        };
        let copy = topology.clone();

        assert_eq!(topology, copy);
        assert_eq!(hash(&topology), hash(&copy));
    }
}
