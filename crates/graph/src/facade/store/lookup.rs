//! GraphStore `lookup` implementation.
use crate::facade::topology::TopologyError;
use gleaph_graph_kernel::entry::Edge;
use gleaph_graph_kernel::entry::EdgeOrientation;
use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    fn lookup_forward_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, TaggedEdgeLabelId)>, GraphStoreError> {
        Ok(
            crate::facade::topology::read::edge_at(handle.occurrence(EdgeOrientation::Forward))?
                .map(|edge| (edge, handle.label_id)),
        )
    }

    fn lookup_reverse_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, TaggedEdgeLabelId)>, GraphStoreError> {
        Ok(
            crate::facade::topology::read::edge_at(handle.occurrence(EdgeOrientation::Reverse))?
                .map(|edge| (edge, handle.label_id)),
        )
    }

    pub(super) fn lookup_edge_entry(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, TaggedEdgeLabelId)>, GraphStoreError> {
        if let Some(found) = self.lookup_forward_out_edge(handle)? {
            return Ok(Some(found));
        }
        let reverse_canonical = self.canonical_reverse_in_edge_handle(handle);
        if reverse_canonical != handle
            && let Some(found) = self.lookup_forward_out_edge(reverse_canonical)?
        {
            return Ok(Some(found));
        }
        let undirected_canonical = self.canonical_edge_handle(handle);
        if undirected_canonical != handle
            && let Some(found) = self.lookup_forward_out_edge(undirected_canonical)?
        {
            return Ok(Some(found));
        }
        if reverse_canonical != handle {
            return self.lookup_reverse_out_edge(reverse_canonical);
        }
        self.lookup_reverse_out_edge(handle)
    }

    fn contains_vertex(&self, vertex_id: VertexId) -> bool {
        self.vertex_exists(vertex_id)
    }

    pub(super) fn ensure_vertex_id(&self, vertex_id: VertexId) -> Result<(), TopologyError> {
        if self.contains_vertex(vertex_id) {
            Ok(())
        } else {
            Err(TopologyError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        }
    }
}
