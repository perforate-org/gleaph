//! `gleaph-graph-explorer` — a GPUI graph explorer that loads a Gleaph graph
//! through the Rust SDK and visualizes prepared-query results as overlays over
//! a persistent scene.
//!
//! The crate is organized around the boundaries described in
//! `design/demo/graph-explorer-roadmap.md`:
//!
//! - [`graph`] — SDK → `gpui-graph` conversion. Loads the whole graph through
//!   multiple bounded SDK queries (one all-vertex `element_id` query, one query
//!   per vertex label, one query per edge label) and produces a
//!   [`gpui_graph::GraphBatch`] plus a [`mapping::GraphIdentityMap`]. No
//!   Gleaph-specific type leaks into `gpui-graph`.
//! - [`mapping`] — the explicit boundary between Gleaph entity identities
//!   (opaque `element_id` bytes) and `gpui-graph` identities.
//! - [`query`] — prepared-query presets and execution through the SDK.
//! - [`presentation`] — converts a successful query result into a visual
//!   presentation model (`context` / `result`), mapped onto
//!   [`gpui_graph::OverlayCategory`].
//! - [`session`] — [`session::GraphExplorerSession`], the central reusable
//!   abstraction owning graph-loading, mapping, active query, execution status,
//!   presentation, and selection.

pub mod graph;
pub mod mapping;
pub mod presentation;
pub mod query;
pub mod session;
