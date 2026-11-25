//! GraphStore edge lookup by label/neighbor.
//!
//! Counterpart and canonical-handle resolution moved to [`super::counterpart`], which owns that
//! pairing; this file only finds rows.
use crate::facade::topology::TopologyError;
use gleaph_graph_kernel::entry::Edge;
use gleaph_graph_kernel::entry::EdgeOrientation;
use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub(crate) fn find_forward_edge_bucket_label(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<Option<TaggedEdgeLabelId>, TopologyError> {
        crate::facade::topology::read::find_forward_label(owner_vertex_id, edge)
    }

    pub(crate) fn find_first_forward_handle<F>(
        &self,
        owner_vertex_id: VertexId,
        expected_label: TaggedEdgeLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        crate::facade::topology::read::visit_bucket(
            owner_vertex_id,
            EdgeOrientation::Forward,
            expected_label,
            crate::facade::topology::read::BucketRead::Default,
            |edge| {
                if found.is_none() && pred(&edge) {
                    found = Some(EdgeHandle::at_slot(
                        owner_vertex_id,
                        expected_label,
                        edge.edge_slot_index.raw(),
                    ));
                }
            },
        )
        .map_err(GraphStoreError::from)
        .map(|()| found)
    }

    pub(crate) fn find_first_reverse_handle<F>(
        &self,
        row_vertex_id: VertexId,
        expected_label: TaggedEdgeLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        crate::facade::topology::read::visit_bucket(
            row_vertex_id,
            EdgeOrientation::Reverse,
            expected_label,
            crate::facade::topology::read::BucketRead::Default,
            |edge| {
                if found.is_none() && pred(&edge) {
                    found = Some(EdgeHandle::at_slot(
                        row_vertex_id,
                        expected_label,
                        edge.edge_slot_index.raw(),
                    ));
                }
            },
        )
        .map_err(GraphStoreError::from)
        .map(|()| found)
    }

    pub(crate) fn find_outgoing_edge_with_bucket_label(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, TaggedEdgeLabelId)>, GraphStoreError> {
        self.lookup_edge_entry(handle)
    }

    pub(crate) fn find_outgoing_edge_record(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<Edge>, GraphStoreError> {
        Ok(self.lookup_edge_entry(handle)?.map(|(edge, _)| edge))
    }
}
