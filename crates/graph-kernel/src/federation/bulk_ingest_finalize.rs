//! Post-ingest maintenance finalize on a graph shard (router → graph).

use super::{LocalVertexId, ShardId};
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Minimum forward edge inserts on one source vertex in a DML batch to treat it as a hot hub.
pub const HOT_FORWARD_EDGE_INSERT_THRESHOLD: u32 = 2;

/// Maximum router-side finalize/drain retries when the wasm instruction budget stops early.
pub const BULK_INGEST_FINALIZE_MAX_DRAIN_RETRIES: u32 = 8;

const FINALIZE_BULK_INGEST: &[&str] = &["GLEAPH", "FINALIZE_BULK_INGEST"];
const FINALIZE_FORWARD_EDGE_SPAN: &[&str] = &["GLEAPH", "FINALIZE_FORWARD_EDGE_SPAN"];
const DRAIN_DEFERRED_MAINTENANCE: &[&str] = &["GLEAPH", "DRAIN_DEFERRED_MAINTENANCE"];

/// True for Gleaph bulk-ingest finalize procedure names on the wire.
pub fn is_gleaph_finalize_procedure_name(parts: &[impl AsRef<str>]) -> bool {
    procedure_parts_match(parts, FINALIZE_BULK_INGEST)
        || procedure_parts_match(parts, FINALIZE_FORWARD_EDGE_SPAN)
        || procedure_parts_match(parts, DRAIN_DEFERRED_MAINTENANCE)
}

fn procedure_parts_match(parts: &[impl AsRef<str>], expected: &[&str]) -> bool {
    parts.len() == expected.len()
        && parts
            .iter()
            .zip(expected)
            .all(|(part, exp)| part.as_ref() == *exp)
}

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
