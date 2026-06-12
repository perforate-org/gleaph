//! Cursor-based edge property posting backfill on a graph shard (router → graph → index).

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Resume cursor: lexicographic [`EdgePropertyKey`] wire bytes (14 bytes on graph shard).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgePostingBackfillArgs {
    pub after_key: Option<Vec<u8>>,
    pub max_entries: u32,
}

/// Progress from one edge posting backfill batch.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgePostingBackfillResult {
    pub next_after_key: Option<Vec<u8>>,
    pub entries_processed: u32,
    pub postings_synced: u32,
    pub done: bool,
}
