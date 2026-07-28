//! GraphStore `edge_properties` implementation.

use super::super::stable::EDGE_PROPERTIES;
use super::super::stable::edge_properties::EdgePropertyKey;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::labeled::CanonicalEdgeOccurrence;
use ic_stable_structures::Storable;

use super::GraphStore;

impl GraphStore {
    /// Reads a sidecar entry for a handle already known to be canonical.
    /// Hot query paths use this to avoid repeating CounterpartScan.
    pub(crate) fn edge_property_at_canonical_handle(
        &self,
        handle: super::handle::EdgeHandle,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
            )
        })
    }

    pub fn edge_property(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        Ok(EDGE_PROPERTIES.with_borrow(|properties| {
            properties.get(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                property_id,
            )
        }))
    }

    pub fn set_edge_property(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        self.commit_edge_property_write(occurrence, property_id, value)
    }

    pub fn remove_edge_property(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        property_id: PropertyId,
    ) -> Result<Option<Value>, super::error::GraphStoreError> {
        self.commit_edge_property_remove(occurrence, property_id)
    }

    pub fn edge_properties(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<Vec<(PropertyId, Value)>, super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        Ok(EDGE_PROPERTIES.with_borrow(|properties| {
            properties.properties_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
            )
        }))
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
        use crate::index::catalog_context;
        use crate::property::sortable_index_key;

        if !catalog_context::is_edge_property_indexed(property_id) {
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
        use crate::index::catalog_context;
        use crate::property::sortable_index_key;

        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.for_each_property_for_edge(
                owner_vertex_id,
                label_id,
                slot_index,
                |pid, value| {
                    if !catalog_context::is_edge_property_indexed(pid) {
                        return;
                    }
                    let Some(inline_property_bytes) = sortable_index_key(&value) else {
                        return;
                    };
                    f(pid, inline_property_bytes);
                },
            );
        });
    }

    pub(crate) fn edge_properties_gql_record(
        &self,
        occurrence: CanonicalEdgeOccurrence,
    ) -> Result<Value, super::error::GraphStoreError> {
        let handle = self.canonical_edge_handle_from_occurrence(occurrence)?;
        Ok(EDGE_PROPERTIES.with_borrow(|properties| {
            let mut fields: Vec<(String, Value)> = Vec::new();
            properties.for_each_property_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
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
        }))
    }

    pub(crate) fn edge_properties_gql_record_at_canonical_handle(
        &self,
        handle: super::handle::EdgeHandle,
    ) -> Value {
        EDGE_PROPERTIES.with_borrow(|properties| {
            let mut fields = Vec::new();
            properties.for_each_property_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index.raw(),
                |property_id, value| {
                    let name = self
                        .property_name(property_id)
                        .unwrap_or_else(|| property_id.raw().to_string());
                    fields.push((name, value));
                },
            );
            Value::Record(fields)
        })
    }
}
