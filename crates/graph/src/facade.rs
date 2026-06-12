//! Public coordination layer over stable storage.

pub(crate) mod derived_state;
mod ic_budget;
mod ic_gql_extensions;
mod stable;

mod store;
mod store_edge_insert;

pub mod mutation_executor;

pub use stable::property_catalog::PropertyCatalogError;
pub use stable::vertex_labels::VertexLabelStoreError;
pub use stable::vertex_properties::VertexPropertyStoreError;

pub use ic_budget::timer_lara_maintenance_budget;
pub(crate) use ic_budget::{
    post_edge_insert_maintenance_budget, unlimited_lara_maintenance_budget,
};
pub use ic_gql_extensions::{ic_extension_type_names, init_ic_gql_extensions};
pub use store::{
    EdgeHandle, GraphStore, GraphStoreError, canonical_undirected_owner,
    catalog_edge_label_from_wire,
};

pub use stable::metadata::{FederationRouting, GraphMetadata, GraphMetadataError};

/// Phase 8 edge-profile read benches (ADR 0007).
#[cfg(feature = "canbench")]
pub mod bench_stable_layout {
    use gleaph_graph_kernel::entry::{
        EdgeLabelId, EdgePayloadProfile, EdgeWeightProfile, WeightEncoding,
    };
    use std::hint::black_box;

    use super::GraphStore;
    use crate::test_labels::install_test_edge_payload_profile;

    pub fn edge_profile_label() -> EdgeLabelId {
        EdgeLabelId::from_raw(1)
    }

    pub fn install_edge_profile_fixtures() {
        let label = edge_profile_label();
        let weight = EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        };
        install_test_edge_payload_profile(label, EdgePayloadProfile::from(weight));
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
    std::hint::black_box(memory::init_label_telemetry_seq());
    std::hint::black_box(memory::init_label_telemetry_outbox());
    std::hint::black_box(memory::init_applied_mutation_requests());
}
