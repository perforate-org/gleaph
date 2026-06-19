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
/// A `None` resume starts a fresh detach (and drops the shard↔canister auth
/// mapping so the shard can no longer insert postings while the purge runs).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardDetachCursor {
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
