//! Shared property-index API transport types.

use crate::federation::ShardId;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct PostingHit {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Compare encoded property values using the same lexicographic order as index posting keys.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum PostingRangeRequest {
    Ge(Vec<u8>),
    Gt(Vec<u8>),
    Le(Vec<u8>),
    Lt(Vec<u8>),
}
