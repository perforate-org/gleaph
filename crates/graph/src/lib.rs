use crate::memory::Memory;
use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

mod memory;

thread_local! {
    static GRAPH: RefCell<Graph<Edge, Vertex, Memory>> = RefCell::new(
        memory::init_graph()
    )
}
