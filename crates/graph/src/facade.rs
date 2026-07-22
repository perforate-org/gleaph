//! Public coordination layer over stable storage.

pub(crate) mod derived_state;
mod ic_budget;
mod ic_gql_extensions;
pub(crate) mod maintenance_timer;
pub(crate) mod stable;

mod store;
mod store_edge_insert;

pub mod mutation_executor;

pub use stable::property_catalog::PropertyCatalogError;
pub use stable::vertex_embeddings::VertexEmbeddingStoreError;
pub use stable::vertex_labels::VertexLabelStoreError;
pub use stable::vertex_properties::VertexPropertyStoreError;

pub(crate) use ic_budget::IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT;
pub use ic_budget::{bulk_ingest_finalize_maintenance_budget, timer_lara_maintenance_budget};
pub(crate) use ic_budget::{delete_maintenance_budget, post_edge_insert_maintenance_budget};
pub use ic_gql_extensions::{ic_extension_type_names, init_ic_gql_extensions};
pub(crate) use store::vertex_hidden_by_pending_purge;
pub use store::{
    BulkIngestFinalizeReport, BulkIngestFinalizeSpec, EdgeHandle, GraphStore, GraphStoreError,
    canonical_undirected_owner, catalog_edge_label_from_wire,
};

pub(crate) use stable::ensure_graph_initialized;
pub(crate) use stable::memory::stable_memory_stats;
pub use stable::metadata::{FederationRouting, GraphMetadata, GraphMetadataError};
pub(crate) use stable::repair_journal::RepairPostingOp;

/// Phase 8 edge-profile read benches (ADR 0007).
#[cfg(feature = "canbench")]
pub mod bench_stable_layout {
    use gleaph_graph_kernel::entry::{
        EdgeInlineValueProfile, EdgeLabelId, EdgeWeightProfile, WeightEncoding,
    };
    use std::hint::black_box;

    use super::GraphStore;
    use crate::test_labels::install_test_edge_inline_value_profile;

    pub fn edge_profile_label() -> EdgeLabelId {
        EdgeLabelId::from_raw(1)
    }

    pub fn install_edge_profile_fixtures() {
        let label = edge_profile_label();
        let weight = EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        };
        install_test_edge_inline_value_profile(label, EdgeInlineValueProfile::from(weight));
    }

    pub fn read_weight_via_store(label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        black_box(GraphStore::new().edge_label_weight_profile(label))
    }
}

/// Re-initializes all graph stable stores on the persisted memory manager (canbench / ADR 0007).
#[cfg(feature = "canbench")]
pub fn bench_stable_reopen_touch() {
    use stable::memory;
    std::hint::black_box(memory::init_graph());
    std::hint::black_box(memory::init_vertex_label_store());
    std::hint::black_box(memory::init_vertex_property_store());
    std::hint::black_box(memory::init_edge_property_store());
    std::hint::black_box(memory::init_edge_alias_index());
    std::hint::black_box(memory::init_metadata());
    std::hint::black_box(memory::init_label_stats_delta_seq());
    std::hint::black_box(memory::init_label_stats_delta_log());
    std::hint::black_box(memory::init_graph_mutation_journal());
}
