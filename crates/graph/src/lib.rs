#![cfg_attr(test, feature(f128))]

use gleaph_graph_kernel::entry::{Edge, Vertex};
use ic_stable_lara::DeferredBidirectionalLaraGraph as Graph;
use std::cell::RefCell;

pub mod edge_ids;
pub mod edge_properties;
pub mod label_catalog;
mod memory;
pub mod mutation_executor;
mod plan_mutation_error;
mod plan_property_expr_evaluator;
pub mod plan_mutation_executor;
pub mod property_catalog;
pub mod store;
pub mod vertex_labels;
pub mod vertex_properties;

pub use mutation_executor::GraphMutationExecutor;
pub use plan_mutation_error::PlanMutationError;
pub use plan_mutation_executor::{PlanMutationBindings, PlanMutationExecutor};
pub use plan_property_expr_evaluator::{PlanPropertyExprEvaluation, PlanPropertyExprEvaluator};
pub use store::GraphStore;

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

    static VERTEX_EDGE_IDS: RefCell<memory::StableVertexEdgeIdAllocator> = RefCell::new(
        memory::init_vertex_edge_id_allocator()
    );
}
