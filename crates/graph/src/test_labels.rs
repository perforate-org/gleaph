//! Deterministic label ids for graph tests and benchmarks.
//!
//! Production label names are resolved by the router. Tests and benchmarks often build fixtures
//! directly in `gleaph-graph`, so they use deterministic name-derived ids without stable storage.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::facade::{EdgeHandle, GraphStore, GraphStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
use ic_stable_lara::VertexId;

static EDGE_LABEL_NAMES: Mutex<Option<HashMap<u16, String>>> = Mutex::new(None);

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
        properties
            .into_iter()
            .map(|(name, value)| {
                self.get_or_insert_property_id(name.as_ref())
                    .map(|id| (id, value))
                    .map_err(GraphStoreError::from)
            })
            .collect()
    }
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
