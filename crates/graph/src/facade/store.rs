use super::stable::edge_label_catalog::EdgeLabelCatalogError;
use super::stable::edge_value_profiles::EdgeValueProfileStoreError;
use super::stable::edge_weight_profiles::EdgeWeightProfileStoreError;
use super::stable::memory::StableGraph;
use super::stable::vertex_label_catalog::VertexLabelCatalogError;
use super::stable::{
    EDGE_ALIASES, EDGE_LABEL_CATALOG, EDGE_PROPERTIES, EDGE_VALUE_PROFILES, EDGE_WEIGHT_PROFILES,
    GRAPH, GRAPH_DEFAULT_EDGE_LABEL, METADATA, PEER_GRAPH_CANISTERS, PROPERTY_CATALOG,
    REMOTE_FORWARD_IN, REMOTE_VERTEX_REFS, VERTEX_LABEL_CATALOG, VERTEX_LABELS, VERTEX_LOGICAL_IDS,
    VERTEX_PROPERTIES,
};
use super::{
    FederationRouting, GraphMetadata, GraphMetadataError, PropertyCatalogError,
    VertexLabelStoreError, VertexPropertyStoreError,
};
use crate::index::{edge_equal, pending, placement};
use candid::Principal;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, EdgeTarget, EdgeValueProfile,
    EdgeWeightProfile, PropertyId, RemoteRefId, TaggedEdgeLabelId, Vertex, VertexLabelId,
    VertexRef,
};
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, LogicalVertexId, ReleaseLogicalVertexArgs, VertexPlacement,
    standalone_logical_vertex_id,
};
use gleaph_graph_kernel::path::GraphPathVertexId;
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

/// Tag bit for reverse-IN alias keys so they do not collide with forward-OUT slot indices
/// on the same vertex (both CSR stores use independent slot counters).
const EDGE_ALIAS_REVERSE_IN_TAG: u32 = 1 << 31;

#[inline]
fn edge_alias_slot_key(slot_index: u32, reverse_in: bool) -> u32 {
    if reverse_in {
        slot_index | EDGE_ALIAS_REVERSE_IN_TAG
    } else {
        slot_index
    }
}

#[inline]
fn edge_alias_slot_key_parts(slot_key: u32) -> (u32, bool) {
    let reverse_in = slot_key & EDGE_ALIAS_REVERSE_IN_TAG != 0;
    (slot_key & !EDGE_ALIAS_REVERSE_IN_TAG, reverse_in)
}

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

fn wire_catalog_label(label: Option<EdgeLabelId>, directedness: EdgeDirectedness) -> LaraLabelId {
    lara_label(edge_storage_label(
        label,
        matches!(directedness, EdgeDirectedness::Undirected),
    ))
}

pub fn canonical_undirected_owner(a: VertexId, b: VertexId) -> VertexId {
    if u32::from(a) >= u32::from(b) { a } else { b }
}

fn build_edge_to(target: VertexId) -> Edge {
    Edge {
        target: VertexRef::local(target),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
    }
}

fn build_edge_to_with_value_bytes(target: VertexId, value_bytes: &[u8]) -> Edge {
    build_edge_to(target).with_value_bytes(value_bytes)
}

fn build_edge_to_remote(remote_ref: RemoteRefId) -> Edge {
    Edge {
        target: VertexRef::remote_ref(remote_ref),
        edge_slot_index: EdgeSlotIndex::from_raw(0),
        label_id: 0,
        value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
    }
}

fn build_edge_to_remote_with_value_bytes(remote_ref: RemoteRefId, value_bytes: &[u8]) -> Edge {
    build_edge_to_remote(remote_ref).with_value_bytes(value_bytes)
}

fn validate_edge_value_bytes(value_bytes: &[u8]) -> Result<(), GraphStoreError> {
    match value_bytes.len() {
        0 | 1 | 2 | 4 | 8 => Ok(()),
        len => Err(GraphStoreError::InvalidEdgeValueWidth(len)),
    }
}

fn edge_value_bytes_match(edge: &Edge, value_bytes: &[u8]) -> bool {
    edge.value_bytes() == value_bytes
}

fn edge_matches_local_neighbor(edge: &Edge, neighbor: VertexId, value_bytes: &[u8]) -> bool {
    edge.neighbor_vid() == neighbor && edge_value_bytes_match(edge, value_bytes)
}

fn edge_matches_remote_target(edge: &Edge, remote_ref: RemoteRefId, value_bytes: &[u8]) -> bool {
    matches!(
        edge.edge_target(),
        Some(EdgeTarget::Remote(found)) if found == remote_ref
    ) && edge_value_bytes_match(edge, value_bytes)
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
    EdgeValueProfile(EdgeValueProfileStoreError),
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
    /// Edge value byte width is not supported by labeled edge-value storage.
    InvalidEdgeValueWidth(usize),
    VertexPlacement(placement::VertexPlacementError),
    /// Router reports this shard-local vertex is frozen during migration.
    VertexMigrating,
    /// Shard-local CSR row is tombstoned (stale after migration).
    VertexTombstoned,
}

impl fmt::Display for GraphStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Graph(err) => write!(f, "{err}"),
            Self::VertexLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeLabelCatalog(err) => write!(f, "{err}"),
            Self::EdgeValueProfile(err) => write!(f, "{err}"),
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
            Self::InvalidEdgeValueWidth(width) => {
                write!(f, "edge value byte width {width} is not supported")
            }
            Self::VertexPlacement(err) => write!(f, "{err}"),
            Self::VertexMigrating => write!(f, "vertex is frozen for migration on this shard"),
            Self::VertexTombstoned => write!(f, "vertex row is tombstoned on this shard"),
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
            Self::EdgeValueProfile(err) => Some(err),
            Self::EdgeWeightProfile(err) => Some(err),
            Self::PropertyCatalog(err) => Some(err),
            Self::VertexLabel(err) => Some(err),
            Self::PropertyValue(err) => Some(err),
            Self::VertexNotDetached { .. }
            | Self::EdgeNotFound { .. }
            | Self::InvalidEdgeLabelId(_)
            | Self::InvalidEdgeValueWidth(_)
            | Self::VertexPlacement(_)
            | Self::VertexMigrating
            | Self::VertexTombstoned => None,
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

impl From<EdgeValueProfileStoreError> for GraphStoreError {
    fn from(value: EdgeValueProfileStoreError) -> Self {
        Self::EdgeValueProfile(value)
    }
}

impl From<EdgeWeightProfileStoreError> for GraphStoreError {
    fn from(value: EdgeWeightProfileStoreError) -> Self {
        Self::EdgeWeightProfile(value)
    }
}

pub fn catalog_edge_label_from_wire(label: LaraLabelId) -> Option<EdgeLabelId> {
    if label == LaraLabelId::UNLABELED_DIRECTED || label == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(label.label_index()))
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

    pub fn is_peer_graph_canister(&self, principal: &Principal) -> bool {
        PEER_GRAPH_CANISTERS.with_borrow(|peers| peers.contains(principal))
    }

    pub fn bootstrap_peer_graph_canisters(&self, peers: &[Principal], self_canister: Principal) {
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.insert_many(peers, self_canister));
    }

    pub fn add_peer_graph_canister(&self, peer: Principal, self_canister: Principal) {
        if peer == self_canister {
            return;
        }
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.insert(peer));
    }

    pub fn remove_peer_graph_canister(&self, peer: &Principal) -> bool {
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.remove(peer))
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

    /// Registers a legacy weight profile and mirrors it into [`EDGE_VALUE_PROFILES`].
    ///
    /// Traversal decode prefers [`EdgeValueProfile`] (see [`Self::edge_label_value_profile`]);
    /// the weight catalog remains for existing callers until profile APIs fully converge.
    pub fn set_edge_label_weight_profile(
        &self,
        label: EdgeLabelId,
        profile: EdgeWeightProfile,
    ) -> Result<(), GraphStoreError> {
        let value_profile = EdgeValueProfile::from(profile.clone());
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.insert(label, value_profile))?;
        Ok(())
    }

    pub fn set_edge_label_value_profile(
        &self,
        label: EdgeLabelId,
        profile: EdgeValueProfile,
    ) -> Result<(), GraphStoreError> {
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.insert(label, profile))?;
        Ok(())
    }

    pub fn edge_label_weight_profile(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        EDGE_WEIGHT_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn edge_label_value_profile(&self, label: EdgeLabelId) -> Option<EdgeValueProfile> {
        EDGE_VALUE_PROFILES.with_borrow(|store| store.get(label))
    }

    pub fn remove_edge_label_weight_profile(&self, label: EdgeLabelId) {
        EDGE_WEIGHT_PROFILES.with_borrow_mut(|store| store.remove(label));
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.remove(label));
    }

    pub fn remove_edge_label_value_profile(&self, label: EdgeLabelId) {
        EDGE_VALUE_PROFILES.with_borrow_mut(|store| store.remove(label));
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
    pub(crate) fn assert_local_vertex_writable(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        if self.vertex(vertex_id).is_some_and(|v| v.is_tombstone()) {
            return Err(GraphStoreError::VertexTombstoned);
        }
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(logical_vertex_id) = self.logical_vertex_id(vertex_id) else {
            return Ok(());
        };
        let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
        if let gleaph_graph_kernel::federation::VertexPlacement::Migrating { source, .. } =
            placement
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
    pub fn ensure_remote_ref(&self, logical_vertex_id: LogicalVertexId) -> RemoteRefId {
        REMOTE_VERTEX_REFS.with_borrow_mut(|table| table.ensure_remote_ref(logical_vertex_id))
    }

    pub fn logical_vertex_for_remote_ref(
        &self,
        remote_ref: RemoteRefId,
    ) -> Option<LogicalVertexId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.logical_vertex_id(remote_ref))
    }

    pub fn remote_ref_for_logical(
        &self,
        logical_vertex_id: LogicalVertexId,
    ) -> Option<RemoteRefId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.remote_ref_for_logical(logical_vertex_id))
    }

    pub(crate) fn edge_sidecar_owner_from_in_row(&self, dst: VertexId, edge: &Edge) -> VertexId {
        if self.edge_is_undirected(dst, edge).unwrap_or(false) {
            canonical_undirected_owner(dst, edge.neighbor_vid())
        } else {
            edge.neighbor_vid()
        }
    }

    pub fn edge_target(&self, edge: &Edge) -> Option<EdgeTarget> {
        edge.edge_target()
    }

    /// Pushes a vertex row during migration import (no router allocate).
    pub(crate) fn push_migrated_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        self.with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
    }

    pub(crate) fn register_logical_vertex_mapping(
        &self,
        vertex_id: VertexId,
        logical_vertex_id: LogicalVertexId,
    ) {
        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| {
            map.insert(vertex_id, logical_vertex_id);
        });
    }

    /// Inserts a forward-only directed edge to a vertex on another shard (remote ref).
    pub fn insert_directed_edge_to_logical(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_edge_to_logical_with_value_bytes(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            false,
            &[],
        )
    }

    pub(crate) fn insert_directed_edge_to_logical_with_value_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_edge_to_logical_with_value_bytes(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            false,
            value_bytes,
        )
    }

    pub(crate) fn insert_undirected_edge_to_logical_with_value_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_edge_to_logical_with_value_bytes(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            true,
            value_bytes,
        )
    }

    fn insert_edge_to_logical_with_value_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_value_bytes(value_bytes)?;

        let remote_ref = self.ensure_remote_ref(target_logical_vertex_id);
        let label = lara_label(edge_storage_label(catalog_label, undirected));
        let forward = build_edge_to_remote_with_value_bytes(remote_ref, value_bytes);
        self.with_graph_mut(|graph| {
            graph.insert_forward_out_edge(source_vertex_id, label, forward)
        })?;
        let handle = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_remote_target(edge, remote_ref, value_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        self.register_remote_forward_in(handle, remote_ref);
        Ok(handle)
    }

    pub(crate) fn register_remote_forward_in(&self, handle: EdgeHandle, remote_ref: RemoteRefId) {
        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
            index.insert(
                remote_ref,
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(crate) fn unregister_remote_forward_in_for_out_edge(
        &self,
        source_vertex_id: VertexId,
        edge: &Edge,
    ) {
        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
            return;
        };
        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
            index.remove(
                remote_ref,
                source_vertex_id,
                edge.label_id,
                edge.edge_slot_index.raw(),
            );
        });
    }

    fn unregister_remote_forward_in_for_handle(&self, handle: EdgeHandle) {
        let label = handle.label_id;
        for edge in self
            .directed_out_edges(handle.owner_vertex_id)
            .unwrap_or_default()
        {
            if edge.label_id != label.raw() || edge.edge_slot_index.raw() != handle.slot_index {
                continue;
            }
            self.unregister_remote_forward_in_for_out_edge(handle.owner_vertex_id, &edge);
            return;
        }
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
        self.set_vertex_property_inner(vertex_id, property_id, value, true)
    }

    /// Sets a vertex property without queueing federated index postings (migration import).
    pub(crate) fn set_vertex_property_without_index_pending(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        self.set_vertex_property_inner(vertex_id, property_id, value, false)
    }

    fn set_vertex_property_inner(
        &self,
        vertex_id: VertexId,
        property_id: PropertyId,
        value: Value,
        record_index_pending: bool,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        let prev =
            VERTEX_PROPERTIES.with_borrow(|properties| properties.get(vertex_id, property_id));
        let out = VERTEX_PROPERTIES
            .with_borrow_mut(|properties| properties.set(vertex_id, property_id, value.clone()))?;
        if record_index_pending {
            pending::record_vertex_property_change(
                vertex_id,
                property_id,
                prev.as_ref(),
                Some(&value),
            );
        }
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
        let forward = build_edge_to(target_vertex_id);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, &[])
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) =
            self.find_reverse_alias_for_canonical(canonical, target_vertex_id, source_vertex_id)?
        {
            self.insert_edge_alias(alias, canonical, true);
        }
        Ok(canonical)
    }

    /// Inserts a directed edge with a 2-byte little-endian value (migration / tests).
    pub(crate) fn insert_directed_edge_with_inline_value(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_value: u16,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_directed_edge_with_value_bytes(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            &inline_value.to_le_bytes(),
        )
    }

    pub(crate) fn insert_directed_edge_with_value_bytes(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_value_bytes(value_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to_with_value_bytes(target_vertex_id, value_bytes);
        // Reverse CSR rows only store the source id; edge values live on the forward owner.
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, value_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) =
            self.find_reverse_alias_for_canonical(canonical, target_vertex_id, source_vertex_id)?
        {
            self.insert_edge_alias(alias, canonical, true);
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
        let edge_ab = build_edge_to(endpoint_b);
        let edge_ba = build_edge_to(endpoint_a);
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
            .find_first_forward_handle_descending(owner_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target, &[])
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
        if let Some(alias) =
            self.find_first_forward_handle_descending(alias_vertex_id, label, |edge| {
                edge.neighbor_vid() == owner_vertex_id
            })?
        {
            self.insert_edge_alias(alias, canonical, false);
        }
        Ok(canonical)
    }

    pub(crate) fn insert_undirected_edge_with_value_bytes(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_value_bytes(value_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, true));
        let edge_ab = build_edge_to_with_value_bytes(endpoint_b, value_bytes);
        let edge_ba = build_edge_to_with_value_bytes(endpoint_a, value_bytes);
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
            .find_first_forward_handle_descending(owner_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target, value_bytes)
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
        if let Some(alias) =
            self.find_first_forward_handle_descending(alias_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, owner_vertex_id, value_bytes)
            })?
        {
            self.insert_edge_alias(alias, canonical, false);
        }
        Ok(canonical)
    }

    /// Directed outgoing edges at `vertex_id` in ascending slot order.
    pub fn directed_out_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge)
        })?;
        Ok(edges)
    }

    /// Directed incoming edges at `vertex_id` in ascending slot order.
    pub fn directed_in_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge)
        })?;
        Ok(edges)
    }

    /// Undirected edges at `vertex_id` in ascending slot order (forward store only).
    pub fn undirected_edges(&self, vertex_id: VertexId) -> Result<Vec<Edge>, GraphStoreError> {
        let mut edges = Vec::new();
        self.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            edges.push(edge);
        })?;
        Ok(edges)
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
    /// directed forward out-edges.
    pub(crate) fn skip_then_visit_each_directed_out_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
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
                    BucketDirectedness::Directed,
                    offset_remaining,
                    visit,
                )
            })
            .map_err(GraphStoreError::from)
    }

    /// Like [`Self::skip_then_visit_each_directed_out_edge`], but for undirected buckets only.
    pub(crate) fn skip_then_visit_each_undirected_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
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
                    BucketDirectedness::Undirected,
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
    /// directed incoming edges (reverse CSR).
    pub(crate) fn skip_then_visit_each_directed_in_edge<Visit, Err>(
        &self,
        vertex_id: VertexId,
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
                    BucketDirectedness::Directed,
                    offset_remaining,
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

    /// Directed outgoing edges for one catalog label (`EdgeLabelId` MSB ignored; wire key packed internally).
    pub fn for_each_directed_out_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// Directed outgoing edges for one catalog label; skips `ensure_vertex` on the hot path.
    pub fn for_each_directed_out_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// Directed incoming edges for one catalog label (reverse CSR; MSB packed internally).
    pub fn for_each_directed_in_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_in_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// Directed incoming edges for one catalog label; skips reverse vertex range validation.
    pub fn for_each_directed_in_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_in_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Directed),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// Undirected edges incident to `vertex_id` (forward out-adjacency only).
    pub fn for_each_undirected_edges_for_label<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_ordered(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Undirected),
            order,
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// Like [`Self::for_each_undirected_edges_for_label`], but skips `ensure_vertex`.
    pub fn for_each_undirected_edges_for_label_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        label: EdgeLabelId,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        self.for_each_out_edges_for_label_unchecked(
            vertex_id,
            wire_catalog_label(Some(label), EdgeDirectedness::Undirected),
            visit,
        )
        .map_err(GraphStoreError::from)
    }

    /// All directed outgoing edges at `vertex_id` (any catalog label, directed buckets only).
    pub fn for_each_directed_out_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_directed_out_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    /// All directed incoming edges at `vertex_id` (reverse store, directed buckets only).
    pub fn for_each_directed_in_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_directed_in_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    /// All undirected edges at `vertex_id` (forward out-adjacency, undirected buckets only).
    pub fn for_each_undirected_edges<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_undirected_edges(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    /// Like [`Self::for_each_undirected_edges`], but skips `ensure_vertex`.
    pub fn for_each_undirected_edges_unchecked<Visit>(
        &self,
        vertex_id: VertexId,
        order: OutEdgeOrder,
        visit: Visit,
    ) -> Result<(), GraphStoreError>
    where
        Visit: FnMut(Edge),
    {
        GRAPH
            .with_borrow(|graph| graph.for_each_undirected_edges_unchecked(vertex_id, order, visit))
            .map_err(GraphStoreError::from)
    }

    pub(crate) fn find_forward_edge_bucket_label(
        &self,
        owner_vertex_id: VertexId,
        edge: &Edge,
    ) -> Result<Option<LaraLabelId>, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.find_forward_edge_label(owner_vertex_id, edge))
    }

    /// Returns the first forward handle matching `pred` in descending slot order.
    ///
    /// Labeled iteration visits the highest slot index first, so this usually resolves the
    /// row written most recently when `pred` matches exactly one edge.
    fn find_first_forward_handle_descending<F>(
        &self,
        owner_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_for_label(owner_vertex_id, expected_label, |edge| {
                    if found.is_none() && pred(&edge) {
                        found = Some(EdgeHandle::at_slot(
                            owner_vertex_id,
                            expected_label,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    /// Like [`Self::find_first_forward_handle_descending`] on the reverse CSR store.
    fn find_first_reverse_handle_descending<F>(
        &self,
        row_vertex_id: VertexId,
        expected_label: LaraLabelId,
        mut pred: F,
    ) -> Result<Option<EdgeHandle>, GraphStoreError>
    where
        F: FnMut(&Edge) -> bool,
    {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label(row_vertex_id, expected_label, |edge| {
                    if found.is_none() && pred(&edge) {
                        found = Some(EdgeHandle::at_slot(
                            row_vertex_id,
                            expected_label,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    /// Pairs a newly inserted forward canonical row with the newest matching reverse-store row.
    fn find_reverse_alias_for_canonical(
        &self,
        canonical: EdgeHandle,
        target_vertex_id: VertexId,
        source_vertex_id: VertexId,
    ) -> Result<Option<EdgeHandle>, GraphStoreError> {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label(target_vertex_id, canonical.label_id, |edge| {
                    if found.is_none() && edge.neighbor_vid() == source_vertex_id {
                        found = Some(EdgeHandle::at_slot(
                            target_vertex_id,
                            canonical.label_id,
                            edge.edge_slot_index.raw(),
                        ));
                    }
                })
            })
            .map_err(GraphStoreError::from)
            .map(|()| found)
    }

    /// Resolves an undirected non-owner forward-half alias, if any.
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

    /// Resolves a reverse-IN CSR row alias to its owning forward canonical row.
    pub(crate) fn canonical_reverse_in_edge_handle(&self, handle: EdgeHandle) -> EdgeHandle {
        EDGE_ALIASES
            .with_borrow(|aliases| {
                aliases.get(
                    handle.owner_vertex_id,
                    handle.label_id.raw(),
                    edge_alias_slot_key(handle.slot_index, true),
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

    fn canonical_edge_handle_for_sidecar(&self, handle: EdgeHandle) -> EdgeHandle {
        let reverse = self.canonical_reverse_in_edge_handle(handle);
        if reverse != handle {
            return reverse;
        }
        self.canonical_edge_handle(handle)
    }

    fn insert_edge_alias(&self, alias: EdgeHandle, canonical: EdgeHandle, reverse_in: bool) {
        if alias.owner_vertex_id == canonical.owner_vertex_id
            && alias.label_id == canonical.label_id
            && alias.slot_index == canonical.slot_index
        {
            return;
        }
        debug_assert_eq!(alias.label_id, canonical.label_id);
        let alias_slot_key = edge_alias_slot_key(alias.slot_index, reverse_in);
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.insert(
                alias.owner_vertex_id,
                alias.label_id.raw(),
                alias_slot_key,
                canonical.owner_vertex_id,
                canonical.slot_index,
            );
        });
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
        self.assert_local_vertex_writable(vertex_id)?;
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
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;

        let mut to_clear: Vec<EdgeHandle> = Vec::new();
        let mut push_out = |edge: Edge| {
            self.unregister_remote_forward_in_for_out_edge(vertex_id, &edge);
            let owner = self.edge_sidecar_owner_from_out_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        };
        self.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            push_out(edge);
        })?;
        self.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            push_out(edge);
        })?;
        self.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            let owner = self.edge_sidecar_owner_from_in_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        })?;
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
        let canonical = self.canonical_edge_handle_for_sidecar(handle);
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let is_undirected = TaggedEdgeLabelId::from_raw(canonical.label_id.raw()).is_undirected();
        let alias = self.alias_for_canonical_edge(canonical);
        self.clear_edge_sidecars(handle);
        self.unregister_remote_forward_in_for_handle(canonical);
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
        let Some(EdgeTarget::Local(neighbor)) = edge.edge_target() else {
            self.drain_deferred_maintenance()?;
            return Ok(());
        };
        if is_undirected {
            if let Some((alias_vertex_id, alias_slot_index, _)) = alias {
                self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_at_slot(
                        alias_vertex_id,
                        canonical.label_id,
                        alias_slot_index,
                    )
                })?;
            } else {
                self.with_graph_mut(|graph| {
                    graph.remove_directed_deferred(
                        neighbor,
                        canonical.owner_vertex_id,
                        edge.with_neighbor_vid(canonical.owner_vertex_id),
                    )
                })?;
            }
        } else if let Some((alias_vertex_id, alias_slot_index, reverse_in)) = alias {
            debug_assert!(
                reverse_in,
                "directed aliases should point at reverse-IN rows"
            );
            self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot(
                    alias_vertex_id,
                    canonical.label_id,
                    alias_slot_index,
                )
            })?;
        } else {
            self.remove_reverse_edge_for_canonical_directed(
                neighbor,
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )?;
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    fn remove_reverse_edge_for_canonical_directed(
        &self,
        row_vertex_id: VertexId,
        owner_vertex_id: VertexId,
        label_id: LaraLabelId,
        forward_slot_index: u32,
    ) -> Result<(), GraphStoreError> {
        let removed = self.with_graph_mut(|graph| {
            graph.remove_reverse_edge_at_slot(row_vertex_id, label_id, forward_slot_index)
        })?;
        if removed.is_some() {
            return Ok(());
        }
        let mut sole_slot = None;
        let mut count = 0u32;
        self.with_graph_mut(|graph| {
            graph.for_each_in_edges_for_label(row_vertex_id, label_id, |edge| {
                if edge.neighbor_vid() == owner_vertex_id {
                    count = count.saturating_add(1);
                    sole_slot = Some(edge.edge_slot_index.raw());
                }
            })
        })?;
        if count == 1 {
            let _ = self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot(
                    row_vertex_id,
                    label_id,
                    sole_slot.expect("count == 1"),
                )
            })?;
        }
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

    fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
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

    fn alias_for_canonical_edge(&self, canonical: EdgeHandle) -> Option<(VertexId, u32, bool)> {
        EDGE_ALIASES.with_borrow(|aliases| {
            aliases
                .find_alias_for_canonical(
                    canonical.owner_vertex_id,
                    canonical.label_id.raw(),
                    canonical.slot_index,
                )
                .map(|(vertex_id, slot_key)| {
                    let (slot_index, reverse_in) = edge_alias_slot_key_parts(slot_key);
                    (vertex_id, slot_index, reverse_in)
                })
        })
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
                    for (property_id, value) in &moved_properties {
                        edge_equal::record_edge_property_change(
                            owner_vertex_id,
                            label_id,
                            moved.old_slot_index,
                            *property_id,
                            Some(value),
                            None,
                        );
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
                let label = LaraLabelId::from_raw(label_id);
                let _ = GRAPH.with_borrow(|graph| {
                    graph.for_each_out_edges_for_label_unchecked(owner_vertex_id, label, |edge| {
                        if edge.edge_slot_index.raw() != moved.new_slot_index {
                            return;
                        }
                        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
                            return;
                        };
                        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
                            index.move_slot(
                                remote_ref,
                                owner_vertex_id,
                                label_id,
                                moved.old_slot_index,
                                moved.new_slot_index,
                            );
                        });
                    })
                });
            }
            LabeledOrientation::Reverse => {
                EDGE_ALIASES.with_borrow_mut(|aliases| {
                    aliases.move_alias_key(
                        owner_vertex_id,
                        label_id,
                        edge_alias_slot_key(moved.old_slot_index, true),
                        edge_alias_slot_key(moved.new_slot_index, true),
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
        self.release_federated_vertex_placement_if_authoritative(vertex_id)?;

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

    fn release_federated_vertex_placement_if_authoritative(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(logical_vertex_id) = self.logical_vertex_id(vertex_id) else {
            return Ok(());
        };
        let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)?;
        let VertexPlacement::Active(loc) = placement else {
            return Ok(());
        };
        if loc.shard_id != routing.shard_id
            || loc.local_vertex_id != placement::local_vertex_id_raw(vertex_id)
        {
            return Ok(());
        }
        placement::release_logical_vertex_placement(
            routing.router_canister,
            ReleaseLogicalVertexArgs { logical_vertex_id },
        )?;
        VERTEX_LOGICAL_IDS.with_borrow_mut(|map| map.remove(vertex_id));
        Ok(())
    }

    fn lookup_forward_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_out_edges_for_label(
                    handle.owner_vertex_id,
                    handle.label_id,
                    |edge| {
                        if edge.edge_slot_index.raw() == handle.slot_index {
                            found = Some((edge, handle.label_id));
                        }
                    },
                )
            })
            .map_err(GraphStoreError::from)?;
        Ok(found)
    }

    fn lookup_reverse_out_edge(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        let mut found = None;
        GRAPH
            .with_borrow(|graph| {
                graph.for_each_in_edges_for_label(handle.owner_vertex_id, handle.label_id, |edge| {
                    if edge.edge_slot_index.raw() == handle.slot_index {
                        found = Some((edge, handle.label_id));
                    }
                })
            })
            .map_err(GraphStoreError::from)?;
        Ok(found)
    }

    fn lookup_edge_entry(
        &self,
        handle: EdgeHandle,
    ) -> Result<Option<(Edge, LaraLabelId)>, GraphStoreError> {
        if let Some(found) = self.lookup_forward_out_edge(handle)? {
            return Ok(Some(found));
        }
        let reverse_canonical = self.canonical_reverse_in_edge_handle(handle);
        if reverse_canonical != handle {
            if let Some(found) = self.lookup_forward_out_edge(reverse_canonical)? {
                return Ok(Some(found));
            }
        }
        let undirected_canonical = self.canonical_edge_handle(handle);
        if undirected_canonical != handle {
            if let Some(found) = self.lookup_forward_out_edge(undirected_canonical)? {
                return Ok(Some(found));
            }
        }
        if reverse_canonical != handle {
            return self.lookup_reverse_out_edge(reverse_canonical);
        }
        self.lookup_reverse_out_edge(handle)
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
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
    use std::collections::BTreeMap;

    #[test]
    fn peer_graph_canister_bootstrap_and_remove() {
        let store = GraphStore::new();
        let self_canister = Principal::self_authenticating([1u8; 32]);
        let peer_a = Principal::self_authenticating([2u8; 32]);
        let peer_b = Principal::self_authenticating([3u8; 32]);

        store.bootstrap_peer_graph_canisters(&[self_canister, peer_a, peer_b], self_canister);
        assert!(store.is_peer_graph_canister(&peer_a));
        assert!(store.is_peer_graph_canister(&peer_b));
        assert!(!store.is_peer_graph_canister(&self_canister));

        store.add_peer_graph_canister(peer_a, self_canister);
        assert!(store.remove_peer_graph_canister(&peer_a));
        assert!(!store.is_peer_graph_canister(&peer_a));
        assert!(store.is_peer_graph_canister(&peer_b));
    }

    #[test]
    fn inline_edge_values_round_trip_on_parallel_out_edges() {
        let store = GraphStore::new();
        let s = store.insert_vertex().expect("s");
        let a = store.insert_vertex().expect("a");
        let mid = store.insert_vertex().expect("mid");
        let dst = store.insert_vertex().expect("dst");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        store
            .insert_directed_edge_with_inline_value(s, mid, Some(label_id), 10)
            .expect("s->mid");
        store
            .insert_directed_edge_with_inline_value(s, a, Some(label_id), 5)
            .expect("s->a");
        store
            .insert_directed_edge_with_inline_value(a, mid, Some(label_id), 1)
            .expect("a->mid");
        store
            .insert_directed_edge_with_inline_value(mid, dst, Some(label_id), 0)
            .expect("mid->dst");
        let _ = dst;
        let mut weights = Vec::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(s, label_id, |edge| {
                weights.push(edge.inline_value_u16());
            })
            .expect("out edges");
        weights.sort_unstable();
        assert_eq!(weights, vec![5, 10]);
    }

    #[test]
    fn weighted_road_parallel_out_edges_from_a_round_trip() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        let c = store.insert_vertex().expect("c");
        let label_id = store
            .get_or_insert_edge_label_id("WgtRoad")
            .expect("road label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(label_id), 1)
            .expect("a->b");
        store
            .insert_directed_edge_with_inline_value(b, c, Some(label_id), 1)
            .expect("b->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(label_id), 100)
            .expect("a->c");
        let mut weights = Vec::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(a, label_id, |edge| {
                weights.push(edge.inline_value_u16());
            })
            .expect("out edges from a");
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 100]);
    }

    #[test]
    fn directed_out_edges_visit_attaches_inline_values() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let label_id = store
            .get_or_insert_edge_label_id("VisitWgtRoad")
            .expect("road label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        for weight in 1..=8u16 {
            let t = store.insert_vertex().expect("target");
            store
                .insert_directed_edge_with_inline_value(a, t, Some(label_id), weight)
                .expect("a->t");
        }
        let mut weights = Vec::new();
        store
            .for_each_directed_out_edges(a, OutEdgeOrder::Ascending, |edge| {
                weights.push(edge.inline_value_u16());
            })
            .expect("out edges");
        weights.sort_unstable();
        assert_eq!(weights, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn delete_valued_directed_edge_by_handle_removes_reverse_alias_slot() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = store
            .get_or_insert_edge_label_id("DeleteValuedDirected")
            .expect("label");

        let first = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[1, 0])
            .expect("first edge");
        let second = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[2, 0])
            .expect("second edge");

        assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);
        store.delete_edge_by_handle(first).expect("delete first");

        let in_edges = store.directed_in_edges(target).expect("in after");
        assert_eq!(in_edges.len(), 1);
        assert!(in_edges.iter().all(|edge| edge.neighbor_vid() == source));

        let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
        let reverse = store
            .find_first_reverse_handle_descending(target, wire_label, |edge| {
                edge.neighbor_vid() == source
            })
            .expect("reverse lookup")
            .expect("remaining reverse edge");
        assert_eq!(store.canonical_reverse_in_edge_handle(reverse), second);
    }

    #[test]
    fn directed_reverse_alias_does_not_require_matching_slot_index() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let other_source = store.insert_vertex().expect("other source");
        let label_id = store
            .get_or_insert_edge_label_id("DirectedAliasSlotSkew")
            .expect("label");

        store
            .insert_directed_edge_with_value_bytes(other_source, target, Some(label_id), &[7, 0])
            .expect("preexisting edge");
        let canonical = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[42, 0])
            .expect("skewed edge");

        let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
        let reverse = store
            .find_first_reverse_handle_descending(target, wire_label, |edge| {
                edge.neighbor_vid() == source
            })
            .expect("reverse lookup")
            .expect("reverse edge");
        assert_ne!(
            reverse.slot_index, canonical.slot_index,
            "test setup should force forward/reverse slot skew"
        );
        assert_eq!(store.canonical_reverse_in_edge_handle(reverse), canonical);

        let edge = store
            .find_outgoing_edge_record(reverse)
            .expect("edge lookup")
            .expect("canonicalized edge");
        assert_eq!(edge.value_bytes(), &[42, 0]);
    }

    #[test]
    fn delete_valued_undirected_edge_by_handle_removes_alias_slot() {
        let store = GraphStore::new();
        let low = store.insert_vertex().expect("low");
        let high = store.insert_vertex().expect("high");
        let label_id = store
            .get_or_insert_edge_label_id("DeleteValuedUndirected")
            .expect("label");

        let first = store
            .insert_undirected_edge_with_value_bytes(low, high, Some(label_id), &[1, 0])
            .expect("first edge");
        let second = store
            .insert_undirected_edge_with_value_bytes(low, high, Some(label_id), &[2, 0])
            .expect("second edge");

        store.delete_edge_by_handle(first).expect("delete first");

        let weights_from = |vertex| {
            let mut weights: Vec<u16> = store
                .undirected_edges(vertex)
                .expect("undirected edges")
                .into_iter()
                .map(|edge| edge.inline_value_u16())
                .collect();
            weights.sort_unstable();
            weights
        };
        assert_eq!(weights_from(low), vec![2]);
        assert_eq!(weights_from(high), vec![2]);

        let wire_label = lara_label(label_id.pack(EdgeDirectedness::Undirected));
        let alias = store
            .find_first_forward_handle_descending(low, wire_label, |edge| {
                edge.neighbor_vid() == high
            })
            .expect("alias lookup")
            .expect("remaining alias half");
        assert_eq!(store.canonical_edge_handle(alias), second);
    }

    #[test]
    fn unvalued_parallel_directed_inserts_align_reverse_alias_slot() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = store
            .get_or_insert_edge_label_id("UnvaluedParallelDirected")
            .expect("label");

        let first = store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("first edge");
        let second = store
            .insert_directed_edge(source, target, Some(label_id))
            .expect("second edge");
        assert_ne!(first.slot_index, second.slot_index);
        assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);

        store.delete_edge_by_handle(first).expect("delete first");

        let in_edges = store.directed_in_edges(target).expect("in after");
        assert_eq!(in_edges.len(), 1);
        assert_eq!(in_edges[0].edge_slot_index.raw(), second.slot_index);
    }

    #[test]
    fn valued_parallel_insert_returns_handles_for_each_value() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = store
            .get_or_insert_edge_label_id("ParallelValuedHandles")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");

        let first = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[1, 0])
            .expect("first edge");
        let second = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[2, 0])
            .expect("second edge");

        assert_ne!(first.slot_index, second.slot_index);
        let mut values_by_slot = BTreeMap::new();
        store
            .for_each_directed_out_edges_for_label_unchecked(source, label_id, |edge| {
                values_by_slot.insert(edge.edge_slot_index.raw(), edge.value_bytes().to_vec());
            })
            .expect("out edges");
        assert_eq!(values_by_slot[&first.slot_index], vec![1, 0]);
        assert_eq!(values_by_slot[&second.slot_index], vec![2, 0]);
    }

    #[test]
    fn lookup_edge_record_at_handle_includes_stored_value_bytes() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label_id = store
            .get_or_insert_edge_label_id("LookupEdgeRecordValue")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        let handle = store
            .insert_directed_edge_with_value_bytes(source, target, Some(label_id), &[4, 0])
            .expect("edge");
        let edge = store
            .find_outgoing_edge_record(handle)
            .expect("lookup")
            .expect("edge record");
        assert_eq!(edge.value_bytes(), &[4, 0]);
    }

    /// Regression: vertex `a` is target of `s->a` (reverse-IN alias) and source of `a->mid`
    /// (forward-OUT). Shared slot index `0` in both CSR stores must not alias across stores.
    #[test]
    fn forward_out_lookup_ignores_reverse_in_alias_when_slots_collide() {
        let store = GraphStore::new();
        let s = store.insert_vertex().expect("s");
        let a = store.insert_vertex().expect("a");
        let mid = store.insert_vertex().expect("mid");
        let label_id = store
            .get_or_insert_edge_label_id("ForwardOutReverseInSlotCollision")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");
        store
            .insert_directed_edge_with_value_bytes(s, a, Some(label_id), &[5, 0])
            .expect("s->a");
        let a_to_mid = store
            .insert_directed_edge_with_value_bytes(a, mid, Some(label_id), &[1, 0])
            .expect("a->mid");

        assert_eq!(
            store.canonical_edge_handle(a_to_mid),
            a_to_mid,
            "forward OUT handle must not resolve through reverse-IN alias"
        );
        let edge = store
            .find_outgoing_edge_record(a_to_mid)
            .expect("lookup")
            .expect("edge");
        assert_eq!(edge.value_bytes(), &[1, 0]);
    }

    #[test]
    fn valued_insert_after_delete_returns_handle_for_new_edge() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target_a = store.insert_vertex().expect("target a");
        let target_b = store.insert_vertex().expect("target b");
        let label_id = store
            .get_or_insert_edge_label_id("TombstoneHandleLookup")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("weight profile");

        let doomed = store
            .insert_directed_edge_with_value_bytes(source, target_a, Some(label_id), &[1, 0])
            .expect("doomed edge");
        store
            .insert_directed_edge_with_value_bytes(source, target_b, Some(label_id), &[2, 0])
            .expect("survivor edge");
        store.delete_edge_by_handle(doomed).expect("delete doomed");

        let replacement = store
            .insert_directed_edge_with_value_bytes(source, target_a, Some(label_id), &[9, 0])
            .expect("replacement edge");
        let edge = store
            .directed_out_edges(source)
            .expect("out edges")
            .into_iter()
            .find(|edge| edge.edge_slot_index.raw() == replacement.slot_index)
            .expect("replacement edge record");
        assert_eq!(edge.value_bytes(), &[9, 0]);
        assert_eq!(edge.neighbor_vid(), target_a);
        assert_eq!(
            store.directed_in_edges(target_a).expect("in edges").len(),
            1
        );
    }

    #[test]
    fn insert_edge_handle_lookup_is_scoped_to_expected_label() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let low_label = store
            .get_or_insert_edge_label_id("LookupLow")
            .expect("low label");
        let high_label = store
            .get_or_insert_edge_label_id("LookupHigh")
            .expect("high label");

        store
            .insert_directed_edge(source, target, Some(high_label))
            .expect("high edge");
        let low = store
            .insert_directed_edge(source, target, Some(low_label))
            .expect("low edge");

        assert_eq!(
            low.label_id,
            lara_label(edge_storage_label(Some(low_label), false))
        );
    }

    #[test]
    fn edge_label_lookup_uses_edge_label_annotation() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let directed_label = store
            .get_or_insert_edge_label_id("LookupDirected")
            .expect("directed label");
        let undirected_label = store
            .get_or_insert_edge_label_id("LookupUndirected")
            .expect("undirected label");
        store
            .insert_directed_edge(source, target, Some(directed_label))
            .expect("directed edge");
        let undirected = store
            .insert_undirected_edge(source, target, Some(undirected_label))
            .expect("undirected edge");

        let edge = store
            .undirected_edges(source)
            .expect("undirected edges")
            .into_iter()
            .find(|edge| edge.edge_slot_index.raw() == undirected.slot_index)
            .expect("inserted undirected edge");

        assert_eq!(
            store
                .find_forward_edge_bucket_label(source, &edge)
                .expect("find label"),
            Some(lara_label(edge_storage_label(Some(undirected_label), true)))
        );
        assert!(store.edge_is_undirected(source, &edge).unwrap());
    }

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

        let out_edges = store.directed_out_edges(source).expect("read out edges");
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

        let target_out_edges = store
            .undirected_edges(target)
            .expect("read target out edges");
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

        let out_edges = store.directed_out_edges(source).expect("out edges");
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
        assert!(store.directed_in_edges(b).expect("in").is_empty());
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
            .directed_out_edges(source)
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
        let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));

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

        assert_eq!(
            store.edge_property(third_edge, property),
            Some(Value::Int64(44)),
            "canonical forward handle keeps properties across reverse compaction"
        );

        let reverse_third = store
            .find_first_reverse_handle_descending(target, wire_label, |edge| {
                edge.neighbor_vid() == third
            })
            .expect("reverse lookup after compaction")
            .expect("third reverse edge after compaction");
        assert_eq!(
            store.canonical_reverse_in_edge_handle(reverse_third),
            third_edge,
            "reverse CSR slot should still alias the canonical forward handle"
        );
        assert_eq!(
            store.edge_property(reverse_third, property),
            Some(Value::Int64(44))
        );
    }
}
