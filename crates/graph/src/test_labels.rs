//! Deterministic label ids for graph tests and benchmarks.
//!
//! Production label names are resolved by the router. Tests and benchmarks often build fixtures
//! directly in `gleaph-graph`, so they use deterministic name-derived ids without stable storage.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadProfile, PropertyId, VertexLabelId};
use ic_stable_lara::VertexId;

static EDGE_LABEL_NAMES: Mutex<Option<HashMap<u16, String>>> = Mutex::new(None);
static EDGE_PAYLOAD_PROFILES: Mutex<Option<HashMap<u16, EdgePayloadProfile>>> = Mutex::new(None);
static EDGE_INLINE_PROPERTIES: Mutex<Option<HashMap<u16, PropertyId>>> = Mutex::new(None);

pub(crate) fn install_test_edge_payload_profile(label: EdgeLabelId, profile: EdgePayloadProfile) {
    EDGE_PAYLOAD_PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(label.raw(), profile);
}

pub(crate) fn edge_payload_profile_for_id(label: EdgeLabelId) -> Option<EdgePayloadProfile> {
    EDGE_PAYLOAD_PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&label.raw())
        .cloned()
}

pub(crate) fn install_test_edge_inline_property(label: EdgeLabelId, property_id: PropertyId) {
    EDGE_INLINE_PROPERTIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(label.raw(), property_id);
}

pub(crate) fn edge_inline_property_for_id(label: EdgeLabelId) -> Option<PropertyId> {
    EDGE_INLINE_PROPERTIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&label.raw())
        .copied()
}

pub(crate) fn edge_label_ids_with_payload_profiles() -> Vec<EdgeLabelId> {
    EDGE_PAYLOAD_PROFILES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|profiles| {
            profiles
                .iter()
                .filter(|(_, profile)| profile.required_byte_width() > 0)
                .map(|(id, _)| EdgeLabelId::from_raw(*id))
                .collect()
        })
        .unwrap_or_default()
}

impl GraphStore {
    pub(crate) fn insert_vertex_named(
        &self,
        labels: impl IntoIterator<Item = impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<VertexId, GraphStoreError> {
        let labels = labels
            .into_iter()
            .map(|label| vertex_label_id_for_name(label.as_ref()))
            .collect::<Vec<_>>();
        let properties = self.resolve_named_test_properties(properties)?;
        self.insert_vertex_with(labels, properties)
    }

    pub(crate) fn insert_directed_edge_named(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let label = label.map(|label| edge_label_id_for_name(label.as_ref()));
        let properties = self.resolve_named_test_properties(properties)?;
        self.insert_directed_edge_with(source_vertex_id, target_vertex_id, label, properties)
    }

    #[cfg_attr(not(test), expect(dead_code, reason = "test fixture helper"))]
    pub(crate) fn insert_undirected_edge_named(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        label: Option<impl AsRef<str>>,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        let label = label.map(|label| edge_label_id_for_name(label.as_ref()));
        let properties = self.resolve_named_test_properties(properties)?;
        self.insert_undirected_edge_with(endpoint_a, endpoint_b, label, properties)
    }

    fn resolve_named_test_properties(
        &self,
        properties: impl IntoIterator<Item = (impl AsRef<str>, Value)>,
    ) -> Result<Vec<(PropertyId, Value)>, GraphStoreError> {
        Ok(properties
            .into_iter()
            .map(|(name, value)| (property_id_for_name(name.as_ref()), value))
            .collect())
    }
}

pub(crate) fn property_id_for_name(name: &str) -> PropertyId {
    PropertyId::from_raw(nonzero_hash_u32(name))
}

#[cfg(test)]
pub(crate) fn enter_indexed_edge_property_named(
    name: &str,
) -> crate::index::catalog_context::CatalogGuard {
    crate::index::catalog_context::enter_edge_indexed(&[property_id_for_name(name)])
}

fn nonzero_hash_u32(name: &str) -> u32 {
    let raw = stable_hash(name) as u32;
    if raw == 0 { 1 } else { raw }
}

pub(crate) fn vertex_label_id_for_name(name: &str) -> VertexLabelId {
    VertexLabelId::from_raw(nonzero_hash_u16(name))
}

pub(crate) fn edge_label_id_for_name(name: &str) -> EdgeLabelId {
    let id = EdgeLabelId::from_raw(1 + (stable_hash(name) % 0x7ffe) as u16);
    EDGE_LABEL_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(id.raw(), name.to_owned());
    id
}

pub(crate) fn edge_label_name_for_id(id: EdgeLabelId) -> Option<String> {
    EDGE_LABEL_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&id.raw())
        .cloned()
}

fn nonzero_hash_u16(name: &str) -> u16 {
    let raw = stable_hash(name) as u16;
    if raw == 0 { 1 } else { raw }
}

fn stable_hash(name: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
