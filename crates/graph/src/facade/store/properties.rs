//! Property storage domain: primary stores plus derived index-maintenance events.

use super::super::VertexPropertyStoreError;
use super::super::stable::{EDGE_PROPERTIES, VERTEX_PROPERTIES};
use crate::property::{PropertyValueChange, dispatch_property_index_ops};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::{VertexId, labeled::CanonicalEdgeOccurrence};

use super::GraphStore;

impl GraphStore {
    /// Write a vertex property and enqueue federated index maintenance when enabled.
    pub(super) fn commit_vertex_property_write(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
        record_index_pending: bool,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        let prev =
            VERTEX_PROPERTIES.with_borrow(|properties| properties.get(vertex_id, property_id));
        let out = VERTEX_PROPERTIES
            .with_borrow_mut(|properties| properties.set(vertex_id, property_id, value.clone()))?;
        if record_index_pending {
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vertex_id,
                property_id,
                prev.as_ref(),
                Some(&value),
            ));
        }
        Ok(out)
    }

    /// Remove a vertex property and enqueue federated index maintenance when enabled.
    pub(super) fn commit_vertex_property_remove(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
    ) -> Option<Value> {
        let removed = VERTEX_PROPERTIES
            .with_borrow_mut(|properties| properties.remove(vertex_id, property_id));
        if let Some(ref old) = removed {
            dispatch_property_index_ops(PropertyValueChange::vertex(
                vertex_id,
                property_id,
                Some(old),
                None,
            ));
        }
        removed
    }

    /// Write an edge property on a canonical handle and update local equality postings.
    ///
    /// The caller supplies an explicit orientation-aware [`CanonicalEdgeOccurrence`].
    /// CounterpartScan resolves the canonical sidecar handle before any `EDGE_PROPERTIES`
    /// mutation; a lookup failure leaves sidecar and index state unchanged.
    pub(super) fn commit_edge_property_write(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        self.commit_edge_property_write_at_canonical(handle, property_id, value)
    }

    /// Write a property when the caller already owns an exact canonical sidecar handle.
    ///
    /// Batch insertion uses this after LARA has returned the captured location. The
    /// batch preflight has already validated every id and value, so this path does
    /// not perform a counterpart scan or introduce another fallible lookup.
    pub(crate) fn commit_edge_property_write_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        let prev = EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
            )
        });
        let old = EDGE_PROPERTIES
            .with_borrow_mut(|properties| {
                properties.set(
                    handle.owner_vertex_id,
                    handle.label_id.raw(),
                    handle.slot_index.raw(),
                    property_id,
                    value.clone(),
                )
            })
            .map_err(super::error::GraphStoreError::from)?;
        dispatch_property_index_ops(PropertyValueChange::edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index.raw(),
            property_id,
            prev.as_ref(),
            Some(&value),
        ));
        Ok(old)
    }

    /// Co-write a preflighted set of initial properties at one canonical edge.
    ///
    /// The caller validates the complete set before LARA commit. This method keeps
    /// the primary sidecar writes and their derived-event dispatch under one Graph
    /// boundary; a post-preflight storage failure is an invariant violation.
    pub(crate) fn commit_edge_property_writes_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
        properties: &[(PropertyId, Value)],
    ) {
        let previous = EDGE_PROPERTIES.with_borrow_mut(|store| {
            properties
                .iter()
                .map(|(property_id, value)| {
                    let previous = store
                        .set(
                            handle.owner_vertex_id,
                            handle.label_id.raw(),
                            handle.slot_index.raw(),
                            *property_id,
                            value.clone(),
                        )
                        .expect("validated batch sidecar must be writable");
                    (*property_id, previous)
                })
                .collect::<Vec<_>>()
        });
        for ((property_id, value), (_, previous)) in properties.iter().zip(previous) {
            dispatch_property_index_ops(PropertyValueChange::edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                *property_id,
                previous.as_ref(),
                Some(value),
            ));
        }
    }

    /// Remove an edge property on a canonical handle and update local equality postings.
    pub(super) fn commit_edge_property_remove(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        let prev = EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
            )
        });
        let removed = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            properties.remove(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
            )
        });
        if let Some(ref old) = prev {
            dispatch_property_index_ops(PropertyValueChange::edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
                Some(old),
                None,
            ));
        }
        Ok(removed)
    }

    /// Remove every edge property on a canonical handle.
    ///
    /// The caller must supply an explicit orientation-aware occurrence; this method resolves the
    /// canonical sidecar handle before the removal.
    pub(super) fn commit_remove_all_edge_properties(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<(), super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        self.commit_remove_all_edge_properties_at_canonical(handle);
        Ok(())
    }

    pub(super) fn commit_remove_all_edge_properties_at_canonical(
        &self,
        handle: super::handle::EdgeHandle,
    ) {
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
            );
        });
    }

    pub(super) fn commit_move_edge_properties(
        owner_vertex_id: VertexId,
        moved: ic_stable_lara::labeled::EdgeSlotMove,
    ) -> Vec<(PropertyId, Value)> {
        let label_id = moved.label_id.raw();
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store
                .move_all_for_edge(
                    owner_vertex_id,
                    label_id,
                    moved.old_slot_index,
                    moved.new_slot_index,
                )
                .expect("stored edge property values remain encodable")
        })
    }

    /// Remove every vertex property and enqueue federated index maintenance when enabled.
    pub(super) fn commit_clear_vertex_properties(&self, vertex_id: VertexId) {
        let props: Vec<PropertyId> = VERTEX_PROPERTIES.with_borrow(|store| {
            store
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        for property_id in props {
            let _ = self.commit_vertex_property_remove(vertex_id, property_id);
        }
    }
}
