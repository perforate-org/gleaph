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
//! - [`parameters`] — prepared-operation parameter definitions from the Router
//!   manifest and the validation/binding rules over bound values.
//! - [`results`] — the query result as a UI-neutral table (columns, display
//!   strings) that a virtualized console table reads row by row.
//! - [`session`] — [`session::GraphExplorerSession`], the central reusable
//!   abstraction owning graph-loading, mapping, active query, execution status,
//!   presentation, results and selection.

pub mod graph;
pub mod mapping;
pub mod parameters;
pub mod presentation;
pub mod query;
pub mod results;
pub mod session;
