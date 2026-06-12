//! Stable-memory-backed graph fragments (catalogs, property stores, id allocator, init).
//!
//! Module visibility is `pub(in crate::facade)` (see `facade.rs`): only code under `facade`
//! (notably [`super::store`]) may reference this module directly. Stable-backed
//! error types are re-exported at the `facade` root for public `GraphStore` signatures.

use std::cell::RefCell;

pub(crate) mod memory;
pub(crate) mod layout;

pub(crate) mod edge_alias;
pub(crate) mod edge_equality_postings;
pub(crate) mod edge_payload_profiles;
pub(crate) mod edge_properties;
pub(crate) mod edge_weight_profiles;
pub(crate) mod label_telemetry;
pub(crate) mod metadata;
pub(crate) mod property_catalog;
pub(crate) mod vertex_labels;
pub(crate) mod vertex_properties;

pub(crate) use memory::GRAPH_DEFAULT_EDGE_LABEL;

thread_local! {
    pub(crate) static GRAPH: RefCell<memory::StableGraph> = RefCell::new(
        memory::init_graph()
    );

    pub(crate) static VERTEX_LABELS: RefCell<memory::StableVertexLabelStore> = RefCell::new(
        memory::init_vertex_label_store()
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

    pub(crate) static EDGE_PAYLOAD_PROFILES: RefCell<memory::StableEdgePayloadProfileStore> =
        RefCell::new(memory::init_edge_payload_profiles());

    pub(crate) static EDGE_EQUALITY_POSTINGS: RefCell<memory::StableEdgeEqualityPostingStore> =
        RefCell::new(memory::init_edge_equality_postings());

    pub(crate) static LABEL_TELEMETRY_SEQ: RefCell<memory::StableLabelTelemetrySeq> =
        RefCell::new(memory::init_label_telemetry_seq());

    pub(crate) static LABEL_TELEMETRY_OUTBOX: RefCell<memory::StableLabelTelemetryOutbox> =
        RefCell::new(memory::init_label_telemetry_outbox());

    pub(crate) static APPLIED_MUTATION_REQUESTS: RefCell<memory::StableAppliedMutationRequests> =
        RefCell::new(memory::init_applied_mutation_requests());
}
