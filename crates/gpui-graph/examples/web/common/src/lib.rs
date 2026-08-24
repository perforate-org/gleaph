//! Application-owned data shared by both wasm modules of the web example.
//!
//! Two things live here, and their placement IS the lesson:
//!
//! - [`fixture`](self::fixture) — the deterministic ~100-vertex demo graph and
//!   its seed placement. Both compilation units inject the same scene.
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

pub use batch_codec::DemoBatchCodec;
pub use fixture::{DEMO_NODE_COUNT, demo_batch, initial_position};
