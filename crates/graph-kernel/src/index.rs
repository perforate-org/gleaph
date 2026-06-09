//! Shared property-index API transport types.

use crate::federation::ShardId;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct PostingHit {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// One equality arm for [`IndexIntersectionRequest`].
///
/// `value` must be the sortable index key from `gleaph_gql::value_to_index_key_bytes`.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexEqualSpec {
    pub property_id: u32,
    pub value: Vec<u8>,
}

/// Intersect equality postings for multiple properties (at least two specs; Eq only).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexIntersectionRequest {
    pub specs: Vec<IndexEqualSpec>,
}

/// Compare encoded property values using the same lexicographic order as index posting keys.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum PostingRangeRequest {
    Ge(Vec<u8>),
    Gt(Vec<u8>),
    Le(Vec<u8>),
    Lt(Vec<u8>),
}

/// Global posting cardinality for one encoded property value (all shards).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCount {
    pub encoded_value: Vec<u8>,
    pub count: u64,
}
