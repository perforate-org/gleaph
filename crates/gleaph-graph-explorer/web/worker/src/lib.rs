//! ADR 0076 S4a application-owned worker bootstrap for the graph explorer web
//! entry.
//!
//! `gpui-graph` deliberately ships no worker bundle: an application spawns its
//! own Worker script, which imports a wasm module and drives a concrete
//! [`gpui_graph::WorkerBackend`] inside `DedicatedWorkerGlobalScope`
//! (`gpui_graph::worker::web_transport`). This crate is that counterpart for
//! the explorer web entry, compiled to wasm32 by `web/build.sh` and imported by
//! the hand-written `assets/worker.js`.
//!
//! Layout:
//!
//! - [`batch_codec`] — the application-side payload codec for
//!   `SceneMutation::Merge`. Library-encoded requests
//!   ([`gpui_graph::ToWorker::encode_wire_bytes`]) cover moves and interaction
//!   snapshots only; batch payloads are explicitly left to applications via
//!   the [`gpui_graph::worker::PayloadCodec`] seam. The codec rides under the
//!   library's application-mutation envelope so library bytes cross verbatim.
//! - [`paint_timing`] — the pure timing-harness core: one indexed paint-frame
//!   build over the same deterministic `random_5000`-family fixture as
//!   `crates/gpui-graph/benches/paint_bench.rs`, timed as-is on whatever target
//!   runs it. The wasm export wraps this core, so browser numbers and native
//!   baseline numbers come out of identical code.
//!
//! Everything platform-independent is compiled and tested on native targets;
//! only the `imp` module is wasm-gated.

pub mod batch_codec;
pub mod paint_timing;

#[cfg(target_arch = "wasm32")]
mod imp;

pub use batch_codec::ExplorerBatchCodec;
