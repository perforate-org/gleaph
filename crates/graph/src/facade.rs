//! Public coordination layer over stable storage and canister-local auth.

mod ic_budget;
mod ic_gql_extensions;
mod stable;

pub mod auth;

mod store;

pub mod mutation_executor;

pub use stable::edge_ids::{VertexEdgeIdAllocatorError, canonical_undirected_owner};
pub use stable::label_catalog::LabelCatalogError;
pub use stable::property_catalog::PropertyCatalogError;
pub use stable::vertex_labels::VertexLabelStoreError;
pub use stable::vertex_properties::VertexPropertyStoreError;

pub use ic_budget::timer_lara_maintenance_budget;
pub use ic_gql_extensions::{ic_extension_type_names, init_ic_gql_extensions};
pub use store::{EdgeHandle, GraphStore, GraphStoreError};

pub use stable::metadata::{GraphMetadata, GraphMetadataError, IndexRouting};
