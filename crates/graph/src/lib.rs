#![cfg_attr(test, feature(f128))]

use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

mod stable;

pub mod facade;
pub mod plan;

pub use facade::{
    GRAPH_TIMER_LARA_MAX_INSTRUCTIONS, GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS, GraphStore,
    IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT, timer_lara_maintenance_budget,
};

thread_local! {
    static GRAPH: RefCell<Graph<Edge, Vertex, stable::memory::Memory>> = RefCell::new(
        stable::memory::init_graph()
    );

    static LABEL_CATALOG: RefCell<stable::memory::StableLabelCatalog> = RefCell::new(
        stable::memory::init_label_catalog()
    );

    static VERTEX_LABELS: RefCell<stable::memory::StableVertexLabelStore> = RefCell::new(
        stable::memory::init_vertex_label_store()
    );

    static PROPERTY_CATALOG: RefCell<stable::memory::StablePropertyCatalog> = RefCell::new(
        stable::memory::init_property_catalog()
    );

    static VERTEX_PROPERTIES: RefCell<stable::memory::StableVertexPropertyStore> = RefCell::new(
        stable::memory::init_vertex_property_store()
    );

    static EDGE_PROPERTIES: RefCell<stable::memory::StableEdgePropertyStore> = RefCell::new(
        stable::memory::init_edge_property_store()
    );

    static VERTEX_EDGE_IDS: RefCell<stable::memory::StableVertexEdgeIdAllocator> = RefCell::new(
        stable::memory::init_vertex_edge_id_allocator()
    );
}
