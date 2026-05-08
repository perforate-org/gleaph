use crate::label_catalog::LabelCatalog;
use crate::property_catalog::PropertyCatalog;
use crate::vertex_labels::VertexLabelStore;
use crate::vertex_properties::VertexPropertyStore;
use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::{lara::maintenance::DeferredConfig, DeferredBidirectionalLaraGraph as Graph};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::DefaultMemoryImpl;
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
const LABEL_NAME_TO_ID: MemoryId = MemoryId::new(16);
const LABEL_ID_TO_NAME: MemoryId = MemoryId::new(17);
const VERTEX_LABEL_SETS: MemoryId = MemoryId::new(18);
const PROPERTY_NAME_TO_ID: MemoryId = MemoryId::new(19);
const PROPERTY_ID_TO_NAME: MemoryId = MemoryId::new(20);
const VERTEX_PROPERTIES: MemoryId = MemoryId::new(21);

const GRAPH_ELEM_CAPACITY: u64 = 0;
const GRAPH_SEGMENT_SIZE: u32 = 32;
const GRAPH_INITIAL_VERTEX_EDGE_SLOTS: u32 = 0;

pub(super) type Memory = VirtualMemory<DefaultMemoryImpl>;
pub(super) type StableLabelCatalog = LabelCatalog<Memory, Memory>;
pub(super) type StableVertexLabelStore = VertexLabelStore<Memory>;
pub(super) type StablePropertyCatalog = PropertyCatalog<Memory, Memory>;
pub(super) type StableVertexPropertyStore = VertexPropertyStore<Memory>;

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
        GRAPH_ELEM_CAPACITY,
        GRAPH_SEGMENT_SIZE,
        GRAPH_INITIAL_VERTEX_EDGE_SLOTS,
        DeferredConfig::default(),
    )
    .unwrap()
}

pub(super) fn init_label_catalog() -> StableLabelCatalog {
    LabelCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_NAME_TO_ID)),
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_ID_TO_NAME)),
    )
}

pub(super) fn init_vertex_label_store() -> StableVertexLabelStore {
    VertexLabelStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LABEL_SETS)))
}

pub(super) fn init_property_catalog() -> StablePropertyCatalog {
    PropertyCatalog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(PROPERTY_NAME_TO_ID)),
        MEMORY_MANAGER.with(|m| m.borrow().get(PROPERTY_ID_TO_NAME)),
    )
}

pub(super) fn init_vertex_property_store() -> StableVertexPropertyStore {
    VertexPropertyStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_PROPERTIES)))
}
