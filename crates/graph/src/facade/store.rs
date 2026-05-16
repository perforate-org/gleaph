use super::stable::edge_ids::canonical_undirected_owner;
use super::stable::edge_label_catalog::EdgeLabelCatalogError;
use super::stable::edge_weight_profiles::EdgeWeightProfileStoreError;
use super::stable::memory::StableGraph;
use super::stable::vertex_label_catalog::VertexLabelCatalogError;
use super::stable::{
    EDGE_LABEL_CATALOG, EDGE_PROPERTIES, EDGE_WEIGHT_PROFILES, GRAPH, GRAPH_DEFAULT_EDGE_LABEL,
    METADATA, PREPARED_QUERY_CATALOG, PROPERTY_CATALOG, VERTEX_EDGE_IDS, VERTEX_LABEL_CATALOG,
    VERTEX_LABELS, VERTEX_PROPERTIES,
};
use super::{
    GraphMetadata, GraphMetadataError, IndexRouting, PropertyCatalogError,
    VertexEdgeIdAllocatorError, VertexLabelStoreError, VertexPropertyStoreError,
};
use crate::index::{edge_equal, pending};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    Edge, EdgeLabelId, EdgeWeightProfile, PropertyId, Vertex, VertexEdgeId, VertexLabelId,
    VertexRef,
};
use gleaph_graph_prepared::{PreparedQueryError, PreparedQueryRecord};
use ic_stable_lara::{
    DeferredBidirectionalLabeledError, LabelId as LaraLabelId, MaintenanceBudget, VertexCount,
    VertexId, labeled::LabeledBidirectionalMaintenanceReport, traits::CsrEdge,
};
use std::fmt;

/// Stateless facade over graph storage thread-locals.
///
/// `GraphStore` is the public coordination point for operations that need to
/// touch multiple stable structures in a consistent order. It intentionally
/// carries no fields; all state lives in the canister-local stable structures
/// initialized in [`super::stable`].
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;

fn edge_storage_label(catalog: Option<EdgeLabelId>, undirected: bool) -> EdgeLabelId {
    match catalog {
        None => {
            if undirected {
                EdgeLabelId::UNLABELED_UNDIRECTED
            } else {
                EdgeLabelId::UNLABELED_DIRECTED
            }
        }
        Some(catalog_id) => EdgeLabelId::from_catalog(catalog_id, undirected),
    }
}

fn lara_label(id: EdgeLabelId) -> LaraLabelId {
    LaraLabelId::from_raw(id.raw())
}

fn build_edge_to(target: VertexId, vertex_edge_id: VertexEdgeId, inline_value: u16) -> Edge {
    Edge {
        target: VertexRef::local(target),
        vertex_edge_id,
        inline_value,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub vertex_edge_id: VertexEdgeId,
}

#[derive(Debug)]
pub enum GraphStoreError {
    Graph(DeferredBidirectionalLabeledError),
    VertexEdgeId(VertexEdgeIdAllocatorError),
    VertexLabelCatalog(VertexLabelCatalogError),
    EdgeLabelCatalog(EdgeLabelCatalogError),
    EdgeWeightProfile(EdgeWeightProfileStoreError),
    PropertyCatalog(PropertyCatalogError),
    VertexLabel(VertexLabelStoreError),
    PropertyValue(VertexPropertyStoreError),
    /// `DELETE` vertex without `DETACH` while the vertex still has incident edges.
    VertexNotDetached {
        vertex_id: VertexId,
    },
    /// No outgoing edge record matches the handle on the owner's forward row.
    EdgeNotFound {
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    },
    /// Edge label id is outside the inline edge band `0x0001..=0x3FFF`.
    InvalidEdgeLabelId(EdgeLabelId),
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::VertexEdgeId(err) => write!(f, "{err}"),
            Self::VertexLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeWeightProfile(err) => write!(f, "{err}"),
            Self::PropertyCatalog(err) => write!(f, "{err}"),
            Self::VertexLabel(err) => write!(f, "{err}"),
            Self::PropertyValue(err) => write!(f, "{err}"),
            Self::VertexNotDetached { vertex_id } => write!(
                f,
                "cannot delete vertex {vertex_id:?} without DETACH while it still has incident edges"
            ),
            Self::EdgeNotFound {
                owner_vertex_id,
                vertex_edge_id,
            } => write!(
                f,
                "no edge record for owner {owner_vertex_id:?} and local edge id {vertex_edge_id:?}"
            ),
            Self::InvalidEdgeLabelId(id) => write!(
                f,
                "edge label id {} is not a catalog edge label (MSB clear, non-zero)",
                id.raw()
            ),
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::VertexEdgeId(err) => Some(err),
            Self::VertexLabelCatalog(err) => Some(err),
            Self::EdgeLabelCatalog(err) => Some(err),
            Self::EdgeWeightProfile(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_) => None,
        }
    }
}

impl From<DeferredBidirectionalLabeledError> for GraphStoreError {
    fn from(value: DeferredBidirectionalLabeledError) -> Self {
        Self::Graph(value)
    }
}

impl From<VertexEdgeIdAllocatorError> for GraphStoreError {
    fn from(value: VertexEdgeIdAllocatorError) -> Self {
        Self::VertexEdgeId(value)
    }
}

impl From<VertexLabelCatalogError> for GraphStoreError {
    fn from(value: VertexLabelCatalogError) -> Self {
        Self::VertexLabelCatalog(value)
    }
}

impl From<EdgeLabelCatalogError> for GraphStoreError {
    fn from(value: EdgeLabelCatalogError) -> Self {
        Self::EdgeLabelCatalog(value)
    }
}

impl From<EdgeWeightProfileStoreError> for GraphStoreError {
    fn from(value: EdgeWeightProfileStoreError) -> Self {
        Self::EdgeWeightProfile(value)
    }
}

impl From<PropertyCatalogError> for GraphStoreError {
    fn from(value: PropertyCatalogError) -> Self {
        Self::PropertyCatalog(value)
    }
}

impl From<VertexLabelStoreError> for GraphStoreError {
    fn from(value: VertexLabelStoreError) -> Self {
        Self::VertexLabel(value)
    }
}

impl From<VertexPropertyStoreError> for GraphStoreError {
    fn from(value: VertexPropertyStoreError) -> Self {
        Self::PropertyValue(value)
    }
}

impl GraphStore {
    pub const fn new() -> Self {
        Self
    }

    pub fn set_metadata(&self, metadata: GraphMetadata) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| m.set(metadata))
    }

    pub fn logical_graph_name(&self) -> Option<String> {
        METADATA.with_borrow(|m| m.get().logical_graph_name())
    }

    pub fn set_logical_graph_name(&self, name: Option<String>) -> Result<(), GraphMetadataError> {
        if let Some(name) = &name {
            GraphMetadata::validate_name(name)?;
        }
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            metadata.set_logical_graph_name(name);
            m.set(metadata)
        })
    }

    pub fn index_routing(&self) -> Option<IndexRouting> {
        METADATA.with_borrow(|m| m.get().index_routing())
    }

    pub fn set_index_routing(
        &self,
        index_routing: Option<IndexRouting>,
    ) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            metadata.set_index_routing(index_routing);
            m.set(metadata)
        })
    }

    pub fn index_configured(&self) -> bool {
        METADATA.with_borrow(|m| m.get().index_configured())
    }

    pub fn vertex_label_id(&self, name: &str) -> Option<VertexLabelId> {
        VERTEX_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn edge_label_id(&self, name: &str) -> Option<EdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn vertex_label_name(&self, id: VertexLabelId) -> Option<String> {
        VERTEX_LABEL_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn edge_label_name(&self, id: EdgeLabelId) -> Option<String> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn get_or_insert_vertex_label_id(
        &self,
        name: &str,
    ) -> Result<VertexLabelId, VertexLabelCatalogError> {
        VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

    pub fn get_or_insert_edge_label_id(
        &self,
        name: &str,
    ) -> Result<EdgeLabelId, EdgeLabelCatalogError> {
        EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

    pub(crate) fn edge_is_undirected(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        let bucket = self
            .find_forward_edge_bucket_label(owner_vertex_id, edge)?
            .unwrap_or(GRAPH_DEFAULT_EDGE_LABEL);
        Ok(EdgeLabelId::from_raw(bucket.raw()).is_undirected())
    }

    pub fn insert_vertex_label_with_id(
        &self,
        name: &str,
        id: VertexLabelId,
    ) -> Result<(), VertexLabelCatalogError> {
        VERTEX_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }

    pub fn insert_edge_label_with_id(
        &self,
        name: &str,
        id: EdgeLabelId,
    ) -> Result<(), EdgeLabelCatalogError> {
        EDGE_LABEL_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }

    pub fn set_edge_label_weight_profile(
        &self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), GraphStoreError> {
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        Ok(())
    }

    pub fn edge_label_weight_profile(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        EDGE_WEIGHT_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn remove_edge_label_weight_profile(&self, label: EdgeLabelId) {
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    pub fn property_id(&self, name: &str) -> Option<PropertyId> {
        PROPERTY_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn property_name(&self, id: PropertyId) -> Option<String> {
        PROPERTY_CATALOG.with_borrow(|catalog| catalog.get_name(id))
    }

    pub fn get_or_insert_property_id(
        &self,
        name: &str,
    ) -> Result<PropertyId, PropertyCatalogError> {
        PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.get_or_insert(name))
    }

    pub fn insert_property_with_id(
        &self,
        name: &str,
        id: PropertyId,
    ) -> Result<(), PropertyCatalogError> {
        PROPERTY_CATALOG.with_borrow_mut(|catalog| catalog.insert_with_id(name, id))
    }

    pub fn prepared_query_register(
        &self,
        name: String,
        source: &str,
    ) -> Result<(), PreparedQueryError> {
        PREPARED_QUERY_CATALOG.with_borrow_mut(|c| c.register(name, source))
    }

    pub fn prepared_query_drop(&self, name: &str) {
        PREPARED_QUERY_CATALOG.with_borrow_mut(|c| {
            c.remove(name);
        });
    }

    pub fn prepared_query_get(&self, name: &str) -> Option<PreparedQueryRecord> {
        PREPARED_QUERY_CATALOG.with_borrow(|c| c.get(name))
    }

    pub fn prepared_query_contains(&self, name: &str) -> bool {
        PREPARED_QUERY_CATALOG.with_borrow(|c| c.contains_key(name))
    }

    pub fn vertex_count(&self) -> VertexCount {
        GRAPH.with_borrow(|graph| graph.vertex_count())
    }

    pub fn insert_vertex(&self) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        self.insert_vertex_row(Vertex::default())
    }

    pub fn insert_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        self.with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
    }

    pub fn vertex(&self, vertex_id: VertexId) -> Option<Vertex> {
        GRAPH.with_borrow(|graph| graph.vertex_row(vertex_id).ok().map(Vertex::from))
    }

    pub fn set_vertex(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        let row = vertex.into();
        GRAPH.with_borrow(|graph| graph.set_vertex_row(vertex_id, &row))
    }

    pub fn vertex_labels(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<VertexLabelId> {
        VERTEX_LABELS.with_borrow(|labels| labels.labels_for(vertex_id, vertex))
    }

    /// GQL `labels` list values without allocating an intermediate `Vec<VertexLabelId>`.
    pub(crate) fn vertex_label_gql_list(&self, vertex_id: VertexId, vertex: Vertex) -> Vec<Value> {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| {
                let mut out = Vec::with_capacity(slice.len());
                for &label in slice {
                    out.push(
                        self.vertex_label_name(label)
                            .map(Value::Text)
                            .unwrap_or_else(|| Value::Uint64(u64::from(label.raw()))),
                    );
                }
                out
            })
        })
    }

    /// Whether `vertex` has `label_id`, using an inline primary-label check when there is no
    /// multi-label sidecar (avoids an allocation per lookup).
    #[inline]
    pub fn vertex_has_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label_id: VertexLabelId,
    ) -> bool {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| slice.contains(&label_id))
        })
    }

    pub fn set_vertex_labels(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        labels: impl IntoIterator<Item = VertexLabelId>,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with_borrow_mut(|store| store.set_labels(vertex_id, vertex, labels))
    }

    pub fn add_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Result<Vertex, VertexLabelStoreError> {
        VERTEX_LABELS.with_borrow_mut(|store| store.add_label(vertex_id, vertex, label))
    }

    pub fn remove_vertex_label(
        &self,
        vertex_id: VertexId,
        vertex: Vertex,
        label: VertexLabelId,
    ) -> Vertex {
        VERTEX_LABELS.with_borrow_mut(|store| store.remove_label(vertex_id, vertex, label))
    }

    pub fn vertex_property(&self, vertex_id: VertexId, property_id: PropertyId) -> Option<Value> {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.get(vertex_id, property_id))
    }

    pub fn set_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        let prev =
            VERTEX_PROPERTIES.with_borrow(|properties| properties.get(vertex_id, property_id));
        let out = VERTEX_PROPERTIES
            .with_borrow_mut(|properties| properties.set(vertex_id, property_id, value.clone()))?;
        pending::record_vertex_property_change(vertex_id, property_id, prev.as_ref(), Some(&value));
        Ok(out)
    }

    pub fn remove_vertex_property(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
    ) -> Option<Value> {
        let removed = VERTEX_PROPERTIES
            .with_borrow_mut(|properties| properties.remove(vertex_id, property_id));
        if let Some(ref old) = removed {
            pending::record_vertex_property_change(vertex_id, property_id, Some(old), None);
        }
        removed
    }

    pub fn vertex_properties(&self, vertex_id: VertexId) -> Vec<(PropertyId, Value)> {
        VERTEX_PROPERTIES.with_borrow(|properties| properties.properties_for(vertex_id))
    }

    pub fn edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        EDGE_PROPERTIES
            .with_borrow(|properties| properties.get(owner_vertex_id, vertex_edge_id, property_id))
    }

    pub fn set_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        let prev = EDGE_PROPERTIES
            .with_borrow(|properties| properties.get(owner_vertex_id, vertex_edge_id, property_id));
        let old = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            properties.set(owner_vertex_id, vertex_edge_id, property_id, value.clone())
        })?;
        edge_equal::record_edge_property_change(
            owner_vertex_id,
            vertex_edge_id,
            property_id,
            prev.as_ref(),
            Some(&value),
        );
        Ok(old)
    }

    pub fn remove_edge_property(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        let prev = EDGE_PROPERTIES
            .with_borrow(|properties| properties.get(owner_vertex_id, vertex_edge_id, property_id));
        let removed = EDGE_PROPERTIES.with_borrow_mut(|properties| {
            properties.remove(owner_vertex_id, vertex_edge_id, property_id)
        });
        if let Some(ref old) = prev {
            edge_equal::record_edge_property_change(
                owner_vertex_id,
                vertex_edge_id,
                property_id,
                Some(old),
                None,
            );
        }
        removed
    }

    pub fn edge_properties(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Vec<(PropertyId, Value)> {
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.properties_for_edge(owner_vertex_id, vertex_edge_id)
        })
    }

    pub fn allocate_vertex_edge_id(
        &self,
        owner_vertex_id: VertexId,
    ) -> Result<VertexEdgeId, VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with_borrow_mut(|ids| ids.allocate_for_owner(owner_vertex_id))
    }

    pub fn allocate_directed_edge_id(
        &self,
        source_vertex_id: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with_borrow_mut(|ids| ids.allocate_directed(source_vertex_id))
    }

    pub fn allocate_undirected_edge_id(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
    ) -> Result<(VertexId, VertexEdgeId), VertexEdgeIdAllocatorError> {
        VERTEX_EDGE_IDS.with_borrow_mut(|ids| ids.allocate_undirected(endpoint_a, endpoint_b))
    }

    fn validate_catalog_edge_label(label: Option<EdgeLabelId>) -> Result<(), GraphStoreError> {
        if let Some(id) = label {
            if id.is_undirected() || (id.raw() != 0 && !id.is_catalog_allocatable()) {
                return Err(GraphStoreError::InvalidEdgeLabelId(id));
            }
        }
        Ok(())
    }

    pub fn insert_directed_edge(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let (owner_vertex_id, vertex_edge_id) = self.allocate_directed_edge_id(source_vertex_id)?;
        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to(target_vertex_id, vertex_edge_id, 0);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            vertex_edge_id,
            inline_value: forward.inline_value,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    /// Test/canbench helper to insert a directed edge with a specific `inline_value`.
    #[cfg(any(test, feature = "canbench"))]
    pub fn insert_directed_edge_with_inline_value(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_value: u16,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let (owner_vertex_id, vertex_edge_id) = self.allocate_directed_edge_id(source_vertex_id)?;
        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to(target_vertex_id, vertex_edge_id, inline_value);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            vertex_edge_id,
            inline_value: forward.inline_value,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
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
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let (owner_vertex_id, vertex_edge_id) =
            self.allocate_undirected_edge_id(endpoint_a, endpoint_b)?;
        let label = lara_label(edge_storage_label(catalog_label, true));
        let edge_ab = build_edge_to(endpoint_b, vertex_edge_id, 0);
        let edge_ba = build_edge_to(endpoint_a, vertex_edge_id, 0);
        self.with_graph_mut(|graph| {
            graph.insert_undirected_deferred(endpoint_a, endpoint_b, label, edge_ab, edge_ba)
        })?;
        Ok(EdgeHandle {
            owner_vertex_id,
            vertex_edge_id,
        })
    }

    pub fn out_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.collect_out_edges_slot_order(vertex_id))
    }

    pub fn in_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.collect_in_edges_slot_order(vertex_id))
    }

    pub(crate) fn out_edges_for_label(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.iter_out_edges_for_label(vertex_id, label))
    }

    pub(crate) fn for_each_out_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| graph.for_each_out_edges_for_label(vertex_id, label, visit))
    }

    pub(crate) fn for_each_out_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = canbench_rs::bench_scope("graph_store_tls_out_label_unchecked");
        GRAPH.with_borrow(|graph| {
            graph.for_each_out_edges_for_label_unchecked(vertex_id, label, visit)
        })
    }

    pub(crate) fn in_edges_for_label(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.iter_in_edges_for_label(vertex_id, label))
    }

    pub(crate) fn for_each_in_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| graph.for_each_in_edges_for_label(vertex_id, label, visit))
    }

    pub(crate) fn for_each_in_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        #[cfg(all(feature = "canbench", target_family = "wasm"))]
        let _scope = canbench_rs::bench_scope("graph_store_tls_in_label_unchecked");
        GRAPH.with_borrow(|graph| {
            graph.for_each_in_edges_for_label_unchecked(vertex_id, label, visit)
        })
    }

    pub(crate) fn find_forward_edge_bucket_label(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<Option<LaraLabelId>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.find_forward_edge_label(owner_vertex_id, edge))
    }

    /// Scans outgoing edges without materializing the full CSR row.
    pub fn for_each_out_edge_matching<Match, Visit>(
        &self,
        vertex_id: VertexId,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        self.for_each_out_edge_matching_with_raw(
            vertex_id,
            None::<&mut dyn FnMut(&[u8]) -> bool>,
            matches,
            visit,
        )
    }

    /// Like [`Self::for_each_out_edge_matching`] with an optional slab raw-byte prefilter.
    pub fn for_each_out_edge_matching_with_raw<Match, Visit>(
        &self,
        vertex_id: VertexId,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_out_edge_matching_with_raw(vertex_id, raw_matches, matches, visit)
        })
    }

    /// Scans incoming edges without materializing the full CSR row.
    pub fn for_each_in_edge_matching<Match, Visit>(
        &self,
        vertex_id: VertexId,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        self.for_each_in_edge_matching_with_raw(
            vertex_id,
            None::<&mut dyn FnMut(&[u8]) -> bool>,
            matches,
            visit,
        )
    }

    /// Like [`Self::for_each_in_edge_matching`] with an optional slab raw-byte prefilter.
    pub fn for_each_in_edge_matching_with_raw<Match, Visit>(
        &self,
        vertex_id: VertexId,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_in_edge_matching_with_raw(vertex_id, raw_matches, matches, visit)
        })
    }

    /// Runs deferred LARA maintenance until the queue is empty or the budget is exhausted.
    ///
    /// Production canisters should use a tight instruction budget and rely on
    /// heartbeat/timer draining; tests and small graphs typically pass
    /// `MaintenanceBudget { max_instructions: 0, .. }` to disable the instruction cap.
    ///
    /// For timer-driven draining with a conservative cap under the ICP per-message limit,
    /// prefer [`Self::run_timer_maintenance_tick`].
    ///
    /// See `docs/ic-timer-maintenance-strategy.md` for the intended canister maintenance model.
    pub fn run_maintenance_best_effort(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        GRAPH
            .with_borrow(|graph| graph.maintenance(budget))
            .map_err(GraphStoreError::from)
    }

    /// Runs one **budgeted** LARA maintenance pass for timer/heartbeat loops.
    ///
    /// Uses [`timer_lara_maintenance_budget`](crate::facade::timer_lara_maintenance_budget),
    /// aligned with the ICP per-message instruction ceiling documented at
    /// <https://docs.internetcomputer.org/references/cycles-costs/#resource-limits>.
    /// Call again on later timer ticks while the returned report's
    /// `remaining_queue_len()` is non-zero, or when a prior budgeted run set
    /// `instruction_budget_exhausted` and work may remain.
    ///
    /// Mutation paths that must finish deferred work in the same message should
    /// keep using the internal full drain (`max_instructions: 0`) instead.
    pub fn run_timer_maintenance_tick(
        &self,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// `DELETE` semantics: remove the vertex only when it has no incident edges.
    pub fn delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    /// `DETACH DELETE` semantics: remove all incident edges, then delete the vertex.
    ///
    /// Incident edges are cleared via LARA's queued incremental `delete_vertex_deferred`
    /// maintenance; stable edge property sidecars are cleared as each edge is removed.
    pub fn detach_delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;

        let mut to_clear: Vec<(VertexId, VertexEdgeId)> = Vec::new();
        for e in self.out_edges(vertex_id).map_err(GraphStoreError::from)? {
            to_clear.push((
                self.edge_sidecar_owner_from_out_row(vertex_id, &e),
                e.vertex_edge_id,
            ));
        }
        for e in self.in_edges(vertex_id).map_err(GraphStoreError::from)? {
            to_clear.push((
                self.edge_sidecar_owner_from_in_row(vertex_id, &e),
                e.vertex_edge_id,
            ));
        }
        to_clear.sort_unstable();
        to_clear.dedup();

        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        for (owner, veid) in to_clear {
            Self::clear_edge_properties_stable(owner, veid);
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    /// Removes one logical edge (and its stable properties) identified by `handle`.
    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.ensure_vertex_id(handle.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let edge = self
            .find_outgoing_edge_record(handle.owner_vertex_id, handle.vertex_edge_id)?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                vertex_edge_id: handle.vertex_edge_id,
            })?;
        Self::clear_edge_properties_stable(handle.owner_vertex_id, handle.vertex_edge_id);
        let neighbor = edge.neighbor_vid();
        let removed = if self.edge_is_undirected(handle.owner_vertex_id, &edge)? {
            self.with_graph_mut(|graph| {
                graph.remove_undirected_deferred(handle.owner_vertex_id, neighbor, edge)
            })?
        } else {
            self.with_graph_mut(|graph| {
                graph.remove_directed_deferred(handle.owner_vertex_id, neighbor, edge)
            })?
        };
        if !removed {
            return Err(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                vertex_edge_id: handle.vertex_edge_id,
            });
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    fn drain_deferred_maintenance(&self) -> Result<(), GraphStoreError> {
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        };
        self.run_maintenance_best_effort(budget)?;
        Ok(())
    }

    fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.has_incident_edges(vertex_id))
    }

    fn edge_sidecar_owner_from_out_row(&self, endpoint: VertexId, edge: &Edge) -> VertexId {
        if self.edge_is_undirected(endpoint, edge).unwrap_or(false) {
            canonical_undirected_owner(endpoint, edge.neighbor_vid())
        } else {
            endpoint
        }
    }

    fn edge_sidecar_owner_from_in_row(&self, dst: VertexId, edge: &Edge) -> VertexId {
        if self.edge_is_undirected(dst, edge).unwrap_or(false) {
            canonical_undirected_owner(dst, edge.neighbor_vid())
        } else {
            edge.neighbor_vid()
        }
    }

    fn clear_edge_properties_stable(owner_vertex_id: VertexId, vertex_edge_id: VertexEdgeId) {
        edge_equal::remove_all_for_edge(owner_vertex_id, vertex_edge_id);
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(owner_vertex_id, vertex_edge_id);
        });
    }

    fn clear_vertex_properties_stable_only(&self, vertex_id: VertexId) {
        let props: Vec<PropertyId> = VERTEX_PROPERTIES.with_borrow(|store| {
            store
                .properties_for(vertex_id)
                .into_iter()
                .map(|(pid, _)| pid)
                .collect()
        });
        for pid in props {
            let _ = self.remove_vertex_property(vertex_id, pid);
        }
    }

    fn clear_vertex_stable_payloads_before_graph_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.clear_vertex_properties_stable_only(vertex_id);

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        // Label sidecars live in `VERTEX_LABELS`; the CSR row is unchanged. Do not call
        // `set_vertex` here: it mirrors the forward row into reverse and would corrupt
        // reverse-only locator state for this `VertexId`.
        let _ = VERTEX_LABELS.with_borrow_mut(|labels| {
            labels
                .set_labels(vertex_id, vertex, [])
                .map_err(GraphStoreError::from)
        })?;
        Ok(())
    }

    fn find_outgoing_edge_record(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Result<Option<Edge>, GraphStoreError> {
        let edges = self
            .out_edges(owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        Ok(edges
            .into_iter()
            .find(|candidate| candidate.vertex_edge_id == vertex_edge_id))
    }

    fn contains_vertex(&self, vertex_id: VertexId) -> bool {
        u32::from(vertex_id) < u32::from(self.vertex_count())
    }

    fn ensure_vertex_id(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), DeferredBidirectionalLabeledError> {
        if self.contains_vertex(vertex_id) {
            Ok(())
        } else {
            Err(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        }
    }

    pub(crate) fn with_graph_mut<R>(&self, f: impl FnOnce(&mut StableGraph) -> R) -> R {
        GRAPH.with_borrow_mut(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_vertices_and_edges_through_facade() {
        let store = GraphStore::new();
        let start: u32 = store.vertex_count().into();
        let source = store.insert_vertex().expect("insert source vertex");
        let target = store.insert_vertex().expect("insert target vertex");

        assert_eq!(source, VertexId::from(start));
        assert_eq!(target, VertexId::from(start + 1));

        let directed = store
            .insert_directed_edge(source, target, None)
            .expect("insert directed edge");

        assert_eq!(directed.owner_vertex_id, source);
        assert_eq!(directed.vertex_edge_id, VertexEdgeId::from_raw(1));

        let out_edges = store.out_edges(source).expect("read out edges");
        assert!(out_edges.iter().any(|edge| {
            edge.target == VertexRef::local(target)
                && edge.vertex_edge_id == directed.vertex_edge_id
                && !store.edge_is_undirected(source, edge).unwrap()
        }));

        let undirected = store
            .insert_undirected_edge(target, source, None)
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, target);
        assert_eq!(undirected.vertex_edge_id, VertexEdgeId::from_raw(1));

        let target_out_edges = store.out_edges(target).expect("read target out edges");
        assert!(target_out_edges.iter().any(|edge| {
            edge.target == VertexRef::local(source)
                && edge.vertex_edge_id == undirected.vertex_edge_id
                && store.edge_is_undirected(target, edge).unwrap()
        }));
    }

    #[test]
    fn timer_maintenance_tick_runs_on_empty_graph() {
        let store = GraphStore::new();
        let report = store.run_timer_maintenance_tick().expect("tick");
        assert_eq!(report.remaining_queue_len(), 0);
    }

    #[test]
    fn detach_delete_homogeneous_directed_edge() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        store.insert_directed_edge(a, b, None).expect("edge");
        store.detach_delete_vertex(a).expect("detach delete");
        assert!(store.out_edges(b).expect("out").is_empty());
    }
}
