//! Cursor-based edge property posting backfill on a graph shard (router → graph → index).

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Resume cursor: lexicographic [`EdgePropertyKey`] wire bytes (14 bytes on graph shard).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgePostingBackfillArgs {
    pub after_key: Option<Vec<u8>>,
    pub max_entries: u32,
}

/// Router → graph shard edge-property backfill request carrying the
/// router-sourced indexed catalog for the operation (ADR 0023 D1/D5).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgePropertyBackfillRequest {
    pub args: EdgePostingBackfillArgs,
    pub catalog: crate::index::IndexedPropertyCatalog,
}

/// Progress from one edge posting backfill batch.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgePostingBackfillResult {
    pub next_after_key: Option<Vec<u8>>,
    pub entries_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}
