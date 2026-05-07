use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::{DeferredBidirectionalLaraGraph as Graph, lara::maintenance::DeferredConfig};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap};
use std::cell::RefCell;

// Graph
const FORWARD_VERTICES: MemoryId = MemoryId::new(0);
const FORWARD_COUNTS: MemoryId = MemoryId::new(1);
const FORWARD_EDGES: MemoryId = MemoryId::new(2);
const FORWARD_LOG: MemoryId = MemoryId::new(3);
const FORWARD_SPAN_META: MemoryId = MemoryId::new(4);
const FORWARD_FREE_SPANS: MemoryId = MemoryId::new(5);
const FORWARD_FREE_SPAN_BY_START: MemoryId = MemoryId::new(6);
const REVERSE_VERTICES: MemoryId = MemoryId::new(7);
const REVERSE_COUNTS: MemoryId = MemoryId::new(8);
const REVERSE_EDGES: MemoryId = MemoryId::new(9);
const REVERSE_LOG: MemoryId = MemoryId::new(10);
const REVERSE_SPAN_META: MemoryId = MemoryId::new(11);
const REVERSE_FREE_SPANS: MemoryId = MemoryId::new(12);
const REVERSE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(13);
const MAINTENANCE_QUEUE: MemoryId = MemoryId::new(14);
const DIRTY_WORK_ITEMS: MemoryId = MemoryId::new(15);

pub(super) type Memory = VirtualMemory<DefaultMemoryImpl>;

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(super) fn init_graph() -> Graph<Edge, Vertex, Memory> {
    Graph::init_with_config(
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FORWARD_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REVERSE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(MAINTENANCE_QUEUE)),
        MEMORY_MANAGER.with(|m| m.borrow().get(DIRTY_WORK_ITEMS)),
        DeferredConfig::default(),
    )
    .unwrap()
}
