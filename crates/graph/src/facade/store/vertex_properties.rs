//! GraphStore `vertex_properties` implementation.

use super::super::stable::VERTEX_PROPERTIES;
use super::super::stable::vertex_properties::VertexPropertyKey;
use super::error::GraphStoreError;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use ic_stable_structures::Storable;

use super::GraphStore;

impl GraphStore {
    pub fn vertex_property(&self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.get(vertex_id, property_id))
    }

    pub fn set_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_write(vertex_id, property_id, value, true, 0)
    }

    pub(crate) fn set_vertex_property_without_index_pending(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_write(vertex_id, property_id, value, false, 0)
    }

    /// Write a vertex property under an explicit canonical mutation identity for the fence.
    pub(crate) fn set_vertex_property_with_mutation_id(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
        mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_write(vertex_id, property_id, value, true, mutation_id)
    }

    pub fn remove_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_remove(vertex_id, property_id, 0)
    }

    /// Remove a vertex property under an explicit canonical mutation identity for the fence.
    pub(crate) fn remove_vertex_property_with_mutation_id(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_remove(vertex_id, property_id, mutation_id)
    }

    pub fn vertex_properties(&self, vertex_id: VertexId) -> Vec<(PropertyId, Value)> {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.properties_for(vertex_id))
    }

    pub(crate) fn vertex_property_cursor(key: VertexPropertyKey) -> Vec<u8> {
        Storable::into_bytes(key)
    }

    pub(crate) fn scan_vertex_properties_batch(
        &self,
        after_key: Option<Vec<u8>>,
        max_entries: u32,
    ) -> Result<Vec<(VertexPropertyKey, Value)>, String> {
        let after = match after_key {
            None => None,
            Some(bytes) => {
                if bytes.len() != 8 {
                    return Err("invalid vertex property cursor key length".into());
                }
                Some(VertexPropertyKey::from_bytes(std::borrow::Cow::Borrowed(
                    &bytes,
                )))
            }
        };
        Ok(VERTEX_PROPERTIES
            .with_borrow(|properties| properties.scan_properties_batch(after, max_entries)))
    }

    pub(crate) fn vertex_properties_gql_record(&self, vertex_id: VertexId) -> Value {
        VERTEX_PROPERTIES.with_borrow(|properties| {
            let mut fields: Vec<(String, Value)> = Vec::new();
            properties.for_each_property_for(vertex_id, |property_id, value| {
                let name = self
                    .property_name(property_id)
                    .unwrap_or_else(|| property_id.raw().to_string());
                fields.push((name, value));
            });
            if fields.is_empty() {
                Value::Record(Vec::new())
            } else {
                Value::Record(fields)
            }
        })
    }
}
