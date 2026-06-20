//! Cursor-based posting backfill on a graph shard (router → graph).

use super::LocalVertexId;
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// One batch of vertex-local posting replay from canonical shard state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PostingBackfillArgs {
    pub start_vertex_id: LocalVertexId,
    pub max_vertices: u32,
}

/// Router → graph shard vertex-property backfill request carrying the
/// router-sourced indexed catalog for the operation (ADR 0023 D1/D5).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VertexPropertyBackfillRequest {
    pub args: PostingBackfillArgs,
    pub catalog: crate::index::IndexedPropertyCatalog,
}

/// Progress from one posting backfill batch on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PostingBackfillResult {
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}
