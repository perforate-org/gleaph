//! Shared property-index API transport types.

use crate::federation::ShardId;
use std::fmt;

/// Maximum encoded sortable index value key size (`V` in index capacity planning).
pub const MAX_INDEX_VALUE_KEY_BYTES: usize = 4096;

/// Maximum number of equality arms the bounded server-side intersection (`lookup_intersection_page`)
/// will accept. Shared between Router candidate resolution and Property Index execution so both
/// enforce the same provider-neutral execution bound for pure equality conjunctions.
///
/// `MAX_EQUALITY_INTERSECTION_ARMS` is the execution limit, not a GQL syntax limit. The planner is
/// free to accept longer pure equality conjunctions; the Router lowers 1 arm to a single equality
/// lookup and 2..=MAX_EQUALITY_INTERSECTION_ARMS arms to the bounded intersection. Requests that
/// resolve to more arms are rejected with a provider-specific error before any index canister call.
pub const MAX_EQUALITY_INTERSECTION_ARMS: usize = 8;

/// Maximum number of posting hits materialized by one paginated index read.
///
/// This is an execution/heap bound, not the ICP message-size limit. The receiver still
/// validates the encoded request and response against the shared transport ceiling. Keeping the
/// page bound here makes Graph, Router, and Property Index use the same producer-side budget.
pub const MAX_POSTING_PAGE_HITS: u32 = 10_000;

/// One shard-owned property-index mutation. The index canister applies these in
/// order and may return a continuation before the request is exhausted.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IndexPostingMutation {
    VertexProperty {
        remove: bool,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    },
    EdgeProperty {
        remove: bool,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
    Label {
        remove: bool,
        label_id: u32,
        vertex_id: u32,
    },
}

/// A bounded progress result from one property-index update call.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub struct IndexPostingBatchProgress {
    /// Number of operations accepted from the beginning of the request.
    pub applied: u32,
    /// First operation not accepted by this call, if any.
    pub next_index: Option<u32>,
    /// True when the index canister stopped at its own instruction budget.
    pub instruction_budget_exhausted: bool,
}

/// Returned when encoded index value key bytes exceed [`MAX_INDEX_VALUE_KEY_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexValueKeyTooLarge {
    pub len: usize,
}

impl fmt::Display for IndexValueKeyTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "index value key length {} exceeds maximum {} bytes",
            self.len, MAX_INDEX_VALUE_KEY_BYTES
        )
    }
}

impl std::error::Error for IndexValueKeyTooLarge {}

/// Validates that encoded index value key bytes are within [`MAX_INDEX_VALUE_KEY_BYTES`].
pub fn validate_index_value_key_bytes(value: &[u8]) -> Result<(), IndexValueKeyTooLarge> {
    if value.len() <= MAX_INDEX_VALUE_KEY_BYTES {
        Ok(())
    } else {
        Err(IndexValueKeyTooLarge { len: value.len() })
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct PostingHit {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Vertex or edge property equality arm for [`IndexIntersectionRequest`] (ADR 0009 §3).
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub enum IndexSubject {
    #[default]
    VertexProperty,
    EdgeProperty {
        label_id: Option<u16>,
    },
}

/// One equality arm for [`IndexIntersectionRequest`].
///
/// `value` must be the sortable index key from `gleaph_gql::value_to_index_key_bytes`.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexEqualSpec {
    pub subject: IndexSubject,
    pub property_id: u32,
    pub value: Vec<u8>,
}

impl IndexEqualSpec {
    pub fn vertex(property_id: u32, value: Vec<u8>) -> Self {
        Self {
            subject: IndexSubject::VertexProperty,
            property_id,
            value,
        }
    }

    pub fn edge(property_id: u32, value: Vec<u8>, label_id: Option<u16>) -> Self {
        Self {
            subject: IndexSubject::EdgeProperty { label_id },
            property_id,
            value,
        }
    }
}

/// Result of a multi-property index intersection (ADR 0009 §3).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IndexIntersectionResult {
    Vertices(Vec<PostingHit>),
    Edges(Vec<EdgePostingHit>),
}

impl IndexIntersectionResult {
    pub fn vertices(self) -> Vec<PostingHit> {
        match self {
            Self::Vertices(hits) => hits,
            Self::Edges(_) => Vec::new(),
        }
    }
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

/// Paginated label intersection export scoped to one shard, with all label sieves applied inside
/// the index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelIntersectionPageRequest {
    pub walk_label_id: u32,
    pub sieve_label_ids: Vec<u32>,
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
    /// Finite half-open encoded interval `[low, high)` supplied by the caller.
    /// Property Index validates the bounds structurally and scans only that interval.
    Between {
        low: Vec<u8>,
        high: Vec<u8>,
    },
}

/// Global posting cardinality for one encoded property value (all shards).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCount {
    pub encoded_value: Vec<u8>,
    pub count: u64,
}

/// Resume cursor for grouped count scans. The next page starts strictly after this value.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCountCursor {
    pub encoded_value: Vec<u8>,
}

/// Maximum groups returned by one grouped-count page. At the maximum encoded value width this
/// keeps the value payload below the conservative 2 MiB inter-canister budget with headroom.
pub const MAX_VALUE_POSTING_COUNT_PAGE_GROUPS: u32 = 256;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupValuePostingCountPageRequest {
    pub property_id: u32,
    pub min_count: u64,
    pub vertex_filter_packed: Option<Vec<u64>>,
    pub after: Option<ValuePostingCountCursor>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCountPage {
    pub counts: Vec<ValuePostingCount>,
    pub next: Option<ValuePostingCountCursor>,
    pub done: bool,
}

/// Resume cursor for paginated vertex property exports (last posting from the prior page).
///
/// Carries `value` because range scans span multiple encoded values; equality scans repeat the
/// fixed request value.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PropertyPostingCursor {
    pub value: Vec<u8>,
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Resume cursor for paginated edge property exports (last posting from the prior page).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct EdgePostingCursor {
    pub value: Vec<u8>,
    pub label_id: u16,
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub slot_index: u32,
}

/// One page of vertex property postings (equality or range export).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PostingHitPage {
    pub hits: Vec<PostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<PropertyPostingCursor>,
    pub done: bool,
}

/// Cursor used by the paginated intersection export. The cursor variant identifies the
/// canonical walk arm selected by the Property Index for the requested intersection shape.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IntersectionPostingCursor {
    Vertex(PropertyPostingCursor),
    Edge(EdgePostingCursor),
}

/// Paginated property intersection export for vertex, edge, and mixed equality shapes.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupPropertyIntersectionPageRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<IntersectionPostingCursor>,
    pub limit: u32,
}

/// One bounded page of a property intersection result.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PropertyIntersectionPage {
    pub hits: IndexIntersectionResult,
    pub next: Option<IntersectionPostingCursor>,
    pub done: bool,
}

/// One page of edge property postings (equality export).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct EdgePostingHitPage {
    pub hits: Vec<EdgePostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<EdgePostingCursor>,
    pub done: bool,
}

/// Paginated equality export for one `(property_id, value)` vertex bucket.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualPageRequest {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated equality export with label membership applied inside the index canister.
///
/// Keeping the label sieve at the producer avoids sending a page to the Router and then
/// performing a second inter-canister query for the same page.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualPageForLabelRequest {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated all-vertex equality intersection (the planner's `IndexIntersection` shape).
///
/// The index walks the first arm (`specs[0]`) in pages and sieves each page against the remaining
/// arms server-side, returning at most `limit` surviving hits plus a [`PropertyPostingCursor`] over
/// the walk arm. This keeps the per-message heap bounded (no per-arm set is materialized) and folds
/// the walk + sieve into a single inter-canister call per page (vs one call per arm per page).
///
/// Execution contract:
/// - `specs` must contain between 2 and [`MAX_EQUALITY_INTERSECTION_ARMS`] arms inclusive; fewer
///   arms produce an empty terminal page.
/// - All specs must target [`IndexSubject::VertexProperty`]; edge or mixed specs produce an empty
///   terminal page (the materializing [`IndexIntersectionRequest`] handles those shapes).
/// - The walk arm is selected deterministically by canonical `(property_id, value)` order, so
///   repeated requests and different callers converge to the same walk plan and resume cursor.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupIntersectionPageRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated all-vertex equality intersection with vertex-label membership applied inside the
/// index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupIntersectionPageForLabelRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range export over encoded values for one vertex property.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangePageRequest {
    pub property_id: u32,
    pub range: PostingRangeRequest,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range export with label membership applied inside the index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangePageForLabelRequest {
    pub property_id: u32,
    pub range: PostingRangeRequest,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range-plus-equality intersection over one vertex property range walk.
///
/// The index walks the finite encoded half-open interval `[low, high)` for `range_property_id`
/// one page at a time and sieves each page against every equality spec in `equal_specs`
/// server-side. The returned `next` and `done` always describe the range walk, even when a page
/// has zero survivors. This keeps the per-message heap bounded: no full range or equality bucket
/// is materialized.
///
/// Execution contract:
/// - `equal_specs` must contain between 1 and [`MAX_EQUALITY_INTERSECTION_ARMS`] vertex specs
///   inclusive. Zero specs is an invalid request shape; callers with no equality sieve should use
///   [`LookupRangePageRequest`] instead. More than [`MAX_EQUALITY_INTERSECTION_ARMS`] specs are
///   rejected by the Property Index.
/// - All specs must target [`IndexSubject::VertexProperty`]. Edge or mixed specs are rejected.
/// - The Property Index canonicalises sieve order by `(property_id, encoded_value)` for
///   deterministic paging; repeated requests and different callers converge to the same walk plan
///   and resume cursor.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangeIntersectionPageRequest {
    pub range_property_id: u32,
    pub low: Vec<u8>,
    pub high: Vec<u8>,
    pub equal_specs: Vec<IndexEqualSpec>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range-plus-equality intersection with vertex-label membership applied inside the
/// index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangeIntersectionPageForLabelRequest {
    pub range_property_id: u32,
    pub low: Vec<u8>,
    pub high: Vec<u8>,
    pub equal_specs: Vec<IndexEqualSpec>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated equality export for one edge property `(property_id, value[, label_id])` bucket.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualPageRequest {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub label_id: Option<u16>,
    pub after: Option<EdgePostingCursor>,
    pub limit: u32,
}
/// Batch paginated equality export for many vertex `(property_id, value)` buckets in one call.
/// Each bucket is answered independently; per-bucket results are paged under a shared limit.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualBatchRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Per-bucket page result for [`LookupEqualBatchRequest`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualBatchResult {
    /// Pages in the same order as `specs`. Each page carries its own cursor/paging state.
    pub pages: Vec<PostingHitPage>,
    /// Smallest spec index not fully returned because the canister neared its instruction budget.
    pub next: Option<u32>,
}

/// Batch paginated equality export for many edge `(property_id, value[, label_id])` buckets.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualBatchRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<EdgePostingCursor>,
    pub limit: u32,
}

/// Per-bucket page result for [`LookupEdgeEqualBatchRequest`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualBatchResult {
    pub pages: Vec<EdgePostingHitPage>,
    pub next: Option<u32>,
}

/// Host entity for an administrator-registered property index (ADR 0009).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexedPropertyKind {
    Vertex,
    Edge,
}

/// Router → graph shard: register one property for index maintenance.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct RegisterIndexedPropertyArgs {
    pub kind: IndexedPropertyKind,
    pub property_id: u32,
}

/// Router → graph shard: register one edge index `(label, property, direction)` (ADR 0012).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct RegisterIndexedEdgeIndexArgs {
    pub label_id: u16,
    pub property_id: u32,
    pub direction_tag: u8,
}

/// One edge index membership `(label, property, direction)` in [`IndexedPropertyCatalog`].
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexedEdgeMembership {
    pub label_id: u16,
    pub property_id: u32,
    pub direction_tag: u8,
}

/// Router-sourced snapshot of which properties are indexed (ADR 0023 D1/D3).
///
/// The router (definitions SSOT) supplies this per operation so the graph shard
/// never persists derived index state. It is consulted ephemerally (set at the
/// start of an operation, cleared at the end) and is therefore immune to the
/// upgrade boundary that made the former shard-local registry stale.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexedPropertyCatalog {
    pub vertex_property_ids: Vec<u32>,
    pub edge_property_ids: Vec<u32>,
    pub edge_indexes: Vec<IndexedEdgeMembership>,
}

impl IndexedPropertyCatalog {
    pub fn is_empty(&self) -> bool {
        self.vertex_property_ids.is_empty()
            && self.edge_property_ids.is_empty()
            && self.edge_indexes.is_empty()
    }
}

/// One edge equality posting hit from graph-index (ADR 0009 §1).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct EdgePostingHit {
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn mixed_intersection_request_candid_roundtrip() {
        let req = IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, vec![1, 2]),
                IndexEqualSpec::edge(2, vec![3], Some(7)),
            ],
        };
        let bytes = Encode!(&req).expect("encode");
        let decoded: IndexIntersectionRequest = Decode!(&bytes, IndexIntersectionRequest).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn validate_index_value_key_bytes_at_and_over_limit() {
        let at_limit = vec![0u8; MAX_INDEX_VALUE_KEY_BYTES];
        validate_index_value_key_bytes(&at_limit).expect("at limit");
        let over = vec![0u8; MAX_INDEX_VALUE_KEY_BYTES + 1];
        let err = validate_index_value_key_bytes(&over).expect_err("over limit");
        assert_eq!(err.len, MAX_INDEX_VALUE_KEY_BYTES + 1);
    }

    #[test]
    fn paginated_property_requests_candid_roundtrip() {
        let cursor = PropertyPostingCursor {
            value: vec![1, 2, 3],
            shard_id: ShardId::new(2),
            vertex_id: 7,
        };
        let equal = LookupEqualPageRequest {
            property_id: 4,
            value: vec![9, 9],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&equal).expect("encode equal");
        assert_eq!(
            Decode!(&bytes, LookupEqualPageRequest).expect("decode equal"),
            equal
        );

        let range = LookupRangePageRequest {
            property_id: 4,
            range: PostingRangeRequest::Ge(vec![5]),
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range).expect("encode range");
        assert_eq!(
            Decode!(&bytes, LookupRangePageRequest).expect("decode range"),
            range
        );

        let bounded_range = LookupRangePageRequest {
            property_id: 4,
            range: PostingRangeRequest::Between {
                low: vec![2, 0, 128, 0, 0, 0],
                high: vec![2, 2, 128, 0, 0, 0],
            },
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&bounded_range).expect("encode bounded range");
        assert_eq!(
            Decode!(&bytes, LookupRangePageRequest).expect("decode bounded range"),
            bounded_range
        );

        let range_intersection = LookupRangeIntersectionPageRequest {
            range_property_id: 5,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: vec![
                IndexEqualSpec::vertex(3, vec![7, 8]),
                IndexEqualSpec::vertex(9, vec![1, 2]),
            ],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection).expect("encode range intersection");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest).expect("decode range intersection"),
            range_intersection
        );

        let range_intersection_one = LookupRangeIntersectionPageRequest {
            range_property_id: 6,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: vec![IndexEqualSpec::vertex(3, vec![7, 8])],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection_one).expect("encode range intersection one");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest)
                .expect("decode range intersection one"),
            range_intersection_one
        );

        let range_intersection_max = LookupRangeIntersectionPageRequest {
            range_property_id: 6,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: (1..=MAX_EQUALITY_INTERSECTION_ARMS)
                .map(|i| IndexEqualSpec::vertex(i as u32, vec![i as u8]))
                .collect(),
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection_max).expect("encode range intersection max");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest)
                .expect("decode range intersection max"),
            range_intersection_max
        );

        let page = PostingHitPage {
            hits: vec![PostingHit {
                shard_id: ShardId::new(2),
                vertex_id: 7,
            }],
            next: Some(cursor.clone()),
            done: false,
        };
        let bytes = Encode!(&page).expect("encode page");
        assert_eq!(Decode!(&bytes, PostingHitPage).expect("decode page"), page);

        let intersection = LookupIntersectionPageRequest {
            specs: vec![
                IndexEqualSpec::vertex(1, vec![1]),
                IndexEqualSpec::vertex(2, vec![2]),
            ],
            after: Some(cursor),
            limit: 256,
        };
        let bytes = Encode!(&intersection).expect("encode intersection page");
        assert_eq!(
            Decode!(&bytes, LookupIntersectionPageRequest).expect("decode intersection page"),
            intersection
        );
    }

    #[test]
    fn paginated_edge_request_candid_roundtrip() {
        let cursor = EdgePostingCursor {
            value: vec![1],
            label_id: 5,
            shard_id: ShardId::new(0),
            owner_vertex_id: 4,
            slot_index: 1,
        };
        let req = LookupEdgeEqualPageRequest {
            property_id: 2,
            value: vec![3, 4],
            label_id: Some(5),
            after: Some(cursor.clone()),
            limit: 128,
        };
        let bytes = Encode!(&req).expect("encode edge req");
        assert_eq!(
            Decode!(&bytes, LookupEdgeEqualPageRequest).expect("decode edge req"),
            req
        );

        let page = EdgePostingHitPage {
            hits: vec![EdgePostingHit {
                shard_id: ShardId::new(0),
                owner_vertex_id: 4,
                label_id: 5,
                slot_index: 1,
            }],
            next: Some(cursor),
            done: true,
        };
        let bytes = Encode!(&page).expect("encode edge page");
        assert_eq!(
            Decode!(&bytes, EdgePostingHitPage).expect("decode edge page"),
            page
        );
    }

    #[test]
    fn intersection_result_variants_candid_roundtrip() {
        let vertices = IndexIntersectionResult::Vertices(vec![PostingHit {
            shard_id: ShardId::new(1),
            vertex_id: 9,
        }]);
        let bytes = Encode!(&vertices).expect("encode vertices");
        let decoded: IndexIntersectionResult =
            Decode!(&bytes, IndexIntersectionResult).expect("decode vertices");
        assert_eq!(decoded, vertices);

        let edges = IndexIntersectionResult::Edges(vec![EdgePostingHit {
            shard_id: ShardId::new(0),
            owner_vertex_id: 4,
            label_id: 2,
            slot_index: 1,
        }]);
        let bytes = Encode!(&edges).expect("encode edges");
        let decoded: IndexIntersectionResult =
            Decode!(&bytes, IndexIntersectionResult).expect("decode edges");
        assert_eq!(decoded, edges);
    }

    #[test]
    fn posting_batch_wire_roundtrip_preserves_order_and_progress() {
        let operations = vec![
            IndexPostingMutation::VertexProperty {
                remove: false,
                property_id: 1,
                value: vec![1, 2],
                vertex_id: 3,
            },
            IndexPostingMutation::EdgeProperty {
                remove: true,
                property_id: 2,
                value: vec![4],
                label_id: 5,
                owner_vertex_id: 6,
                slot_index: 7,
            },
            IndexPostingMutation::Label {
                remove: false,
                label_id: 8,
                vertex_id: 9,
            },
        ];
        let bytes = Encode!(&operations).expect("encode posting batch");
        assert_eq!(
            Decode!(&bytes, Vec<IndexPostingMutation>).expect("decode posting batch"),
            operations
        );

        let progress = IndexPostingBatchProgress {
            applied: 2,
            next_index: Some(2),
            instruction_budget_exhausted: true,
        };
        let bytes = Encode!(&progress).expect("encode progress");
        assert_eq!(
            Decode!(&bytes, IndexPostingBatchProgress).expect("decode progress"),
            progress
        );
    }
}
