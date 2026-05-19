//! Public coordination layer over stable storage and canister-local auth.

pub(crate) mod edge_equality_index;
mod ic_budget;
mod ic_gql_extensions;
mod stable;

pub mod auth;

mod store;

pub mod federation_expand;
pub mod migration;
pub mod mutation_executor;

pub use stable::edge_label_catalog::EdgeLabelCatalogError;
pub use stable::property_catalog::PropertyCatalogError;
pub use stable::vertex_label_catalog::VertexLabelCatalogError;
pub use stable::vertex_labels::VertexLabelStoreError;
pub use stable::vertex_properties::VertexPropertyStoreError;

pub use ic_budget::timer_lara_maintenance_budget;
pub use ic_gql_extensions::{ic_extension_type_names, init_ic_gql_extensions};
pub use store::{EdgeHandle, GraphStore, GraphStoreError, canonical_undirected_owner};

pub use stable::metadata::{FederationRouting, GraphMetadata, GraphMetadataError};
