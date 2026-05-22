//! GraphStore `vertex_row` implementation.

use super::super::stable::GRAPH;
use gleaph_graph_kernel::entry::Vertex;
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexId};

use super::GraphStore;

impl GraphStore {
    pub fn vertex(&self, vertex_id: VertexId) -> Option<Vertex> {
        GRAPH.with_borrow(|graph| graph.vertex_row(vertex_id).ok().map(Vertex::from))
    }

    pub fn set_vertex(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let row = vertex.into();
        GRAPH.with_borrow(|graph| graph.set_vertex_row(vertex_id, &row))
    }
}
