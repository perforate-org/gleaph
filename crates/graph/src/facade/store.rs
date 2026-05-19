use super::stable::edge_label_catalog::EdgeLabelCatalogError;
use super::stable::edge_weight_profiles::EdgeWeightProfileStoreError;
use super::stable::memory::StableGraph;
use super::stable::vertex_label_catalog::VertexLabelCatalogError;
use super::stable::{
    EDGE_ALIASES, EDGE_LABEL_CATALOG, EDGE_PROPERTIES, EDGE_WEIGHT_PROFILES, GRAPH,
    GRAPH_DEFAULT_EDGE_LABEL, METADATA, PREPARED_QUERY_CATALOG, PROPERTY_CATALOG,
    REMOTE_VERTEX_REFS, VERTEX_LABEL_CATALOG, VERTEX_LABELS, VERTEX_LOGICAL_IDS, VERTEX_PROPERTIES,
};
use super::{
    FederationRouting, GraphMetadata, GraphMetadataError, PropertyCatalogError, VertexLabelStoreError,
    VertexPropertyStoreError,
};
use crate::index::{edge_equal, pending, placement};
use gleaph_graph_kernel::federation::{
    standalone_logical_vertex_id, CommitVertexPlacementArgs, LogicalVertexId,
};
use gleaph_graph_kernel::path::GraphPathVertexId;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, EdgeTarget, EdgeWeightProfile, PropertyId,
    RemoteRefId, TaggedEdgeLabelId, Vertex, VertexLabelId, VertexRef,
};
use gleaph_graph_prepared::{PreparedQueryError, PreparedQueryRecord};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, MaintenanceBudget,
    VertexCount, VertexId,
    labeled::{
        BucketDirectedness, EdgeSlotMove, EdgeSlotMoveObserver,
        LabeledBidirectionalMaintenanceReport, LabeledOrientation, OutEdgeOrder,
    },
    traits::CsrEdge,
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

struct GraphSidecarMoveObserver;

impl EdgeSlotMoveObserver for GraphSidecarMoveObserver {
    fn edge_slot_moved(
        &mut self,
        orientation: LabeledOrientation,
        vid: VertexId,
        moved: EdgeSlotMove,
    ) {
        GraphStore::move_edge_sidecars_for_compaction(orientation, vid, moved);
    }
}

fn edge_storage_label(catalog: Option<EdgeLabelId>, undirected: bool) -> TaggedEdgeLabelId {
    match catalog {
        None => {
            if undirected {
                TaggedEdgeLabelId::UNLABELED_UNDIRECTED
            } else {
                TaggedEdgeLabelId::UNLABELED_DIRECTED
            }
        }
        Some(catalog_id) => {
            if undirected {
                catalog_id.pack(EdgeDirectedness::Undirected)
            } else {
                catalog_id.pack(EdgeDirectedness::Directed)
            }
        }
    }
}

fn lara_label(id: TaggedEdgeLabelId) -> LaraLabelId {
    LaraLabelId::from_raw(id.raw())
}

pub fn canonical_undirected_owner(a: VertexId, b: VertexId) -> VertexId {
    if u32::from(a) >= u32::from(b) { a } else { b }
}

fn build_edge_to(target: VertexId, inline_value: u16) -> Edge {
    Edge {
        target: VertexRef::local(target),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_value,
    }
}

fn build_edge_to_remote(remote_ref: RemoteRefId, inline_value: u16) -> Edge {
    Edge {
        target: VertexRef::remote_ref(remote_ref),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        inline_value,
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    pub owner_vertex_id: VertexId,
    pub label_id: LaraLabelId,
    pub slot_index: u32,
}

#[derive(Debug)]
pub enum GraphStoreError {
    Graph(DeferredBidirectionalLabeledError),
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
        label_id: LaraLabelId,
        slot_index: u32,
    },
    /// Edge label id is outside the inline edge band `0x0001..=0x3FFF`.
    InvalidEdgeLabelId(EdgeLabelId),
    VertexPlacement(placement::VertexPlacementError),
    /// Router reports this shard-local vertex is frozen during migration.
    VertexMigrating,
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
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
                label_id,
                slot_index,
            } => write!(
                f,
                "no edge record for owner {owner_vertex_id:?}, label {label_id:?}, slot {slot_index}"
            ),
            Self::InvalidEdgeLabelId(id) => write!(
                f,
                "edge label id {} is not a catalog edge label (MSB clear, non-zero)",
                id.raw()
            ),
            Self::VertexPlacement(err) => write!(f, "{err}"),
            Self::VertexMigrating => write!(f, "vertex is frozen for migration on this shard"),
        }
    }
}

impl EdgeHandle {
    fn at_slot(owner_vertex_id: VertexId, label_id: LaraLabelId, slot_index: u32) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            slot_index,
        }
    }
}

impl std::error::Error for GraphStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Graph(err) => Some(err),
            Self::VertexLabelCatalog(err) => Some(err),
            Self::EdgeLabelCatalog(err) => Some(err),
            Self::EdgeWeightProfile(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_)
            | Self::VertexPlacement(_)
            | Self::VertexMigrating => None,
        }
    }
}

impl From<placement::VertexPlacementError> for GraphStoreError {
    fn from(value: placement::VertexPlacementError) -> Self {
        Self::VertexPlacement(value)
    }
}

impl From<DeferredBidirectionalLabeledError> for GraphStoreError {
    fn from(value: DeferredBidirectionalLabeledError) -> Self {
        Self::Graph(value)
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

    pub fn federation_routing(&self) -> Option<FederationRouting> {
        METADATA.with_borrow(|m| m.get().federation_routing())
    }

    pub fn set_federation_routing(
        &self,
        federation_routing: Option<FederationRouting>,
    ) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            metadata.set_federation_routing(federation_routing);
            m.set(metadata)
        })
    }

    pub fn federation_configured(&self) -> bool {
        METADATA.with_borrow(|m| m.get().federation_configured())
    }

    pub fn vertex_label_id(&self, name: &str) -> Option<VertexLabelId> {
        VERTEX_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    pub fn edge_label_id(&self, name: &str) -> Option<EdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_id(name))
    }

    /// Resolves a catalog name to a **directed** LARA / bucket wire key (MSB clear).
    pub fn edge_label_tagged_directed(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_tagged_directed(name))
    }

    /// Resolves a catalog name to an **undirected** LARA / bucket wire key (MSB set).
    pub fn edge_label_tagged_undirected(&self, name: &str) -> Option<TaggedEdgeLabelId> {
        EDGE_LABEL_CATALOG.with_borrow(|catalog| catalog.get_tagged_undirected(name))
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
        Ok(TaggedEdgeLabelId::from_raw(bucket.raw()).is_undirected())
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

    pub fn insert_vertex(&self) -> Result<VertexId, GraphStoreError> {
        self.insert_vertex_row(Vertex::default())
    }

    pub fn insert_vertex_row(&self, vertex: Vertex) -> Result<VertexId, GraphStoreError> {
        let pending_logical = self
            .federation_routing()
            .map(|routing| placement::allocate_logical_vertex_id(routing.router_canister))
            .transpose()?;

        let vertex_id = self
            .with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
            .map_err(GraphStoreError::from)?;

        let logical_vertex_id = match pending_logical {
            Some(logical_vertex_id) => {
                let routing = self
                    .federation_routing()
                    .expect("federation routing required after allocate");
                placement::commit_vertex_placement(
                    routing.router_canister,
                    CommitVertexPlacementArgs {
                        logical_vertex_id,
                        local_vertex_id: placement::local_vertex_id_raw(vertex_id),
                    },
                )?;
                logical_vertex_id
            }
            None => standalone_logical_vertex_id(vertex_id),
        };

        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| {
            map.insert(vertex_id, logical_vertex_id);
        });
        Ok(vertex_id)
    }

    /// Resolves the stable logical vertex id for a shard-local [`VertexId`].
    pub fn logical_vertex_id(&self, vertex_id: VertexId) -> Option<LogicalVertexId> {
        VERTEX_LOGICAL_IDS
            .with_borrow(|map| map.get(vertex_id))
            .or_else(|| {
                self.federation_routing()
                    .is_none()
                    .then(|| standalone_logical_vertex_id(vertex_id))
            })
    }

    /// Rejects writes to a shard-local vertex that the router has marked as migrating away.
    pub(crate) fn assert_local_vertex_writable(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(logical_vertex_id) = self.logical_vertex_id(vertex_id) else {
            return Ok(());
        };
        let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
        if let gleaph_graph_kernel::federation::VertexPlacement::Migrating { source, .. } = placement
        {
            let local = placement::local_vertex_id_raw(vertex_id);
            if source.shard_id == routing.shard_id && source.local_vertex_id == local {
                return Err(GraphStoreError::VertexMigrating);
            }
        }
        Ok(())
    }

    pub(crate) fn path_vertex_element_id(&self, vertex_id: VertexId) -> Option<GraphPathVertexId> {
        self.logical_vertex_id(vertex_id)
            .map(GraphPathVertexId::new)
    }

    /// Interns a shard-local [`RemoteRefId`] for `logical_vertex_id` (idempotent).
    pub fn ensure_remote_ref(
        &self,
        logical_vertex_id: LogicalVertexId,
    ) -> RemoteRefId {
        REMOTE_VERTEX_REFS.with_borrow_mut(|table| table.ensure_remote_ref(logical_vertex_id))
    }

    pub fn logical_vertex_for_remote_ref(
        &self,
        remote_ref: RemoteRefId,
    ) -> Option<LogicalVertexId> {
        REMOTE_VERTEX_REFS
            .with_borrow(|table| table.logical_vertex_id(remote_ref))
    }

    pub fn edge_target(&self, edge: &Edge) -> Option<EdgeTarget> {
        edge.edge_target()
    }

    /// Inserts a forward-only directed edge to a vertex on another shard (remote ref).
    pub fn insert_directed_edge_to_logical(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let remote_ref = self.ensure_remote_ref(target_logical_vertex_id);
        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to_remote(remote_ref, 0);
        self.with_graph_mut(|graph| {
            graph.insert_forward_out_edge(source_vertex_id, label, forward)
        })?;
        self.find_newest_forward_handle(source_vertex_id, label, |edge| {
            matches!(
                edge.edge_target(),
                Some(EdgeTarget::Remote(found)) if found == remote_ref
            ) && edge.inline_value == forward.inline_value
        })?
        .ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: source_vertex_id,
            label_id: label,
            slot_index: u32::MAX,
        })
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

    #[inline]
    pub(crate) fn vertex_has_any_label(&self, vertex_id: VertexId, vertex: Vertex) -> bool {
        VERTEX_LABELS.with_borrow(|labels| {
            labels.with_label_ids(vertex_id, vertex, |slice| !slice.is_empty())
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

    /// GQL vertex `properties` field as a `Value::Record` without allocating an intermediate
    /// `Vec<(PropertyId, Value)>`.
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

    pub fn edge_property(&self, handle: EdgeHandle, property_id: PropertyId) -> Option<Value> {
        let handle = self.canonical_edge_handle(handle);
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
        let handle = self.canonical_edge_handle(handle);
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
        let handle = self.canonical_edge_handle(handle);
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
        let handle = self.canonical_edge_handle(handle);
        EDGE_PROPERTIES.with_borrow(|properties| {
            properties.properties_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            )
        })
    }

    pub(crate) fn edge_properties_gql_record(&self, handle: EdgeHandle) -> Value {
        let handle = self.canonical_edge_handle(handle);
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

    fn validate_catalog_edge_label(label: Option<EdgeLabelId>) -> Result<(), GraphStoreError> {
        if let Some(id) = label {
            if id.raw() != 0 && !id.is_catalog_allocatable() {
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

        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to(target_vertex_id, 0);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_value: forward.inline_value,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_newest_forward_handle(source_vertex_id, label, |edge| {
                edge.neighbor_vid() == target_vertex_id && edge.inline_value == forward.inline_value
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) = self.find_newest_reverse_handle(target_vertex_id, label, |edge| {
            edge.neighbor_vid() == source_vertex_id && edge.inline_value == forward.inline_value
        })? {
            self.insert_edge_alias(alias, canonical);
        }
        Ok(canonical)
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

        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to(target_vertex_id, inline_value);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_value: forward.inline_value,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_newest_forward_handle(source_vertex_id, label, |edge| {
                edge.neighbor_vid() == target_vertex_id && edge.inline_value == forward.inline_value
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) = self.find_newest_reverse_handle(target_vertex_id, label, |edge| {
            edge.neighbor_vid() == source_vertex_id && edge.inline_value == forward.inline_value
        })? {
            self.insert_edge_alias(alias, canonical);
        }
        Ok(canonical)
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

        let label = lara_label(edge_storage_label(catalog_label, true));
        let edge_ab = build_edge_to(endpoint_b, 0);
        let edge_ba = build_edge_to(endpoint_a, 0);
        self.with_graph_mut(|graph| {
            graph.insert_undirected_deferred(endpoint_a, endpoint_b, label, edge_ab, edge_ba)
        })?;
        let owner_vertex_id = canonical_undirected_owner(endpoint_a, endpoint_b);
        let target = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        let canonical = self
            .find_newest_forward_handle(owner_vertex_id, label, |edge| {
                edge.neighbor_vid() == target && edge.inline_value == 0
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        let alias_vertex_id = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        if let Some(alias) = self.find_newest_forward_handle(alias_vertex_id, label, |edge| {
            edge.neighbor_vid() == owner_vertex_id && edge.inline_value == 0
        })? {
            self.insert_edge_alias(alias, canonical);
        }
        Ok(canonical)
    }

    pub fn out_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.out_edges(vertex_id))
    }

    pub fn in_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.in_edges(vertex_id))
    }

    pub fn asc_out_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.asc_out_edges(vertex_id))
    }

    pub fn asc_in_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<Edge>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.asc_in_edges(vertex_id))
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

    pub(crate) fn for_each_out_edges_for_label_ordered<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_out_edges_for_label_ordered(vertex_id, label, order, visit)
        })
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

    /// Applies CSR `Iterator::advance_by` for the global streaming offset, then visits subsequent
    /// out-edges for one label (see [`DeferredBidirectionalLabeledLaraGraph::skip_then_visit_each_forward_out_edge_for_label`]).
    pub(crate) fn skip_then_visit_each_out_edge_for_label<Visit, Err>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_forward_out_edge_for_label(
                    vertex_id,
                    label,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Applies CSR `Iterator::advance_by` for the global streaming offset, then visits subsequent
    /// forward out-edges whose bucket directedness matches `directedness`.
    pub(crate) fn skip_then_visit_each_out_edge_by_directedness<Visit, Err>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_forward_out_edge_by_directedness(
                    vertex_id,
                    directedness,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Applies CSR `Iterator::advance_by` for the global streaming offset, then visits subsequent
    /// reverse out-edges for one label (incoming forward edges).
    pub(crate) fn skip_then_visit_each_in_edge_for_label<Visit, Err>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_reverse_out_edge_for_label(
                    vertex_id,
                    label,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Applies CSR `Iterator::advance_by` for the global streaming offset, then visits subsequent
    /// reverse out-edges whose bucket directedness matches `directedness`.
    pub(crate) fn skip_then_visit_each_in_edge_by_directedness<Visit, Err>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        offset_remaining: &mut usize,
        visit: Visit,
    ) -> Result<Result<bool, Err>, GraphStoreError>
    where
        Visit: FnMut(Edge) -> Result<bool, Err>,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.skip_then_visit_each_reverse_out_edge_by_directedness(
                    vertex_id,
                    directedness,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Outgoing edges whose bucket label matches `directedness`, in `order`
    /// (see [`ic_stable_lara::LabeledLaraGraph::for_each_out_edges_by_directedness`]).
    pub fn for_each_out_edges_by_directedness<Visit>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_by_directedness(vertex_id, directedness, order, visit)
            })
            .map_err(GraphStoreError::from)
    }

    /// Like [`Self::for_each_out_edges_by_directedness`], but skips `ensure_vertex` on the hot path.
    pub fn for_each_out_edges_by_directedness_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_by_directedness_unchecked(
                    vertex_id,
                    directedness,
                    order,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Incoming edges (reverse CSR at `vertex_id`) filtered by bucket directedness in `order`.
    pub fn for_each_in_edges_by_directedness<Visit>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_by_directedness(vertex_id, directedness, order, visit)
            })
            .map_err(GraphStoreError::from)
    }

    /// Like [`Self::for_each_in_edges_by_directedness`], but skips reverse vertex range validation.
    pub fn for_each_in_edges_by_directedness_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        directedness: BucketDirectedness,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_by_directedness_unchecked(
                    vertex_id,
                    directedness,
                    order,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
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

    pub(crate) fn for_each_in_edges_for_label_ordered<Visit>(
        &self,
        vertex_id: VertexId,
        label: LaraLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.for_each_in_edges_for_label_ordered(vertex_id, label, order, visit)
        })
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

    fn find_newest_forward_handle<F>(
        &self,
        owner_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.find_forward_out_edge_slot_with_label_by_predicate(owner_vertex_id, |edge| {
                    pred(edge)
                })
            })
            .map_err(GraphStoreError::from)
            .map(|found| {
                found.and_then(|(_, label_id, slot_index)| {
                    (label_id == expected_label).then_some(EdgeHandle {
                        owner_vertex_id,
                        label_id,
                        slot_index,
                    })
                })
            })
    }

    fn find_newest_reverse_handle<F>(
        &self,
        row_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        GRAPH
            .with_borrow(|graph| {
                graph.find_reverse_out_edge_slot_with_label_by_predicate(row_vertex_id, |edge| {
                    pred(edge)
                })
            })
            .map_err(GraphStoreError::from)
            .map(|found| {
                found.and_then(|(_, label_id, slot_index)| {
                    (label_id == expected_label).then_some(EdgeHandle::at_slot(
                        row_vertex_id,
                        label_id,
                        slot_index,
                    ))
                })
            })
    }

    pub(crate) fn canonical_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        EDGE_ALIASES
            .with_borrow(|aliases| {
                aliases.get(
                    handle.owner_vertex_id,
                    handle.label_id.raw(),
                    handle.slot_index,
                )
            })
            .map(|canonical| {
                EdgeHandle::at_slot(
                    canonical.canonical_vertex_id(),
                    handle.label_id,
                    canonical.canonical_slot_index(),
                )
            })
            .unwrap_or(handle)
    }

    fn insert_edge_alias(&self, alias: EdgeHandle, canonical: EdgeHandle) {
        if alias.owner_vertex_id == canonical.owner_vertex_id
            && alias.label_id == canonical.label_id
            && alias.slot_index == canonical.slot_index
        {
            return;
        }
        debug_assert_eq!(alias.label_id, canonical.label_id);
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.insert(
                alias.owner_vertex_id,
                alias.label_id.raw(),
                alias.slot_index,
                canonical.owner_vertex_id,
                canonical.slot_index,
            );
        });
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
        GRAPH.with_borrow(|graph| {
            graph.visit_out_edges(
                vertex_id,
                None,
                None,
                None::<&mut dyn FnMut(&[u8]) -> bool>,
                matches,
                visit,
            )
        })
    }

    /// Visits outgoing edges with optional `offset` / `limit`, slab raw-byte prefilter, and match predicate.
    pub fn visit_out_edges<Match, Visit>(
        &self,
        vertex_id: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_out_edges(vertex_id, offset, limit, raw_matches, matches, visit)
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
        self.visit_in_edges(
            vertex_id,
            None,
            None,
            None::<&mut dyn FnMut(&[u8]) -> bool>,
            matches,
            visit,
        )
    }

    /// Visits incoming edges with optional `offset` / `limit`, slab raw-byte prefilter, and match predicate.
    pub fn visit_in_edges<Match, Visit>(
        &self,
        vertex_id: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), DeferredBidirectionalLabeledError>
    where
        Match: FnMut(&Edge) -> bool,
        Visit: FnMut(Edge),
    {
        GRAPH.with_borrow(|graph| {
            graph.visit_in_edges(vertex_id, offset, limit, raw_matches, matches, visit)
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
        let mut observer = GraphSidecarMoveObserver;
        GRAPH
            .with_borrow(|graph| {
                graph.maintenance_with_edge_slot_move_observer(budget, &mut observer)
            })
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

        let mut to_clear: Vec<EdgeHandle> = Vec::new();
        for edge in self.out_edges(vertex_id).map_err(GraphStoreError::from)? {
            let owner = self.edge_sidecar_owner_from_out_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        }
        for edge in self.in_edges(vertex_id).map_err(GraphStoreError::from)? {
            let owner = self.edge_sidecar_owner_from_in_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        }
        to_clear.sort_unstable_by_key(|h| {
            (u32::from(h.owner_vertex_id), h.label_id.raw(), h.slot_index)
        });
        to_clear.dedup_by_key(|h| (u32::from(h.owner_vertex_id), h.label_id.raw(), h.slot_index));

        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        for handle in to_clear {
            self.clear_edge_sidecars(handle);
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    /// Removes one logical edge (and its stable properties) identified by `handle`.
    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        let canonical = self.canonical_edge_handle(handle);
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        self.clear_edge_sidecars(handle);
        let edge = self.with_graph_mut(|graph| {
            graph.remove_forward_edge_at_slot(
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )
        })?;
        let edge = edge.ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: canonical.owner_vertex_id,
            label_id: canonical.label_id,
            slot_index: canonical.slot_index,
        })?;
        let neighbor = edge.neighbor_vid();
        self.with_graph_mut(|graph| {
            graph.remove_reverse_edge_matching(neighbor, canonical.label_id, |cand| {
                cand.neighbor_vid() == canonical.owner_vertex_id
                    && *cand == edge.with_neighbor_vid(canonical.owner_vertex_id)
            })
        })?;
        if TaggedEdgeLabelId::from_raw(canonical.label_id.raw()).is_undirected() {
            self.with_graph_mut(|graph| {
                graph.remove_directed_deferred(
                    neighbor,
                    canonical.owner_vertex_id,
                    edge.with_neighbor_vid(canonical.owner_vertex_id),
                )
            })?;
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

    fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle(handle);
        edge_equal::remove_all_for_edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
        );
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.remove(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
            aliases.remove_all_for_canonical(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    fn move_edge_sidecars_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                let moved_properties = EDGE_PROPERTIES.with_borrow_mut(|store| {
                    store
                        .move_all_for_edge(
                            owner_vertex_id,
                            label_id,
                            moved.old_slot_index,
                            moved.new_slot_index,
                        )
                        .expect("stored edge property values remain encodable")
                });
                if !moved_properties.is_empty() {
                    edge_equal::remove_all_for_edge(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                    );
                    for (property_id, value) in &moved_properties {
                        edge_equal::record_edge_property_change(
                            owner_vertex_id,
                            label_id,
                            moved.new_slot_index,
                            *property_id,
                            None,
                            Some(value),
                        );
                    }
                }
                EDGE_ALIASES.with_borrow_mut(|aliases| {
                    aliases.move_canonical_target(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                    aliases.move_alias_key(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                });
            }
            LabeledOrientation::Reverse => {
                EDGE_ALIASES.with_borrow_mut(|aliases| {
                    aliases.move_alias_key(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                });
            }
        }
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

    fn lookup_forward_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        GRAPH.with_borrow(|graph| {
            graph
                .find_forward_out_edge_slot_with_label_by_predicate(
                    handle.owner_vertex_id,
                    |edge| edge.edge_slot_index == EdgeSlotIndex::from_raw(handle.slot_index),
                )
                .map(|found| {
                    found.and_then(|(edge, label_id, slot_index)| {
                        (label_id == handle.label_id && slot_index == handle.slot_index)
                            .then_some((edge, label_id))
                    })
                })
                .map_err(GraphStoreError::from)
        })
    }

    fn lookup_reverse_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        GRAPH.with_borrow(|graph| {
            graph
                .find_reverse_out_edge_slot_with_label_by_predicate(
                    handle.owner_vertex_id,
                    |edge| edge.edge_slot_index == EdgeSlotIndex::from_raw(handle.slot_index),
                )
                .map(|found| {
                    found.and_then(|(edge, label_id, slot_index)| {
                        (label_id == handle.label_id && slot_index == handle.slot_index)
                            .then_some((edge, label_id))
                    })
                })
                .map_err(GraphStoreError::from)
        })
    }

    fn lookup_edge_entry(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        match self.lookup_forward_out_edge(handle)? {
            Some(found) => Ok(Some(found)),
            None => self.lookup_reverse_out_edge(handle),
        }
    }

    pub(crate) fn find_outgoing_edge_with_bucket_label(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        self.lookup_edge_entry(handle)
    }

    pub(crate) fn find_outgoing_edge_record(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<Edge>, GraphStoreError> {
        Ok(self.lookup_edge_entry(handle)?.map(|(edge, _)| edge))
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
        assert_eq!(
            EdgeSlotIndex::from_raw(directed.slot_index),
            EdgeSlotIndex::from_raw(0)
        );

        let out_edges = store.out_edges(source).expect("read out edges");
        assert!(out_edges.iter().any(|edge| {
            edge.target == VertexRef::local(target)
                && edge.edge_slot_index.raw() == directed.slot_index
                && !store.edge_is_undirected(source, edge).unwrap()
        }));

        let undirected = store
            .insert_undirected_edge(target, source, None)
            .expect("insert undirected edge");

        assert_eq!(undirected.owner_vertex_id, target);
        assert_eq!(
            EdgeSlotIndex::from_raw(undirected.slot_index),
            EdgeSlotIndex::from_raw(0)
        );

        let target_out_edges = store.out_edges(target).expect("read target out edges");
        assert!(target_out_edges.iter().any(|edge| {
            edge.target == VertexRef::local(source)
                && edge.edge_slot_index.raw() == undirected.slot_index
                && store.edge_is_undirected(target, edge).unwrap()
        }));
    }

    #[test]
    fn insert_directed_edge_to_logical_stores_remote_ref() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target_logical = 42_u64;

        let handle = store
            .insert_directed_edge_to_logical(source, target_logical, None)
            .expect("remote edge");

        let remote_ref = store.ensure_remote_ref(target_logical);
        assert_eq!(
            store.logical_vertex_for_remote_ref(remote_ref),
            Some(target_logical)
        );
        assert_eq!(store.ensure_remote_ref(target_logical), remote_ref);

        let out_edges = store.out_edges(source).expect("out edges");
        assert_eq!(out_edges.len(), 1);
        assert_eq!(
            out_edges[0].edge_target(),
            Some(EdgeTarget::Remote(remote_ref))
        );
        assert_eq!(handle.owner_vertex_id, source);
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

    #[test]
    fn forward_edge_compaction_moves_property_sidecars() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let first = store.insert_vertex().expect("first");
        let second = store.insert_vertex().expect("second");
        let third = store.insert_vertex().expect("third");
        let label = store
            .get_or_insert_edge_label_id("CompactionMovesForward")
            .expect("label");
        let property = store
            .get_or_insert_property_id("move_marker")
            .expect("property");

        let first_edge = store
            .insert_directed_edge(source, first, Some(label))
            .expect("first edge");
        store
            .insert_directed_edge(source, second, Some(label))
            .expect("second edge");
        store
            .insert_directed_edge(source, third, Some(label))
            .expect("third edge");

        let old_third = EdgeHandle::at_slot(
            source,
            lara_label(label.pack(EdgeDirectedness::Directed)),
            2,
        );
        store
            .set_edge_property(old_third, property, Value::Int64(33))
            .expect("set property");
        store
            .delete_edge_by_handle(first_edge)
            .expect("delete first");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_vertex_edge_span(LabeledOrientation::Forward, source, 0)
                .expect("mark compaction");
        });
        store
            .run_maintenance_best_effort(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("maintenance");

        let moved = store
            .out_edges(source)
            .expect("out edges")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == third)
            .expect("third edge after compaction");
        assert_eq!(moved.edge_slot_index, EdgeSlotIndex::from_raw(1));
        let new_third = EdgeHandle::at_slot(
            source,
            LaraLabelId::from_raw(moved.label_id),
            moved.edge_slot_index.raw(),
        );
        assert_eq!(
            store.edge_property(new_third, property),
            Some(Value::Int64(33))
        );
        assert_eq!(store.edge_property(old_third, property), None);
    }

    #[test]
    fn reverse_edge_compaction_moves_alias_keys() {
        let store = GraphStore::new();
        let first = store.insert_vertex().expect("first");
        let second = store.insert_vertex().expect("second");
        let third = store.insert_vertex().expect("third");
        let target = store.insert_vertex().expect("target");
        let label = store
            .get_or_insert_edge_label_id("CompactionMovesReverseAlias")
            .expect("label");
        let other_label = store
            .get_or_insert_edge_label_id("CompactionMovesReverseAliasOther")
            .expect("other label");
        let property = store
            .get_or_insert_property_id("reverse_move_marker")
            .expect("property");

        let first_edge = store
            .insert_directed_edge(first, target, Some(label))
            .expect("first edge");
        store
            .insert_directed_edge(second, target, Some(label))
            .expect("second edge");
        let third_edge = store
            .insert_directed_edge(third, target, Some(label))
            .expect("third edge");
        store
            .insert_directed_edge(second, target, Some(other_label))
            .expect("other label edge");
        store
            .set_edge_property(third_edge, property, Value::Int64(44))
            .expect("set property");

        store
            .delete_edge_by_handle(first_edge)
            .expect("delete first");
        store.with_graph_mut(|graph| {
            graph
                .mark_compact_dense_labeled_vertex_maintenance(LabeledOrientation::Reverse, target)
                .expect("mark reverse compaction");
        });
        store
            .run_maintenance_best_effort(MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            })
            .expect("maintenance");

        let moved_alias = store
            .in_edges(target)
            .expect("in edges")
            .into_iter()
            .find(|edge| {
                edge.neighbor_vid() == third
                    && edge.label_id == label.pack(EdgeDirectedness::Directed).raw()
            })
            .expect("third reverse edge after compaction");
        assert_eq!(moved_alias.edge_slot_index, EdgeSlotIndex::from_raw(1));
        let alias_handle = EdgeHandle::at_slot(
            target,
            LaraLabelId::from_raw(moved_alias.label_id),
            moved_alias.edge_slot_index.raw(),
        );
        assert_eq!(
            store.edge_property(alias_handle, property),
            Some(Value::Int64(44))
        );
    }
}
