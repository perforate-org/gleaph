//! Which side of an edge a read is on, and the order a read walks a vertex's edges in.
//!
//! Named here so that code outside the Graph shard's edge store never imports the store to choose
//! them (ADR 0136, Decision 11). They are still the store's own enums: `EdgeOrientation` is the
//! forward (out-adjacency) or reverse (in-adjacency) side, `EdgeOrder` ascending or descending.
//! The names stay when the store is replaced.

pub use ic_stable_lara::labeled::{
    LabeledOrientation as EdgeOrientation, OutEdgeOrder as EdgeOrder,
};
