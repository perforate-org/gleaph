//! GraphStore counterpart resolution.
//!
//! A canonical edge has one row in each orientation, and which row is canonical depends on both -
//! so this file owns the graph-side surface of that pairing: how a handle or occurrence is mapped to
//! the canonical one, and how the physical counterpart row is found. The rule itself lives in
//! `ic_stable_lara`'s counterpart module, reached through
//! [`DeferredBidirectionalLabeledLaraGraph::counterparts`](ic_stable_lara::labeled::DeferredBidirectionalLabeledLaraGraph::counterparts);
//! this file only adapts it to `GraphStore` handles and errors.

use super::super::stable::GRAPH;
use ic_stable_lara::{
    labeled::bidirectional::counterpart::CounterpartLookupError,
    labeled::{CanonicalEdgeOccurrence, LabeledOrientation},
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
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
            .with_borrow(|graph| graph.counterparts().canonical_handle(occurrence))
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
            .with_borrow(|graph| graph.counterparts().of(occurrence))
            .map_err(GraphStoreError::from)
    }
}
