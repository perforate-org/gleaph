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
//!   range.
//! - **Edge** postings carry the catalog `label_id` (direction stripped), so the
//!   scope is `(property_id, label_id)`.
//!
//! Every scope belongs to exactly one physical namespace: a dropped index definition
//! always retires its own namespace unconditionally — no reader can reach a namespace
//! whose catalog row is gone, and a sibling index on the same property owns a disjoint
//! range. The Router keeps the durable retirement identity (and per-target resume
//! cursors) in its retired-index records so purge progress survives response loss,
//! stopped targets, and upgrades without any caller in the loop.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use crate::index::PhysicalIndexId;

/// Which posting set a purge targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum IndexPurgeKind {
    Vertex,
    Edge,
}

/// Resume point for a bounded posting purge. A `None` resume starts a fresh purge of the scope.
///
/// The target identity is part of the cursor so a key obtained from one namespace or logical
/// purge cannot be replayed against another purge.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum IndexPostingPurgeCursor {
    Vertex {
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        resume_key: Vec<u8>,
    },
    Edge {
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        label_id: u16,
        resume_key: Vec<u8>,
    },
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
