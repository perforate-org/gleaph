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
