//! Bounded, resumable shard-posting detach (router → graph-index).
//!
//! Posting keys order `shard_id` after `(property_id, value, …)`, so purging a
//! shard's postings requires a full scan of each posting set. A single update
//! message cannot scan an arbitrarily large index within the per-message
//! instruction and stable read/write limits, so detach is driven as a sequence
//! of bounded steps: the router resumes from the returned [`ShardDetachCursor`]
//! until [`ShardDetachStepResult::done`].

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Posting set purged during shard detach, processed in this fixed order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum ShardDetachPhase {
    Vertex,
    Label,
    Edge,
}

/// Resume point for a bounded shard detach.
///
/// A `None` resume starts or restarts the shard's active detach session. A
/// decoded legacy cursor has no generation and must be rejected by the owner
/// before any derived-state read or write.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardDetachCursor {
    /// Owner-issued generation identifying one durable shard-detach session.
    pub detach_generation: Option<u64>,
    pub phase: ShardDetachPhase,
    /// Encoded last-examined key in `phase`'s set; empty starts the phase.
    pub resume_key: Vec<u8>,
}

/// Progress from one bounded shard detach step.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardDetachStepResult {
    /// Resume point for the next step, or `None` once the purge is complete.
    pub next: Option<ShardDetachCursor>,
    /// Posting keys examined in this step (bounded by the index budget).
    pub examined: u32,
    /// Posting keys removed in this step.
    pub removed: u32,
    /// `true` when the purge is complete (`next` is `None`).
    pub done: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[derive(CandidType, Serialize)]
    struct LegacyShardDetachCursor {
        phase: ShardDetachPhase,
        resume_key: Vec<u8>,
    }

    #[test]
    fn legacy_shard_detach_cursor_decodes_without_generation() {
        let bytes = Encode!(&LegacyShardDetachCursor {
            phase: ShardDetachPhase::Label,
            resume_key: vec![1, 2, 3],
        })
        .expect("encode legacy shard detach cursor");

        let decoded = Decode!(&bytes, ShardDetachCursor).expect("decode legacy cursor");
        assert_eq!(
            decoded,
            ShardDetachCursor {
                detach_generation: None,
                phase: ShardDetachPhase::Label,
                resume_key: vec![1, 2, 3],
            }
        );
    }

    #[test]
    fn current_shard_detach_cursor_round_trips_generation() {
        let cursor = ShardDetachCursor {
            detach_generation: Some(7),
            phase: ShardDetachPhase::Edge,
            resume_key: vec![4, 5, 6],
        };

        let bytes = Encode!(&cursor).expect("encode current shard detach cursor");
        assert_eq!(
            Decode!(&bytes, ShardDetachCursor).expect("decode current cursor"),
            cursor
        );
    }
}
