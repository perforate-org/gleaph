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

/// Progress from one posting backfill batch on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PostingBackfillResult {
    pub next_vertex_id: LocalVertexId,
    pub vertices_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}
