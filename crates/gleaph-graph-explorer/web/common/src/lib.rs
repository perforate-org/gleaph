//! Application-owned data shared by both wasm modules of the explorer web
//! entry.
//!
//! Two things live here, and their placement IS the point:
//!
//! - [`fixture`](self::fixture) — the deterministic random demo graph
//!   (`benches/paint_bench.rs` shape) and its seed placement. The main thread
//!   injects it into the worker replica and holds the same content for its own
//!   camera-fit scene; the timing harness times over it.
//! - [`batch_codec`] — the application's own byte form for
//!   `SceneMutation::Merge` batches, implementing
//!   [`gpui_graph::worker::PayloadCodec`]. The gpui-graph wire covers only
//!   library-owned content ([`ToWorker::encode_wire_bytes`] answers
//!   `PayloadCodecRequired` for merge/apply); what an application's batch
//!   looks like on the wire is the application's decision, made once here and
//!   used identically by the sender (main thread) and the receiver (worker).
//!
//! Everything in this crate is plain Rust: it compiles and is unit-tested on
//! native targets like any other code.

pub mod batch_codec;
pub mod fixture;

pub use batch_codec::ExplorerBatchCodec;
pub use fixture::{SceneFixture, random_fixture};
