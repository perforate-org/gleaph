//! The shard-local vertex id.
//!
//! Named here so that code outside the Graph shard's edge store never imports the store to speak
//! of a vertex (ADR 0136, Decision 11). It is the store's own id until the store is replaced; the
//! name stays when the definition moves here.

pub use ic_stable_lara::VertexId;
