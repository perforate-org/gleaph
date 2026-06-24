//! Cursor-based vertex-embedding backfill on a graph shard (router → graph).
//!
//! Mirrors [`posting_backfill`](super::posting_backfill) for the derived vector index (ADR 0031
//! Slice 2). The router supplies an ephemeral [`IndexedEmbeddingCatalog`] per batch; the graph
//! shard never persists an indexed-embedding registry.

use super::LocalVertexId;
use crate::vector_index::IndexedEmbeddingCatalog;
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// One batch of vertex-local embedding replay from canonical shard state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EmbeddingBackfillArgs {
    pub start_vertex_id: LocalVertexId,
    pub max_vertices: u32,
}

/// Router → graph shard vertex-embedding backfill request carrying the router-sourced indexed
/// embedding catalog for the operation.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VertexEmbeddingBackfillRequest {
    pub args: EmbeddingBackfillArgs,
    pub catalog: IndexedEmbeddingCatalog,
}

/// Progress from one embedding backfill batch on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EmbeddingBackfillResult {
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub embeddings_synced: u32,
    pub done: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn backfill_request_candid_roundtrip() {
        let req = VertexEmbeddingBackfillRequest {
            args: EmbeddingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 128,
            },
            catalog: IndexedEmbeddingCatalog::default(),
        };
        let bytes = Encode!(&req).expect("encode");
        assert_eq!(
            Decode!(&bytes, VertexEmbeddingBackfillRequest).expect("decode"),
            req
        );
    }

    #[test]
    fn backfill_result_candid_roundtrip() {
        let result = EmbeddingBackfillResult {
            next_vertex_id: 9,
            vertices_processed: 9,
            embeddings_synced: 4,
            done: true,
        };
        let bytes = Encode!(&result).expect("encode");
        assert_eq!(
            Decode!(&bytes, EmbeddingBackfillResult).expect("decode"),
            result
        );
    }
}
