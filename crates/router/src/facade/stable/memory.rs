//! Stable-memory layout for the router canister.

use candid::Principal;
use gleaph_graph_kernel::federation::{LogicalVertexId, ShardId, ShardRegistryEntry, VertexPlacement};

use super::storable::StoredGraphRegistryEntry;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, BTreeSet, Cell, DefaultMemoryImpl};
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

const ROUTER_CONTROLLERS: MemoryId = MemoryId::new(0);
const ROUTER_GRAPHS: MemoryId = MemoryId::new(1);
const ROUTER_SHARDS: MemoryId = MemoryId::new(2);
const ROUTER_SHARD_BY_GRAPH: MemoryId = MemoryId::new(3);
const ROUTER_PLACEMENTS: MemoryId = MemoryId::new(4);
const ROUTER_LOGICAL_COUNTER: MemoryId = MemoryId::new(5);
const ROUTER_PENDING_LOGICAL: MemoryId = MemoryId::new(6);
const ROUTER_VERTEX_LABEL_BY_NAME: MemoryId = MemoryId::new(7);
const ROUTER_VERTEX_LABEL_BY_ID: MemoryId = MemoryId::new(8);
const ROUTER_EDGE_LABEL_BY_NAME: MemoryId = MemoryId::new(9);
const ROUTER_EDGE_LABEL_BY_ID: MemoryId = MemoryId::new(10);
const ROUTER_PROPERTY_BY_NAME: MemoryId = MemoryId::new(11);
const ROUTER_PROPERTY_BY_ID: MemoryId = MemoryId::new(12);

pub(crate) type StableControllerSet = BTreeSet<Principal, Memory>;
pub(crate) type StableGraphRegistry = BTreeMap<String, StoredGraphRegistryEntry, Memory>;
pub(crate) type StableShardRegistry = BTreeMap<ShardId, ShardRegistryEntry, Memory>;
pub(crate) type StableShardByGraph = BTreeMap<Principal, ShardId, Memory>;
pub(crate) type StablePlacementMap = BTreeMap<LogicalVertexId, VertexPlacement, Memory>;
pub(crate) type StableLogicalCounter = Cell<u64, Memory>;
pub(crate) type StablePendingLogical = BTreeMap<Principal, LogicalVertexId, Memory>;
pub(crate) type StableLabelNameIntern = BTreeMap<String, u16, Memory>;
pub(crate) type StableLabelIdReverse = BTreeMap<u16, String, Memory>;
pub(crate) type StablePropertyNameIntern = BTreeMap<String, u32, Memory>;
pub(crate) type StablePropertyIdReverse = BTreeMap<u32, String, Memory>;

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

pub(crate) fn init_logical_counter() -> StableLogicalCounter {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_LOGICAL_COUNTER)),
        0u64,
    )
}

pub(crate) fn init_pending_logical() -> StablePendingLogical {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PENDING_LOGICAL)))
}

pub(crate) fn init_vertex_label_by_name() -> StableLabelNameIntern {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_NAME)))
}

pub(crate) fn init_vertex_label_by_id() -> StableLabelIdReverse {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_VERTEX_LABEL_BY_ID)))
}

pub(crate) fn init_edge_label_by_name() -> StableLabelNameIntern {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_NAME)))
}

pub(crate) fn init_edge_label_by_id() -> StableLabelIdReverse {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_EDGE_LABEL_BY_ID)))
}

pub(crate) fn init_property_by_name() -> StablePropertyNameIntern {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_NAME)))
}

pub(crate) fn init_property_by_id() -> StablePropertyIdReverse {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(ROUTER_PROPERTY_BY_ID)))
}
