//! Shared property-index API transport types.

use crate::federation::ShardId;
use std::fmt;

/// Maximum encoded sortable index value key size (`V` in index capacity planning).
pub const MAX_INDEX_VALUE_KEY_BYTES: usize = 4096;

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
    Clone, Debug, Default, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
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
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
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

/// Paginated all-vertex equality intersection (the planner's `IndexIntersection` shape).
///
/// The index walks the first arm (`specs[0]`) in pages and sieves each page against the remaining
/// arms server-side, returning at most `limit` surviving hits plus a [`PropertyPostingCursor`] over
/// the walk arm. This keeps the per-message heap bounded (no per-arm set is materialized) and folds
/// the walk + sieve into a single inter-canister call per page (vs one call per arm per page).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupIntersectionPageRequest {
    pub specs: Vec<IndexEqualSpec>,
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

/// Paginated equality export for one edge property `(property_id, value[, label_id])` bucket.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualPageRequest {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub label_id: Option<u16>,
    pub after: Option<EdgePostingCursor>,
    pub limit: u32,
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
}
