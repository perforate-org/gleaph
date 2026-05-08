use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

pub mod edge_properties;
pub mod label_catalog;
mod memory;
pub mod property_catalog;
pub mod vertex_labels;
pub mod vertex_properties;

thread_local! {
    static GRAPH: RefCell<Graph<Edge, Vertex, memory::Memory>> = RefCell::new(
        memory::init_graph()
    );

    static LABEL_CATALOG: RefCell<memory::StableLabelCatalog> = RefCell::new(
        memory::init_label_catalog()
    );

    static VERTEX_LABELS: RefCell<memory::StableVertexLabelStore> = RefCell::new(
        memory::init_vertex_label_store()
    );

    static PROPERTY_CATALOG: RefCell<memory::StablePropertyCatalog> = RefCell::new(
        memory::init_property_catalog()
    );

    static VERTEX_PROPERTIES: RefCell<memory::StableVertexPropertyStore> = RefCell::new(
        memory::init_vertex_property_store()
    );

    static EDGE_PROPERTIES: RefCell<memory::StableEdgePropertyStore> = RefCell::new(
        memory::init_edge_property_store()
    );
}
