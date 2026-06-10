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

/// Intersect label membership postings for multiple labels (at least two label ids).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexLabelIntersectionRequest {
    pub vertex_label_ids: Vec<u32>,
}

/// Resume cursor for [`LabelLookupPageRequest`] (last posting from the prior page).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct LabelPostingCursor {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Paginated label membership export scoped to one shard.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelLookupPageRequest {
    pub vertex_label_id: u32,
    pub shard_id: ShardId,
    pub after: Option<LabelPostingCursor>,
    pub limit: u32,
}

/// One page of label postings for a shard-local export.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelLookupPageResult {
    pub hits: Vec<PostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<LabelPostingCursor>,
    pub done: bool,
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
