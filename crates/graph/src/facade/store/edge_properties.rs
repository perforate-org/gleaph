//! GraphStore `edge_properties` implementation.

use super::super::VertexPropertyStoreError;
use super::super::stable::EDGE_PROPERTIES;
use super::super::stable::edge_properties::EdgePropertyKey;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_structures::Storable;

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
        self.commit_edge_property_write(handle, property_id, value)
    }

    pub fn remove_edge_property(
        &self,
        handle: EdgeHandle,
        property_id: PropertyId,
    ) -> Option<Value> {
        self.commit_edge_property_remove(handle, property_id)
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

    pub(crate) fn edge_property_cursor(key: EdgePropertyKey) -> Vec<u8> {
        Storable::into_bytes(key)
    }

    pub(crate) fn scan_edge_properties_batch(
        &self,
        after_key: Option<Vec<u8>>,
        max_entries: u32,
    ) -> Result<Vec<(EdgePropertyKey, Value)>, String> {
        let after = match after_key {
            None => None,
            Some(bytes) => {
                if bytes.len() != 14 {
                    return Err("invalid edge property cursor key length".into());
                }
                Some(EdgePropertyKey::from_bytes(std::borrow::Cow::Borrowed(
                    &bytes,
                )))
            }
        };
        Ok(EDGE_PROPERTIES
            .with_borrow(|properties| properties.scan_properties_batch(after, max_entries)))
    }

    /// Scan canonical edge properties for indexed equality (no graph-index client).
    pub(crate) fn collect_edges_matching_indexed_property(
        property_id: PropertyId,
        expected: &[u8],
        label_id: Option<u16>,
    ) -> Vec<(ic_stable_lara::VertexId, u16, u32)> {
        use crate::index::registry;
        use crate::property::sortable_index_key;

        if !registry::is_edge_property_indexed(property_id) {
            return Vec::new();
        }
        let mut out = Vec::new();
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.for_each_property(|key, value| {
                if key.property_id() != property_id {
                    return;
                }
                if label_id.is_some_and(|label| key.label_id() != label) {
                    return;
                }
                let Some(bytes) = sortable_index_key(value) else {
                    return;
                };
                if bytes.as_slice() != expected {
                    return;
                }
                out.push((key.owner_vertex_id(), key.label_id(), key.slot_index()));
            });
        });
        out
    }

    /// Invoke `f` for each indexed property on an edge (for federated index removal enqueue).
    pub(crate) fn for_each_indexed_edge_property_on_edge(
        owner_vertex_id: ic_stable_lara::VertexId,
        label_id: u16,
        slot_index: u32,
        mut f: impl FnMut(PropertyId, Vec<u8>),
    ) {
        use crate::index::registry;
        use crate::property::sortable_index_key;

        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.for_each_property_for_edge(
                owner_vertex_id,
                label_id,
                slot_index,
                |pid, value| {
                    if !registry::is_edge_property_indexed(pid) {
                        return;
                    }
                    let Some(payload_bytes) = sortable_index_key(&value) else {
                        return;
                    };
                    f(pid, payload_bytes);
                },
            );
        });
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
