//! Stateless facade over graph storage thread-locals.
//!
//! `GraphStore` is the public coordination point for operations that need to
//! touch multiple stable structures in a consistent order. It intentionally
//! carries no fields; all state lives in the canister-local stable structures
//! initialized in [`super::stable`].
//!
//! Storage domains (Phase 2 module map):
//! - **adjacency** — `adjacency` (edge insert/delete commit), `edge_insert`, `edge_scan`, `edge_alias`, `edge_logical`, `delete`
//! - **properties** — `properties` (write commit), `vertex_properties`, `edge_properties`, `catalogs`
//! - **labels** — `labels` (write commit), `vertex_labels`
//! - **vertex delete** — `vertex_delete` (sidecar clear and detach delete commit)
//! - **edge profiles** — `edge_profiles` (profile install/update commits), `edge_payload` (public update API)
//! - **local indexes** — `local_indexes` (alias and equality posting commits), `edge_alias` (lookup)
//! - **label stats projection** — `label_stats_delta`
//! - **sidecars** — `sidecar` (coordinates property and local-index derived state)
//! - **maintenance** — `maintenance`

mod adjacency;
mod catalogs;
mod delete;
mod edge_alias;
mod edge_insert;
mod edge_payload;
mod edge_profiles;
mod edge_properties;
mod edge_scan;
mod error;
mod handle;
pub(crate) mod helpers;
mod label_stats_delta;
mod labels;
mod local_indexes;
mod lookup;
mod maintenance;
mod metadata;
mod pending_purge;
mod properties;
mod sidecar;
#[cfg(test)]
mod tests;
mod vertex;
mod vertex_delete;
mod vertex_labels;
mod vertex_properties;
mod vertex_row;

pub use error::GraphStoreError;
pub use handle::EdgeHandle;
pub use helpers::{canonical_undirected_owner, catalog_edge_label_from_wire};
pub use maintenance::{BulkIngestFinalizeReport, BulkIngestFinalizeSpec};
pub(crate) use pending_purge::vertex_hidden_by_pending_purge;

/// Stateless facade over graph storage thread-locals.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;
