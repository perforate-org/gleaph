//! Router canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).

use super::edge_payload_profiles::EdgePayloadProfileStore;
use candid::Principal;
use gleaph_graph_kernel::bidirectional_catalog::{
    BidirectionalCatalog, DenseEdgeLabelPolicy, DenseMaxPlusOnePolicy,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::{
    BackfillShardState, GlobalVertexId, ShardId, ShardRegistryEntry, VertexPlacement,
};

use gleaph_auth::AuthState;
use gleaph_gql_ic::graph_registry::GraphRegistryEntry;

use super::indexed_catalog::{IndexDefRecord, IndexedPropertyKey, NamedIndexKey};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, BTreeSet, Cell, DefaultMemoryImpl};
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

const ROUTER_CONTROLLERS: MemoryId = MemoryId::new(0);
const ROUTER_GRAPHS: MemoryId = MemoryId::new(1);
const ROUTER_SHARDS: MemoryId = MemoryId::new(2);
const ROUTER_SHARD_BY_GRAPH: MemoryId = MemoryId::new(3);
const ROUTER_PLACEMENTS: MemoryId = MemoryId::new(4);
const ROUTER_VERTEX_LABEL_BY_NAME: MemoryId = MemoryId::new(5);
const ROUTER_VERTEX_LABEL_BY_ID: MemoryId = MemoryId::new(6);
const ROUTER_EDGE_LABEL_BY_NAME: MemoryId = MemoryId::new(7);
const ROUTER_EDGE_LABEL_BY_ID: MemoryId = MemoryId::new(8);
const ROUTER_PROPERTY_BY_NAME: MemoryId = MemoryId::new(9);
const ROUTER_PROPERTY_BY_ID: MemoryId = MemoryId::new(10);
const ROUTER_AUTH_PRINCIPAL_RECORDS: MemoryId = MemoryId::new(11);
const ROUTER_VERTEX_LABEL_STATS: MemoryId = MemoryId::new(12);
const ROUTER_EDGE_LABEL_STATS: MemoryId = MemoryId::new(13);
const ROUTER_VERTEX_LABEL_LIVE_BY_SHARD: MemoryId = MemoryId::new(14);
const ROUTER_EDGE_LABEL_LIVE_BY_SHARD: MemoryId = MemoryId::new(15);
const ROUTER_MUTATION_COUNTER: MemoryId = MemoryId::new(16);
const ROUTER_APPLIED_LABEL_TELEMETRY: MemoryId = MemoryId::new(17);
const ROUTER_MUTATION_BY_CLIENT_KEY: MemoryId = MemoryId::new(18);
const ROUTER_LABEL_BACKFILL_STATE: MemoryId = MemoryId::new(19);
const ROUTER_PROPERTY_BACKFILL_STATE: MemoryId = MemoryId::new(20);
const ROUTER_EDGE_PAYLOAD_PROFILES: MemoryId = MemoryId::new(21);
const ROUTER_NAMED_INDEXES: MemoryId = MemoryId::new(22);
const ROUTER_INDEXED_PROPERTY_SET: MemoryId = MemoryId::new(23);

pub(crate) type StableControllerSet = BTreeSet<Principal, Memory>;
pub(crate) type StableGraphRegistry = BTreeMap<String, GraphRegistryEntry, Memory>;
pub(crate) type StableShardRegistry = BTreeMap<ShardId, ShardRegistryEntry, Memory>;
pub(crate) type StableShardByGraph = BTreeMap<Principal, ShardId, Memory>;
pub(crate) type StablePlacementMap = BTreeMap<GlobalVertexId, VertexPlacement, Memory>;
pub(crate) type StableVertexLabelCatalog =
    BidirectionalCatalog<VertexLabelId, Memory, Memory, DenseMaxPlusOnePolicy>;
pub(crate) type StableEdgeLabelCatalog =
    BidirectionalCatalog<EdgeLabelId, Memory, Memory, DenseEdgeLabelPolicy>;
pub(crate) type StablePropertyCatalog =
    BidirectionalCatalog<PropertyId, Memory, Memory, DenseMaxPlusOnePolicy>;
pub(crate) type StableEdgePayloadProfileStore = EdgePayloadProfileStore<Memory>;
pub(crate) type StableNamedIndexMap = BTreeMap<NamedIndexKey, IndexDefRecord, Memory>;
pub(crate) type StableIndexedPropertySet = BTreeSet<IndexedPropertyKey, Memory>;
pub(crate) type StableLabelStatsMap = BTreeMap<u16, super::label_telemetry::LabelStats, Memory>;
pub(crate) type StableLabelShardLiveMap =
    BTreeMap<super::label_telemetry::LabelShardKey, u64, Memory>;
pub(crate) type StableAppliedLabelTelemetrySet =
    BTreeSet<super::label_telemetry::AppliedLabelTelemetryKey, Memory>;
pub(crate) type StableMutationByClientKey = BTreeMap<
    super::label_telemetry::ClientMutationKey,
    super::label_telemetry::RouterMutationRecord,
    Memory,
>;
pub(crate) type StableLabelBackfillStateMap = BTreeMap<ShardId, BackfillShardState, Memory>;
pub(crate) type StablePropertyBackfillStateMap = BTreeMap<ShardId, BackfillShardState, Memory>;
pub(crate) type StableMutationCounter = Cell<u64, Memory>;
pub(crate) type StableAuthState = AuthState<Memory>;

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) fn init_controllers() -> StableControllerSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_CONTROLLERS)))
}

pub(crate) fn init_graphs() -> StableGraphRegistry {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_GRAPHS)))
}

pub(crate) fn init_shards() -> StableShardRegistry {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SHARDS)))
}

pub(crate) fn init_shard_by_graph() -> StableShardByGraph {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_SHARD_BY_GRAPH)))
}

pub(crate) fn init_placements() -> StablePlacementMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PLACEMENTS)))
}

pub(crate) fn init_vertex_label_catalog() -> StableVertexLabelCatalog {
    BidirectionalCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_ID)),
    )
}

pub(crate) fn init_edge_label_catalog() -> StableEdgeLabelCatalog {
    BidirectionalCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_ID)),
    )
}

pub(crate) fn init_vertex_label_stats() -> StableLabelStatsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_STATS)))
}

pub(crate) fn init_edge_label_stats() -> StableLabelStatsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_STATS)))
}

pub(crate) fn init_vertex_label_live_by_shard() -> StableLabelShardLiveMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_LIVE_BY_SHARD)))
}

pub(crate) fn init_edge_label_live_by_shard() -> StableLabelShardLiveMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_LIVE_BY_SHARD)))
}

pub(crate) fn init_mutation_counter() -> StableMutationCounter {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_MUTATION_COUNTER)),
        0u64,
    )
}

pub(crate) fn init_applied_label_telemetry() -> StableAppliedLabelTelemetrySet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_APPLIED_LABEL_TELEMETRY)))
}

pub(crate) fn init_mutation_by_client_key() -> StableMutationByClientKey {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_MUTATION_BY_CLIENT_KEY)))
}

pub(crate) fn init_property_catalog() -> StablePropertyCatalog {
    BidirectionalCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_NAME)),
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_ID)),
    )
}

pub(crate) fn init_auth_state() -> StableAuthState {
    AuthState::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_AUTH_PRINCIPAL_RECORDS)))
}

pub(crate) fn init_label_backfill_state() -> StableLabelBackfillStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_LABEL_BACKFILL_STATE)))
}

pub(crate) fn init_property_backfill_state() -> StablePropertyBackfillStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BACKFILL_STATE)))
}

pub(crate) fn init_edge_payload_profiles() -> StableEdgePayloadProfileStore {
    EdgePayloadProfileStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_PAYLOAD_PROFILES)),
    )
}

pub(crate) fn init_named_indexes() -> StableNamedIndexMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_NAMED_INDEXES)))
}

pub(crate) fn init_indexed_property_set() -> StableIndexedPropertySet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_INDEXED_PROPERTY_SET)))
}
