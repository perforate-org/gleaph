use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

pub mod label_catalog;
mod memory;
pub mod vertex_labels;

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
}
