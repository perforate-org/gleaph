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
//! - **edge profiles** — `edge_payload`, `sidecar` (weight/payload profiles)
//! - **remote refs** — `remote_refs` (logical vertex handles, forward-in index, logical edge insert)
//! - **local indexes** — `edge_alias`, equality postings in `sidecar`
//! - **telemetry** — `telemetry`
//! - **maintenance** — `maintenance`

mod adjacency;
mod catalogs;
mod delete;
mod edge_alias;
mod edge_insert;
mod edge_logical;
mod edge_payload;
mod edge_properties;
mod edge_scan;
mod error;
mod handle;
pub(crate) mod helpers;
mod labels;
mod lookup;
mod maintenance;
mod metadata;
mod properties;
mod remote_refs;
mod sidecar;
mod telemetry;
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

/// Stateless facade over graph storage thread-locals.
#[derive(Clone, Copy, Debug, Default)]
pub struct GraphStore;
