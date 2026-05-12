//! Stable-memory-backed graph fragments (catalogs, property stores, id allocator, init).
//!
//! Module visibility is `pub(in crate::facade)` (see `facade.rs`): only code under `facade`
//! (notably [`super::store`] and [`super::auth`]) may reference this module directly. Stable-backed
//! error types are re-exported at the `facade` root for public `GraphStore` signatures.

use std::cell::RefCell;

pub(crate) mod memory;

pub(crate) mod edge_ids;
pub(crate) mod edge_properties;
pub(crate) mod label_catalog;
pub(crate) mod metadata;
pub(crate) mod property_catalog;
pub(crate) mod vertex_labels;
pub(crate) mod vertex_properties;

thread_local! {
    pub(crate) static GRAPH: RefCell<memory::StableGraph> = RefCell::new(
        memory::init_graph()
    );

    pub(crate) static LABEL_CATALOG: RefCell<memory::StableLabelCatalog> = RefCell::new(
        memory::init_label_catalog()
    );

    pub(crate) static VERTEX_LABELS: RefCell<memory::StableVertexLabelStore> = RefCell::new(
        memory::init_vertex_label_store()
    );

    pub(crate) static PROPERTY_CATALOG: RefCell<memory::StablePropertyCatalog> = RefCell::new(
        memory::init_property_catalog()
    );

    pub(crate) static VERTEX_PROPERTIES: RefCell<memory::StableVertexPropertyStore> = RefCell::new(
        memory::init_vertex_property_store()
    );

    pub(crate) static EDGE_PROPERTIES: RefCell<memory::StableEdgePropertyStore> = RefCell::new(
        memory::init_edge_property_store()
    );

    pub(crate) static VERTEX_EDGE_IDS: RefCell<memory::StableVertexEdgeIdAllocator> = RefCell::new(
        memory::init_vertex_edge_id_allocator()
    );

    pub(crate) static AUTH_STATE: RefCell<memory::StableAuthState> =
        RefCell::new(memory::init_auth_state());

    pub(crate) static PREPARED_QUERY_CATALOG: RefCell<memory::StablePreparedQueryCatalog> = RefCell::new(
        memory::init_prepared_query_catalog()
    );

    pub(crate) static METADATA: RefCell<memory::StableMetadata> = RefCell::new(
        memory::init_metadata()
    );
}
