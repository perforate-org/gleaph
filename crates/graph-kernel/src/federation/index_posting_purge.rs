//! Bounded, resumable index posting purge for `DROP INDEX` (ADR 0023 D6).
//!
//! `DROP INDEX` must remove the dropped property's postings from graph-index, not
//! just clear the router catalog (closing P7: dropped indexes used to orphan
//! their postings). A single update message cannot delete an arbitrarily large
//! posting set within the per-message instruction / stable read-write limits, so
//! the purge runs as a sequence of bounded steps: the router resumes from the
//! returned [`IndexPostingPurgeCursor`] until [`IndexPostingPurgeStepResult::done`].
//!
//! Scope (the posting key orders `property_id` first, so each scope is a
//! contiguous `property_id` range):
//! - **Vertex** postings carry no label, so the scope is the whole `property_id`
//!   range; driven only once the property is no longer referenced by any vertex
//!   index.
//! - **Edge** postings carry the catalog `label_id` (direction stripped), so the
//!   scope is `(property_id, label_id)`; driven once no remaining edge index
//!   references that `(property_id, label_id)`.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Which posting set a purge targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum IndexPurgeKind {
    Vertex,
    Edge,
}

/// Resume point for a bounded posting purge. A `None` resume starts a fresh
/// purge of the scope.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IndexPostingPurgeCursor {
    /// Encoded last-examined posting key; empty starts the scope.
    pub resume_key: Vec<u8>,
}

/// Progress from one bounded purge step.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IndexPostingPurgeStepResult {
    /// Resume point for the next step, or `None` once the purge is complete.
    pub next: Option<IndexPostingPurgeCursor>,
    /// Posting keys examined in this step (bounded by the index budget).
    pub examined: u32,
    /// Posting keys removed in this step.
    pub removed: u32,
    /// `true` when the purge is complete (`next` is `None`).
    pub done: bool,
}
