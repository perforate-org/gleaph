//! GraphStore counterpart-resolution implementation.

use super::super::stable::GRAPH;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::bidirectional::counterpart::{self, CounterpartLookupError},
    labeled::{CanonicalEdgeOccurrence, LabeledOrientation},
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub(crate) fn find_forward_edge_bucket_label(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<Option<LaraLabelId>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.find_forward_edge_label(owner_vertex_id, edge))
    }

    pub(crate) fn find_first_forward_handle<F>(
        &self,
        owner_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_for_label(owner_vertex_id, expected_label, |edge| {
                    if found.is_none() && pred(&edge) {
                        found = Some(EdgeHandle::at_slot(
                            owner_vertex_id,
                            expected_label,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    pub(crate) fn find_first_reverse_handle<F>(
        &self,
        row_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label(row_vertex_id, expected_label, |edge| {
                    if found.is_none() && pred(&edge) {
                        found = Some(EdgeHandle::at_slot(
                            row_vertex_id,
                            expected_label,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    pub(crate) fn canonical_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        self.scan_only_canonical_edge_handle(handle, LabeledOrientation::Forward)
            .unwrap_or(handle)
    }

    pub(crate) fn canonical_reverse_in_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        self.scan_only_canonical_edge_handle(handle, LabeledOrientation::Reverse)
            .unwrap_or(handle)
    }

    pub(crate) fn canonical_edge_handle_for_sidecar(&self, handle: EdgeHandle) -> EdgeHandle {
        let reverse = self.canonical_reverse_in_edge_handle(handle);
        if reverse != handle {
            return reverse;
        }
        self.canonical_edge_handle(handle)
    }

    /// Resolves a canonical handle through ADR 0048 CounterpartScan on the
    /// ADR 0050 `traverse_next` logical-slot surface.
    pub(crate) fn scan_only_canonical_edge_handle(
        &self,
        handle: EdgeHandle,
        orientation: LabeledOrientation,
    ) -> Result<EdgeHandle, CounterpartLookupError> {
        // For directed edges the forward occurrence is canonical by definition.
        // Avoid a read/rank/select of the reverse bucket for the overwhelmingly
        // common forward lookup; reverse occurrences and undirected edges still
        // use CounterpartScan below.
        if handle.label_id.is_directed() && orientation == LabeledOrientation::Forward {
            return Ok(handle);
        }
        self.scan_only_canonical_edge_handle_from_occurrence(handle.occurrence(orientation))
    }

    pub(crate) fn scan_only_canonical_edge_handle_from_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<EdgeHandle, CounterpartLookupError> {
        GRAPH
            .with_borrow(|graph| {
                counterpart::canonical_handle(graph.forward(), graph.reverse(), occurrence)
            })
            .map(|canonical| {
                EdgeHandle::at_slot(
                    canonical.owner_vertex_id,
                    canonical.label_id,
                    canonical.slot_index.raw(),
                )
            })
    }

    /// Resolves the canonical sidecar handle for a [`CanonicalEdgeOccurrence`] and maps
    /// [`CounterpartLookupError`] into the owning [`GraphStoreError`].
    ///
    /// This is the ADR 0048 CounterpartScan boundary used by the edge-property sidecar group.
    pub(crate) fn canonical_edge_handle_from_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.scan_only_canonical_edge_handle_from_occurrence(occurrence)
            .map_err(GraphStoreError::from)
    }

    /// Resolves the physical counterpart for a live occurrence. The caller supplies the orientation because a logical slot
    /// alone cannot distinguish the forward and reverse stores.
    pub(crate) fn counterpart_edge_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<CanonicalEdgeOccurrence, GraphStoreError> {
        GRAPH
            .with_borrow(|graph| {
                counterpart::counterpart_of(graph.forward(), graph.reverse(), occurrence)
            })
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn find_outgoing_edge_with_bucket_label(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        self.lookup_edge_entry(handle)
    }

    pub(crate) fn find_outgoing_edge_record(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<Edge>, GraphStoreError> {
        Ok(self.lookup_edge_entry(handle)?.map(|(edge, _)| edge))
    }
}
