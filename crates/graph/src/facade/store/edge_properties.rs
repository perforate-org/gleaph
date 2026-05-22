//! GraphStore `edge_properties` implementation.

use super::super::VertexPropertyStoreError;
use super::super::stable::EDGE_PROPERTIES;
use crate::index::edge_equal;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;

use super::GraphStore;
use super::handle::EdgeHandle;

impl GraphStore {
    pub fn edge_property(&self, handle: EdgeHandle, property_id: PropertyId) -> Option<Value> {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
            )
        })
    }

    pub fn set_edge_property(
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
        edge_equal::record_edge_property_change(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
            property_id,
            prev.as_ref(),
            Some(&value),
        );
        Ok(old)
    }

    pub fn remove_edge_property(
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
            edge_equal::record_edge_property_change(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                property_id,
                Some(old),
                None,
            );
        }
        removed
    }

    pub fn edge_properties(&self, handle: EdgeHandle) -> Vec<(PropertyId, Value)> {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.properties_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            )
        })
    }

    pub(crate) fn edge_properties_gql_record(&self, handle: EdgeHandle) -> Value {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        EDGE_PROPERTIES.with_borrow(|properties| {
            let mut fields: Vec<(String, Value)> = Vec::new();
            properties.for_each_property_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
                |property_id, value| {
                    let name = self
                        .property_name(property_id)
                        .unwrap_or_else(|| property_id.raw().to_string());
                    fields.push((name, value));
                },
            );
            if fields.is_empty() {
                Value::Record(Vec::new())
            } else {
                Value::Record(fields)
            }
        })
    }
}
