//! Shared vector-index types.
//!
//! Per [ADR 0031](design/adr/0031-vertex-embedding-store-and-derived-vector-index.md), this module
//! is the home for vector-index wire types. In the first slice it carries only the canonical
//! embedding encoding; derived candidate/search/cursor types are added in their owning phases.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Encoding of a stored vertex embedding.
///
/// Only fixed-dimension `F32` is supported in the first slice. New variants (`F16`, `I8`) must
/// update every exhaustive `match` on this enum, which is the intended compile-time gate before
/// an `UnsupportedEncoding`-style runtime branch is introduced.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorEncoding {
    /// IEEE-754 little-endian `f32` components; byte width is `dims * 4`.
    F32,
}
