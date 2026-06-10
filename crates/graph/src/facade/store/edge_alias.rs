//! GraphStore `edge_alias` implementation.

use super::super::stable::{EDGE_ALIASES, GRAPH};
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId, traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{edge_alias_slot_key, edge_alias_slot_key_parts};

impl GraphStore {
    pub(crate) fn find_forward_edge_bucket_label(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<Option<LaraLabelId>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.find_forward_edge_label(owner_vertex_id, edge))
    }

    pub(crate) fn find_first_forward_handle_descending<F>(
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

    pub(crate) fn find_first_reverse_handle_descending<F>(
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

    pub(super) fn find_reverse_alias_for_canonical(
        &self,
        canonical: EdgeHandle,
        target_vertex_id: VertexId,
        source_vertex_id: VertexId,
    ) -> Result<Option<EdgeHandle>, GraphStoreError> {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label(target_vertex_id, canonical.label_id, |edge| {
                    if found.is_none() && edge.neighbor_vid() == source_vertex_id {
                        found = Some(EdgeHandle::at_slot(
                            target_vertex_id,
                            canonical.label_id,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    pub(crate) fn canonical_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        EDGE_ALIASES
            .with_borrow(|aliases| {
                aliases.get(
                    handle.owner_vertex_id,
                    handle.label_id.raw(),
                    handle.slot_index,
                )
            })
            .map(|canonical| {
                EdgeHandle::at_slot(
                    canonical.canonical_vertex_id(),
                    handle.label_id,
                    canonical.canonical_slot_index(),
                )
            })
            .unwrap_or(handle)
    }

    pub(crate) fn canonical_reverse_in_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        EDGE_ALIASES
            .with_borrow(|aliases| {
                aliases.get(
                    handle.owner_vertex_id,
                    handle.label_id.raw(),
                    edge_alias_slot_key(handle.slot_index, true),
                )
            })
            .map(|canonical| {
                EdgeHandle::at_slot(
                    canonical.canonical_vertex_id(),
                    handle.label_id,
                    canonical.canonical_slot_index(),
                )
            })
            .unwrap_or(handle)
    }

    pub(crate) fn canonical_edge_handle_for_sidecar(&self, handle: EdgeHandle) -> EdgeHandle {
        let reverse = self.canonical_reverse_in_edge_handle(handle);
        if reverse != handle {
            return reverse;
        }
        self.canonical_edge_handle(handle)
    }

    pub(super) fn remove_reverse_edge_for_canonical_directed(
        &self,
        row_vertex_id: VertexId,
        owner_vertex_id: VertexId,
        label_id: LaraLabelId,
        forward_slot_index: u32,
    ) -> Result<(), GraphStoreError> {
        let removed = self.with_graph_mut(|graph| {
            graph.remove_reverse_edge_at_slot(row_vertex_id, label_id, forward_slot_index)
        })?;
        if removed.is_some() {
            return Ok(());
        }
        let mut sole_slot = None;
        let mut count = 0u32;
        self.with_graph_mut(|graph| {
            graph.for_each_in_edges_for_label(row_vertex_id, label_id, |edge| {
                if edge.neighbor_vid() == owner_vertex_id {
                    count = count.saturating_add(1);
                    sole_slot = Some(edge.edge_slot_index.raw());
                }
            })
        })?;
        if count == 1 {
            let _ = self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot(
                    row_vertex_id,
                    label_id,
                    sole_slot.expect("count == 1"),
                )
            })?;
        }
        Ok(())
    }

    pub(crate) fn alias_for_canonical_edge(
        &self,
        canonical: EdgeHandle,
    ) -> Option<(VertexId, u32, bool)> {
        EDGE_ALIASES.with_borrow(|aliases| {
            aliases
                .find_alias_for_canonical(
                    canonical.owner_vertex_id,
                    canonical.label_id.raw(),
                    canonical.slot_index,
                )
                .map(|(vertex_id, slot_key)| {
                    let (slot_index, reverse_in) = edge_alias_slot_key_parts(slot_key);
                    (vertex_id, slot_index, reverse_in)
                })
        })
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
