//! Stable-memory-backed graph fragments (catalogs, property stores, id allocator, init).
//!
//! Module visibility is `pub(in crate::facade)` (see `facade.rs`): only code under `facade`
//! (notably [`super::store`]) may reference this module directly. Stable-backed
//! error types are re-exported at the `facade` root for public `GraphStore` signatures.

use std::cell::RefCell;

pub(crate) mod memory;

pub(crate) mod edge_alias;
pub(crate) mod edge_equality_postings;
pub(crate) mod edge_label_catalog;
pub(crate) mod edge_properties;
pub(crate) mod edge_value_profiles;
pub(crate) mod edge_weight_profiles;
pub(crate) mod metadata;
pub(crate) mod peer_graph_canisters;
pub(crate) mod property_catalog;
pub(crate) mod remote_forward_in;
pub(crate) mod remote_vertex_refs;
pub(crate) mod vertex_label_catalog;
pub(crate) mod vertex_labels;
pub(crate) mod vertex_logical_ids;
pub(crate) mod vertex_properties;

pub(crate) use memory::GRAPH_DEFAULT_EDGE_LABEL;

thread_local! {
    pub(crate) static GRAPH: RefCell<memory::StableGraph> = RefCell::new(
        memory::init_graph()
    );

    pub(crate) static VERTEX_LABEL_CATALOG: RefCell<memory::StableVertexLabelCatalog> =
        RefCell::new(memory::init_vertex_label_catalog());

    pub(crate) static EDGE_LABEL_CATALOG: RefCell<memory::StableEdgeLabelCatalog> =
        RefCell::new(memory::init_edge_label_catalog());

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

    pub(crate) static EDGE_ALIASES: RefCell<memory::StableEdgeAliasIndex> = RefCell::new(
        memory::init_edge_alias_index()
    );

    pub(crate) static METADATA: RefCell<memory::StableMetadata> = RefCell::new(
        memory::init_metadata()
    );

    pub(crate) static EDGE_WEIGHT_PROFILES: RefCell<memory::StableEdgeWeightProfileStore> =
        RefCell::new(memory::init_edge_weight_profiles());

    pub(crate) static EDGE_VALUE_PROFILES: RefCell<memory::StableEdgeValueProfileStore> =
        RefCell::new(memory::init_edge_value_profiles());

    pub(crate) static VERTEX_LOGICAL_IDS: RefCell<memory::StableVertexLogicalIdMap> =
        RefCell::new(memory::init_vertex_logical_ids());

    pub(crate) static REMOTE_VERTEX_REFS: RefCell<memory::StableRemoteVertexRefTable> =
        RefCell::new(memory::init_remote_vertex_refs());

    pub(crate) static REMOTE_FORWARD_IN: RefCell<memory::StableRemoteForwardInIndex> =
        RefCell::new(memory::init_remote_forward_in());

    pub(crate) static EDGE_EQUALITY_POSTINGS: RefCell<memory::StableEdgeEqualityPostingStore> =
        RefCell::new(memory::init_edge_equality_postings());

    pub(crate) static PEER_GRAPH_CANISTERS: RefCell<memory::StablePeerGraphCanisterSet> =
        RefCell::new(memory::init_peer_graph_canisters());
}
