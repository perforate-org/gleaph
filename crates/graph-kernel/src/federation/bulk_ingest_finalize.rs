//! Post-ingest maintenance finalize on a graph shard (router → graph).

use super::{LocalVertexId, ShardId};
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Router → graph: enqueue and/or drain deferred maintenance after bulk ingest.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct BulkIngestFinalizeArgs {
    pub target_shard_id: ShardId,
    pub forward_vertices: Vec<LocalVertexId>,
    pub reverse_vertices: Vec<LocalVertexId>,
    /// `true`: enqueue span compaction then drain; `false`: drain-only retry.
    pub enqueue: bool,
}

/// Progress from one bulk-ingest finalize call on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct BulkIngestFinalizeResult {
    pub queued_forward: u32,
    pub queued_reverse: u32,
    pub processed_work_items: u32,
    pub remaining_queue_len: u64,
    pub instruction_budget_exhausted: bool,
    pub instructions_used: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn bulk_ingest_finalize_args_candid_roundtrip() {
        let args = BulkIngestFinalizeArgs {
            target_shard_id: ShardId::new(0),
            forward_vertices: vec![1, 2],
            reverse_vertices: vec![3],
            enqueue: true,
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: BulkIngestFinalizeArgs =
            Decode!(&bytes, BulkIngestFinalizeArgs).expect("decode");
        assert_eq!(args, decoded);
    }
}
