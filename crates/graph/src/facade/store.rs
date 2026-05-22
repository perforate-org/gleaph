//! Stateless facade over graph storage thread-locals.
//!
//! `GraphStore` is the public coordination point for operations that need to
//! touch multiple stable structures in a consistent order. It intentionally
//! carries no fields; all state lives in the canister-local stable structures
//! initialized in [`super::stable`].

mod catalogs;
mod delete;
mod edge_alias;
mod edge_insert;
mod edge_logical;
mod edge_properties;
mod edge_scan;
mod error;
mod handle;
mod helpers;
mod lookup;
mod maintenance;
mod metadata;
mod sidecar;
#[cfg(test)]
mod tests;
mod vertex;
mod vertex_labels;
mod vertex_properties;
mod vertex_row;

pub use error::GraphStoreError;
pub use handle::EdgeHandle;
pub use helpers::{canonical_undirected_owner, catalog_edge_label_from_wire};

/// Stateless facade over graph storage thread-locals.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;
