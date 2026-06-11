//! Public coordination layer over stable storage.

pub(crate) mod derived_state;
pub(crate) mod edge_equality_index;
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
