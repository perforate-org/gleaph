use crate::edge_ids::VertexEdgeIdAllocatorError;
use crate::label_catalog::LabelCatalogError;
use crate::property_catalog::PropertyCatalogError;
use crate::vertex_labels::VertexLabelStoreError;
use crate::vertex_properties::VertexPropertyStoreError;
use crate::{
    EDGE_PROPERTIES, GRAPH, LABEL_CATALOG, PROPERTY_CATALOG, VERTEX_EDGE_IDS, VERTEX_LABELS,
    VERTEX_PROPERTIES, memory,
};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{Edge, EdgeMeta, LabelId, PropertyId, Vertex, VertexEdgeId};
use ic_stable_lara::{
    DeferredBidirectionalLaraGraph as Graph, VertexCount, VertexId,
    bidirectional::DeferredBidirectionalLaraError,
};
use std::fmt;

/// Stateless facade over graph storage thread-locals.
///
/// `GraphStore` is the public coordination point for operations that need to
/// touch multiple stable structures in a consistent order. It intentionally
/// carries no fields; all state lives in the canister-local stable structures
/// initialized in `lib.rs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub vertex_edge_id: VertexEdgeId,
}

#[derive(Debug)]
pub enum GraphStoreError {
    Graph(DeferredBidirectionalLaraError),
    VertexEdgeId(VertexEdgeIdAllocatorError),
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::VertexEdgeId(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::VertexEdgeId(err) => Some(err),
        }
    }
}

impl From<DeferredBidirectionalLaraError> for GraphStoreError {
    fn from(value: DeferredBidirectionalLaraError) -> Self {
        Self::Graph(value)
    }
}

impl From<VertexEdgeIdAllocatorError> for GraphStoreError {
    fn from(value: VertexEdgeIdAllocatorError) -> Self {
        Self::VertexEdgeId(value)
    }
}

impl GraphStore {
    pub const fn new() -> Self {
        Self
    }

    pub fn label_id(&self, name: &str) -> Option<LabelId> {
        LABEL_CATALOG.with(|catalog| catalog.borrow().get_id(name))
    }

    pub fn label_name(&self, id: LabelId) -> Option<String> {
        LABEL_CATALOG.with(|catalog| catalog.borrow().get_name(id))
    }

    pub fn get_or_insert_label_id(&self, name: &str) -> Result<LabelId, LabelCatalogError> {
        LABEL_CATALOG.with(|catalog| catalog.borrow_mut().get_or_insert(name))
    }

    pub fn insert_label_with_id(&self, name: &str, id: LabelId) -> Result<(), LabelCatalogError> {
        LABEL_CATALOG.with(|catalog| catalog.borrow_mut().insert_with_id(name, id))
    }

    pub fn property_id(&self, name: &str) -> Option<PropertyId> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow().get_id(name))
    }

    pub fn property_name(&self, id: PropertyId) -> Option<String> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow().get_name(id))
    }

    pub fn get_or_insert_property_id(
        &self,
        name: &str,
    ) -> Result<PropertyId, PropertyCatalogError> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow_mut().get_or_insert(name))
    }

    pub fn insert_property_with_id(
        &self,
        name: &str,
        id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        PROPERTY_CATALOG.with(|catalog| catalog.borrow_mut().insert_with_id(name, id))
    }

    pub fn vertex_count(&self) -> VertexCount {
        GRAPH.with(|graph| graph.borrow().vertex_count())
    }

    pub fn insert_vertex(&self) -> Result<VertexId, DeferredBidirectionalLaraError> {
        self.insert_vertex_row(Vertex::default())
    }

    pub fn insert_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLaraError> {
        self.with_graph_mut(|graph| graph.push_vertex(vertex))
    }

    pub fn vertex(&self, vertex_id: VertexId) -> Option<Vertex> {
        if !self.contains_vertex(vertex_id) {
            return None;
        }
        GRAPH.with(|graph| Some(graph.borrow().forward().vertices().get(vertex_id)))
    }

    pub fn set_vertex(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), DeferredBidirectionalLaraError> {
        self.ensure_vertex_id(vertex_id)?;
        GRAPH.with(|graph| {
            let graph = graph.borrow();
            graph.forward().vertices().set(vertex_id, &vertex);
            graph.reverse().vertices().set(vertex_id, &vertex);
        });
        Ok(())
    }

    pub fn vertex_labels(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<LabelId> {
        VERTEX_LABELS.with(|labels| labels.borrow().labels_for(vertex_id, vertex))
    }

    pub fn set_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = LabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with(|store| store.borrow_mut().set_labels(vertex_id, vertex, labels))
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: LabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with(|store| store.borrow_mut().add_label(vertex_id, vertex, label))
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: LabelId,
    ) -> Vertex {
        VERTEX_LABELS.with(|store| store.borrow_mut().remove_label(vertex_id, vertex, label))
    }

    pub fn vertex_property(&self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow().get(vertex_id, property_id))
    }

    pub fn set_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        VERTEX_PROPERTIES
            .with(|properties| properties.borrow_mut().set(vertex_id, property_id, value))
    }

    pub fn remove_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
    ) -> Option<Value> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow_mut().remove(vertex_id, property_id))
    }

    pub fn vertex_properties(&self, vertex_id: VertexId) -> Vec<(PropertyId, Value)> {
        VERTEX_PROPERTIES.with(|properties| properties.borrow().properties_for(vertex_id))
    }

    pub fn edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow()
                .get(owner_vertex_id, vertex_edge_id, property_id)
        })
    }

    pub fn set_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow_mut()
                .set(owner_vertex_id, vertex_edge_id, property_id, value)
        })
    }

    pub fn remove_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow_mut()
                .remove(owner_vertex_id, vertex_edge_id, property_id)
        })
    }

    pub fn edge_properties(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Vec<(PropertyId, Value)> {
        EDGE_PROPERTIES.with(|properties| {
            properties
                .borrow()
                .properties_for_edge(owner_vertex_id, vertex_edge_id)
        })
    }

    pub fn allocate_vertex_edge_id(
        &self,
        owner_vertex_id: VertexId,
    ) -> Result<VertexEdgeId, VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_for_owner(owner_vertex_id))
    }

    pub fn allocate_directed_edge_id(
        &self,
        source_vertex_id: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_directed(source_vertex_id))
    }

    pub fn allocate_undirected_edge_id(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with(|ids| ids.borrow_mut().allocate_undirected(endpoint_a, endpoint_b))
    }

    pub fn insert_directed_edge(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        meta: EdgeMeta,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;

        let (owner_vertex_id, vertex_edge_id) = self.allocate_directed_edge_id(source_vertex_id)?;
        let edge = Edge {
            target: target_vertex_id,
            vertex_edge_id,
            meta: meta.with_undirected(false),
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_deferred(source_vertex_id, target_vertex_id, edge)
        })?;

        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    pub fn insert_undirected_edge(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        meta: EdgeMeta,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;

        let (owner_vertex_id, vertex_edge_id) =
            self.allocate_undirected_edge_id(endpoint_a, endpoint_b)?;
        let edge = Edge {
            target: endpoint_b,
            vertex_edge_id,
            meta: meta.with_undirected(true),
        };
        self.with_graph_mut(|graph| {
            graph.insert_undirected_deferred(endpoint_a, endpoint_b, edge)
        })?;

        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    pub fn out_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLaraError> {
        GRAPH.with(|graph| graph.borrow().collect_out_edges_slot_order(vertex_id))
    }

    pub fn in_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLaraError> {
        GRAPH.with(|graph| graph.borrow().collect_in_edges_slot_order(vertex_id))
    }

    fn contains_vertex(&self, vertex_id: VertexId) -> bool {
        u64::from(vertex_id) < u64::from(self.vertex_count())
    }

    fn ensure_vertex_id(&self, vertex_id: VertexId) -> Result<(), DeferredBidirectionalLaraError> {
        if self.contains_vertex(vertex_id) {
            Ok(())
        } else {
            Err(DeferredBidirectionalLaraError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        }
    }

    pub(crate) fn with_graph_mut<R>(
        &self,
        f: impl FnOnce(&mut Graph<Edge, Vertex, memory::Memory>) -> R,
    ) -> R {
        GRAPH.with(|graph| f(&mut graph.borrow_mut()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_vertices_and_edges_through_facade() {
        let store = GraphStore::new();
        let start: u64 = store.vertex_count().into();
        let source = store.insert_vertex().expect("insert source vertex");
        let target = store.insert_vertex().expect("insert target vertex");

        assert_eq!(source, VertexId::from(start as u32));
        assert_eq!(target, VertexId::from(start as u32 + 1));

        let directed = store
            .insert_directed_edge(source, target, EdgeMeta::default())
            .expect("insert directed edge");

        assert_eq!(directed.owner_vertex_id, source);
        assert_eq!(directed.vertex_edge_id, VertexEdgeId::from_raw(1));

        let out_edges = store.out_edges(source).expect("read out edges");
        assert!(out_edges.iter().any(|edge| {
            edge.target == target
                && edge.vertex_edge_id == directed.vertex_edge_id
                && !edge.meta.is_undirected()
        }));

        let undirected = store
            .insert_undirected_edge(target, source, EdgeMeta::default())
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, source);
        assert_eq!(undirected.vertex_edge_id, VertexEdgeId::from_raw(2));

        let target_out_edges = store.out_edges(target).expect("read target out edges");
        assert!(target_out_edges.iter().any(|edge| {
            edge.target == source
                && edge.vertex_edge_id == undirected.vertex_edge_id
                && edge.meta.is_undirected()
        }));
    }
}
