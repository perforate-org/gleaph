use super::edge_alias::EdgeAliasIndex;
use super::edge_label_catalog::EdgeLabelCatalog;
use super::edge_properties::EdgePropertyStore;
use super::edge_value_profiles::EdgeValueProfileStore;
use super::edge_weight_profiles::EdgeWeightProfileStore;
use super::metadata::{GraphMetadata, StableGraphMetadata};
use super::property_catalog::PropertyCatalog;
use super::vertex_label_catalog::VertexLabelCatalog;
use super::vertex_labels::VertexLabelStore;
use super::vertex_properties::VertexPropertyStore;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledLaraGraph,
    lara::maintenance::DeferredConfig,
};
use ic_stable_structures::DefaultMemoryImpl;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use std::cell::RefCell;

// --- Labeled graph: forward orientation (10 memories) ---
const FWD_VERTICES: MemoryId = MemoryId::new(0);
const FWD_BUCKETS: MemoryId = MemoryId::new(1);
const FWD_BUCKET_FREE_SPANS: MemoryId = MemoryId::new(2);
const FWD_BUCKET_FREE_SPAN_BY_START: MemoryId = MemoryId::new(3);
const FWD_EDGE_COUNTS: MemoryId = MemoryId::new(4);
const FWD_EDGES: MemoryId = MemoryId::new(5);
const FWD_EDGE_LOG: MemoryId = MemoryId::new(6);
const FWD_EDGE_SPAN_META: MemoryId = MemoryId::new(7);
const FWD_EDGE_FREE_SPANS: MemoryId = MemoryId::new(8);
const FWD_EDGE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(9);
const FWD_VALUE_SLAB: MemoryId = MemoryId::new(42);
const FWD_VALUE_LOG: MemoryId = MemoryId::new(49);
const FWD_VALUE_FREE_SPANS: MemoryId = MemoryId::new(45);
const FWD_VALUE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(46);

// --- Labeled graph: reverse orientation (10 memories) ---
const REV_VERTICES: MemoryId = MemoryId::new(10);
const REV_BUCKETS: MemoryId = MemoryId::new(11);
const REV_BUCKET_FREE_SPANS: MemoryId = MemoryId::new(12);
const REV_BUCKET_FREE_SPAN_BY_START: MemoryId = MemoryId::new(13);
const REV_EDGE_COUNTS: MemoryId = MemoryId::new(14);
const REV_EDGES: MemoryId = MemoryId::new(15);
const REV_EDGE_LOG: MemoryId = MemoryId::new(16);
const REV_EDGE_SPAN_META: MemoryId = MemoryId::new(17);
const REV_EDGE_FREE_SPANS: MemoryId = MemoryId::new(18);
const REV_EDGE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(19);
const REV_VALUE_SLAB: MemoryId = MemoryId::new(43);
const REV_VALUE_LOG: MemoryId = MemoryId::new(50);
const REV_VALUE_FREE_SPANS: MemoryId = MemoryId::new(47);
const REV_VALUE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(48);

const MAINTENANCE_QUEUE: MemoryId = MemoryId::new(20);
const DIRTY_WORK_ITEMS: MemoryId = MemoryId::new(21);

const VERTEX_LABEL_NAME_TO_ID: MemoryId = MemoryId::new(22);
const VERTEX_LABEL_ID_TO_NAME: MemoryId = MemoryId::new(23);
const EDGE_LABEL_NAME_TO_ID: MemoryId = MemoryId::new(34);
const EDGE_LABEL_ID_TO_NAME: MemoryId = MemoryId::new(35);
const VERTEX_LABEL_SETS: MemoryId = MemoryId::new(24);
const PROPERTY_NAME_TO_ID: MemoryId = MemoryId::new(25);
const PROPERTY_ID_TO_NAME: MemoryId = MemoryId::new(26);
const VERTEX_PROPERTIES: MemoryId = MemoryId::new(27);
const EDGE_PROPERTIES: MemoryId = MemoryId::new(28);
const EDGE_ALIASES: MemoryId = MemoryId::new(29);
const GRAPH_METADATA: MemoryId = MemoryId::new(32);
const EDGE_WEIGHT_PROFILES: MemoryId = MemoryId::new(33);
const EDGE_VALUE_PROFILES: MemoryId = MemoryId::new(44);
const VERTEX_LOGICAL_IDS: MemoryId = MemoryId::new(36);
const REMOTE_REF_TO_LOGICAL: MemoryId = MemoryId::new(37);
const LOGICAL_TO_REMOTE_REF: MemoryId = MemoryId::new(38);
const REMOTE_FORWARD_IN: MemoryId = MemoryId::new(39);
const EDGE_EQUALITY_POSTINGS: MemoryId = MemoryId::new(40);
const PEER_GRAPH_CANISTERS: MemoryId = MemoryId::new(41);

pub(crate) const GRAPH_DEFAULT_EDGE_LABEL: LaraLabelId = LaraLabelId::UNLABELED_DIRECTED;

/// Initial slab capacity for both labeled orientations (grows as needed).
const GRAPH_ELEM_CAPACITY: u64 = 1 << 20;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) type StableGraph = DeferredBidirectionalLabeledLaraGraph<Edge, Memory>;
pub(crate) type StableVertexLabelCatalog = VertexLabelCatalog<Memory, Memory>;
pub(crate) type StableEdgeLabelCatalog = EdgeLabelCatalog<Memory, Memory>;
pub(crate) type StableVertexLabelStore = VertexLabelStore<Memory>;
pub(crate) type StablePropertyCatalog = PropertyCatalog<Memory, Memory>;
pub(crate) type StableVertexPropertyStore = VertexPropertyStore<Memory>;
pub(crate) type StableEdgePropertyStore = EdgePropertyStore<Memory>;
pub(crate) type StableEdgeAliasIndex = EdgeAliasIndex<Memory>;
pub(crate) type StableMetadata = StableGraphMetadata<Memory>;
pub(crate) type StableEdgeWeightProfileStore = EdgeWeightProfileStore<Memory>;
pub(crate) type StableEdgeValueProfileStore = EdgeValueProfileStore<Memory>;
pub(crate) type StableVertexLogicalIdMap = super::vertex_logical_ids::VertexLogicalIdMap<Memory>;
pub(crate) type StableRemoteVertexRefTable =
    super::remote_vertex_refs::RemoteVertexRefTable<Memory>;
pub(crate) type StableRemoteForwardInIndex = super::remote_forward_in::RemoteForwardInIndex<Memory>;
pub(crate) type StableEdgeEqualityPostingStore =
    super::edge_equality_postings::EdgeEqualityPostingStore<Memory>;
pub(crate) type StablePeerGraphCanisterSet =
    super::peer_graph_canisters::PeerGraphCanisterSet<Memory>;

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) fn init_graph() -> StableGraph {
    let graph = DeferredBidirectionalLabeledLaraGraph::init_with_config(
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKETS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKET_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKET_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VALUE_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VALUE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VALUE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VALUE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKETS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKET_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKET_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VALUE_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VALUE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VALUE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VALUE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(MAINTENANCE_QUEUE)),
        MEMORY_MANAGER.with(|m| m.borrow().get(DIRTY_WORK_ITEMS)),
        GRAPH_ELEM_CAPACITY,
        GRAPH_DEFAULT_EDGE_LABEL,
        DeferredConfig::default(),
    )
    .unwrap();

    crate::facade::init_ic_gql_extensions();

    graph
}

pub(crate) fn init_vertex_label_catalog() -> StableVertexLabelCatalog {
    VertexLabelCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LABEL_NAME_TO_ID)),
        MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LABEL_ID_TO_NAME)),
    )
}

pub(crate) fn init_edge_label_catalog() -> StableEdgeLabelCatalog {
    EdgeLabelCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_LABEL_NAME_TO_ID)),
        MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_LABEL_ID_TO_NAME)),
    )
}

pub(crate) fn init_vertex_label_store() -> StableVertexLabelStore {
    VertexLabelStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LABEL_SETS)))
}

pub(crate) fn init_property_catalog() -> StablePropertyCatalog {
    PropertyCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(PROPERTY_NAME_TO_ID)),
        MEMORY_MANAGER.with(|m| m.borrow().get(PROPERTY_ID_TO_NAME)),
    )
}

pub(crate) fn init_vertex_property_store() -> StableVertexPropertyStore {
    VertexPropertyStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_PROPERTIES)))
}

pub(crate) fn init_edge_property_store() -> StableEdgePropertyStore {
    EdgePropertyStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_PROPERTIES)))
}

pub(crate) fn init_edge_alias_index() -> StableEdgeAliasIndex {
    EdgeAliasIndex::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_ALIASES)))
}

pub(crate) fn init_edge_weight_profiles() -> StableEdgeWeightProfileStore {
    EdgeWeightProfileStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_WEIGHT_PROFILES)))
}

pub(crate) fn init_edge_value_profiles() -> StableEdgeValueProfileStore {
    EdgeValueProfileStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_VALUE_PROFILES)))
}

pub(crate) fn init_metadata() -> StableMetadata {
    StableGraphMetadata::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(GRAPH_METADATA)),
        GraphMetadata::default(),
    )
}

pub(crate) fn init_vertex_logical_ids() -> StableVertexLogicalIdMap {
    super::vertex_logical_ids::VertexLogicalIdMap::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LOGICAL_IDS)),
    )
}

pub(crate) fn init_remote_vertex_refs() -> StableRemoteVertexRefTable {
    super::remote_vertex_refs::RemoteVertexRefTable::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(REMOTE_REF_TO_LOGICAL)),
        MEMORY_MANAGER.with(|m| m.borrow().get(LOGICAL_TO_REMOTE_REF)),
    )
}

pub(crate) fn init_remote_forward_in() -> StableRemoteForwardInIndex {
    super::remote_forward_in::RemoteForwardInIndex::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(REMOTE_FORWARD_IN)),
    )
}

pub(crate) fn init_edge_equality_postings() -> StableEdgeEqualityPostingStore {
    super::edge_equality_postings::EdgeEqualityPostingStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_EQUALITY_POSTINGS)),
    )
}

pub(crate) fn init_peer_graph_canisters() -> StablePeerGraphCanisterSet {
    super::peer_graph_canisters::PeerGraphCanisterSet::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(PEER_GRAPH_CANISTERS)),
    )
}
