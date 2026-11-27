//! GraphStore `vertex_properties` implementation.

use super::super::stable::VERTEX_PROPERTIES;
use super::super::stable::vertex_properties::VertexPropertyKey;
use super::error::GraphStoreError;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::entry::VertexId;
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
        self.commit_vertex_property_write(vertex_id, property_id, value, true, 0.into())
    }

    pub(crate) fn set_vertex_property_without_index_pending(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, GraphStoreError> {
        self.commit_vertex_property_write(vertex_id, property_id, value, false, 0.into())
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
        self.commit_vertex_property_remove(vertex_id, property_id, 0.into())
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

    /// Whether the vertex property store is deep enough for reading ahead to pay at all.
    pub(crate) fn vertex_properties_deep_enough_to_sweep(&self) -> bool {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.deep_enough_to_sweep())
    }

    /// Whether reading `count` vertices from `first` to `last` ahead in one sweep pays.
    pub(crate) fn vertex_properties_sweep_pays(
        &self,
        first: VertexId,
        last: VertexId,
        count: usize,
    ) -> bool {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.sweep_pays(first, last, count))
    }

    /// [`Self::vertex_properties_gql_record`] of each of `vertex_ids` (ascending and distinct),
    /// in their order, read in one sweep; `None` when they are too sparse for a sweep to pay.
    pub(crate) fn vertex_properties_gql_records(
        &self,
        vertex_ids: &[VertexId],
    ) -> Option<Vec<Value>> {
        let mut records: Vec<Vec<(String, Value)>> = vec![Vec::new(); vertex_ids.len()];
        let mut at = 0;
        let swept = VERTEX_PROPERTIES.with_borrow(|properties| {
            properties.sweep_properties_of(vertex_ids, |vertex_id, property_id, value| {
                while vertex_ids.get(at).is_some_and(|&id| id < vertex_id) {
                    at += 1;
                }
                let name = self
                    .property_name(property_id)
                    .unwrap_or_else(|| property_id.raw().to_string());
                if let Some(fields) = records.get_mut(at) {
                    fields.push((name, value));
                }
            })
        });
        swept.then(|| records.into_iter().map(Value::Record).collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Read ahead in one sweep, each vertex's record is the one it reads alone, vertices without
    /// properties included; ids too sparse for a sweep are left to single reads.
    #[test]
    fn records_read_ahead_match_each_vertex_read_alone() {
        let store = GraphStore::new();
        let name = store.get_or_insert_property_id("name").expect("property");
        let age = store.get_or_insert_property_id("age").expect("property");
        let small: Vec<VertexId> = (0..8)
            .map(|_| store.insert_vertex().expect("vertex"))
            .collect();
        for &vertex in &small {
            store
                .set_vertex_property(vertex, name, Value::Text("small".into()))
                .expect("name");
        }
        assert!(
            store.vertex_properties_gql_records(&small).is_none(),
            "a shallow store is left to single reads"
        );
        // Enough entries for a lookup to descend three levels: a sweep pays.
        let vertices: Vec<VertexId> = (0..1100)
            .map(|_| store.insert_vertex().expect("vertex"))
            .collect();
        for (index, &vertex) in vertices.iter().enumerate() {
            if index % 3 != 0 {
                store
                    .set_vertex_property(vertex, name, Value::Text(format!("v{index}")))
                    .expect("name");
            }
            if index % 2 == 0 {
                store
                    .set_vertex_property(vertex, age, Value::Int64(index as i64))
                    .expect("age");
            }
        }
        let dense: Vec<VertexId> = vertices[5..30].to_vec();
        let sparse: Vec<VertexId> = vec![vertices[0], vertices[1099]];
        for ids in [dense, sparse] {
            let alone: Vec<Value> = ids
                .iter()
                .map(|&vertex| store.vertex_properties_gql_record(vertex))
                .collect();
            if let Some(swept) = store.vertex_properties_gql_records(&ids) {
                assert_eq!(swept, alone);
            }
        }
        assert!(
            store
                .vertex_properties_gql_records(&vertices[5..30])
                .is_some(),
            "dense ids are swept"
        );
        assert!(
            store
                .vertex_properties_gql_records(&[vertices[0], vertices[1099]])
                .is_none(),
            "sparse ids are left to single reads"
        );
    }
}
