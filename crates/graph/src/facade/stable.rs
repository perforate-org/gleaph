//! Stable-memory-backed graph fragments (catalogs, property stores, id allocator, init).
//!
//! Module visibility is `pub(in crate::facade)` (see `facade.rs`): only code under `facade`
//! (notably [`super::store`]) may reference this module directly. Stable-backed
//! error types are re-exported at the `facade` root for public `GraphStore` signatures.

use std::cell::RefCell;

pub(crate) mod layout;
pub(crate) mod memory;

pub(crate) mod edge_alias;
pub(crate) mod edge_properties;
pub(crate) mod label_stats_delta;
pub(crate) mod local_unique;
pub(crate) mod metadata;
pub(crate) mod property_catalog;
pub(crate) mod repair_journal;
pub(crate) mod unique_effect_outbox;
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

    pub(crate) static LABEL_STATS_DELTA_SEQ: RefCell<memory::StableLabelStatsDeltaSeq> =
        RefCell::new(memory::init_label_stats_delta_seq());

    pub(crate) static LABEL_STATS_DELTA_LOG: RefCell<memory::StableLabelStatsDeltaLog> =
        RefCell::new(memory::init_label_stats_delta_log());

    pub(crate) static GRAPH_MUTATION_JOURNAL: RefCell<memory::StableGraphMutationJournal> =
        RefCell::new(memory::init_graph_mutation_journal());

    pub(crate) static PENDING_VERTEX_PURGES: RefCell<memory::StablePendingPurges> =
        RefCell::new(memory::init_pending_vertex_purges());

    pub(crate) static INDEX_REPAIR_JOURNAL: RefCell<memory::StableRepairJournal> =
        RefCell::new(memory::init_index_repair_journal());

    pub(crate) static UNIQUE_EFFECT_OUTBOX: RefCell<memory::StableUniqueEffectOutbox> =
        RefCell::new(memory::init_unique_effect_outbox());

    pub(crate) static GRAPH_LOCAL_UNIQUE_VALUES: RefCell<memory::StableGraphLocalUniqueTable> =
        RefCell::new(memory::init_graph_local_unique_table());
}

/// Forces the stable graph to initialize now. Called from `post_upgrade` so a
/// layout/version skew (LARA magic/version/stride mismatch) traps at the upgrade
/// boundary with an actionable message, instead of lazily on the first query.
pub(crate) fn ensure_graph_initialized() {
    GRAPH.with_borrow(|_| {});
}
