use gleaph_graph_kernel::entry::{Edge, LabelId, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

pub mod label_catalog;
mod memory;

thread_local! {
    static GRAPH: RefCell<Graph<Edge, Vertex, memory::Memory>> = RefCell::new(
        memory::init_graph()
    );

    static LABEL_CATALOG: RefCell<memory::StableLabelCatalog> = RefCell::new(
        memory::init_label_catalog()
    );
}
