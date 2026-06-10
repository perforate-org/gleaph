//! Property storage domain: primary stores plus derived index-maintenance events.

use super::super::VertexPropertyStoreError;
use super::super::stable::{EDGE_PROPERTIES, VERTEX_PROPERTIES};
use crate::index::pending;
use crate::property::PropertyValueChange;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::{VertexId, labeled::EdgeSlotMove};

use super::GraphStore;
use super::handle::EdgeHandle;

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
            pending::record_vertex_property_change(PropertyValueChange::vertex(
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
            pending::record_vertex_property_change(PropertyValueChange::vertex(
                vertex_id,
                property_id,
                Some(old),
                None,
            ));
        }
        removed
    }

    /// Write an edge property on a canonical handle and update local equality postings.
    pub(super) fn commit_edge_property_write(
        &self,
        handle: EdgeHandle,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        let prev = EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
            )
        });
        let old = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            properties.set(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
                value.clone(),
            )
        })?;
        self.commit_record_edge_property_equality_change(PropertyValueChange::edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
            property_id,
            prev.as_ref(),
            Some(&value),
        ));
        Ok(old)
    }

    /// Remove an edge property on a canonical handle and update local equality postings.
    pub(super) fn commit_edge_property_remove(
        &self,
        handle: EdgeHandle,
        property_id: PropertyId,
    ) -> Option<Value> {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        let prev = EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
            )
        });
        let removed = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            properties.remove(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
            )
        });
        if let Some(ref old) = prev {
            self.commit_record_edge_property_equality_change(PropertyValueChange::edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
                Some(old),
                None,
            ));
        }
        removed
    }

    /// Remove every edge property on a canonical handle.
    pub(super) fn commit_remove_all_edge_properties(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(super) fn commit_move_edge_properties_for_compaction(
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
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
