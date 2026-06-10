//! Internal property-host identity for vertex and edge sidecars.

use ic_stable_lara::VertexId;

/// Host entity for a property value in graph storage.
///
/// Physical stable keys remain separate per entity class; this type centralizes
/// the logical identity used by validation, encoding, and index-maintenance paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PropertyEntity {
    Vertex(VertexId),
    Edge {
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    },
}

impl PropertyEntity {
    #[inline]
    pub const fn vertex(vertex_id: VertexId) -> Self {
        Self::Vertex(vertex_id)
    }

    #[inline]
    pub const fn edge(owner_vertex_id: VertexId, label_id: u16, slot_index: u32) -> Self {
        Self::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_and_edge_identities_are_distinct() {
        let vertex = PropertyEntity::vertex(VertexId::from(1));
        let edge = PropertyEntity::edge(VertexId::from(1), 2, 3);
        assert_ne!(vertex, edge);
    }
}
