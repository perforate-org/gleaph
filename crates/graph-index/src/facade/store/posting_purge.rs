//! Bounded, resumable `DROP INDEX` posting purge (ADR 0023 D6).
//!
//! Closes P7: dropped indexes used to orphan their postings because `DROP INDEX`
//! only cleared the router catalog. The router now drives this purge after
//! removing the catalog entry, resuming each step until `done`. Posting keys
//! order `property_id` first, so each scope is a contiguous `property_id` range
//! (vertex: the whole range; edge: filtered by the catalog `label_id`, which the
//! key carries with direction stripped).

use super::IndexStore;
use crate::edge_key::EdgePostingKey;
use crate::key::PostingKey;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind,
};
use ic_stable_structures::{BTreeSet, Memory, Storable};
use std::borrow::Cow;
use std::ops::Bound;

use crate::facade::stable::{INDEX_EDGE_POSTINGS, INDEX_VERTEX_POSTINGS};

/// Upper bound on posting keys examined per purge step. Keeps a single message
/// within the canister instruction / stable read-write budgets regardless of
/// total index size; the router resumes until the purge reports `done`.
const MAX_PURGE_EXAMINE_PER_STEP: u32 = 20_000;

/// Outcome of purging up to `budget` keys from one posting set scope.
struct PurgeStep {
    examined: u32,
    removed: u32,
    /// Resume key when the scope was not fully scanned, or `None` when exhausted.
    resume_key: Option<Vec<u8>>,
}

/// Scans the `[range_lower, range_upper)` slice of `set` (resuming after
/// `resume_key`) up to `budget` keys, removing those for which `matches` holds.
/// Collects matches before removing so the scan does not mutate mid-iteration.
fn purge_range_step<K, M>(
    set: &mut BTreeSet<K, M>,
    range_lower: K,
    range_upper: Bound<K>,
    resume_key: &[u8],
    matches: impl Fn(&K) -> bool,
    budget: u32,
) -> PurgeStep
where
    K: Storable + Ord + Clone,
    M: Memory,
{
    let lower = if resume_key.is_empty() {
        Bound::Included(range_lower)
    } else {
        Bound::Excluded(K::from_bytes(Cow::Borrowed(resume_key)))
    };

    let mut examined = 0u32;
    let mut to_remove: Vec<K> = Vec::new();
    let mut last_key: Option<K> = None;
    let mut exhausted = true;
    {
        for key in set.range((lower, range_upper)) {
            if examined >= budget {
                exhausted = false;
                break;
            }
            examined += 1;
            if matches(&key) {
                to_remove.push(key.clone());
            }
            last_key = Some(key);
        }
    }
    let removed = u32::try_from(to_remove.len()).unwrap_or(u32::MAX);
    for key in &to_remove {
        set.remove(key);
    }
    let resume_key = if exhausted {
        None
    } else {
        last_key.map(Storable::into_bytes)
    };
    PurgeStep {
        examined,
        removed,
        resume_key,
    }
}

/// Exclusive upper bound for the contiguous `property_id` range of vertex keys.
fn vertex_property_upper(property_id: u32) -> Bound<PostingKey> {
    match property_id.checked_add(1) {
        Some(next) => Bound::Excluded(PostingKey::prefix_lower(next, &[])),
        None => Bound::Unbounded,
    }
}

/// Exclusive upper bound for the contiguous `property_id` range of edge keys.
fn edge_property_upper(property_id: u32) -> Bound<EdgePostingKey> {
    match property_id.checked_add(1) {
        Some(next) => Bound::Excluded(EdgePostingKey::prefix_lower(next, &[])),
        None => Bound::Unbounded,
    }
}

impl IndexStore {
    fn commit_purge_property_postings_step(
        &self,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
        budget: u32,
    ) -> IndexPostingPurgeStepResult {
        let resume_key = resume.map(|cursor| cursor.resume_key).unwrap_or_default();

        let step = match kind {
            IndexPurgeKind::Vertex => INDEX_VERTEX_POSTINGS.with_borrow_mut(|set| {
                purge_range_step(
                    set,
                    PostingKey::prefix_lower(property_id, &[]),
                    vertex_property_upper(property_id),
                    &resume_key,
                    |_key| true,
                    budget,
                )
            }),
            IndexPurgeKind::Edge => INDEX_EDGE_POSTINGS.with_borrow_mut(|set| {
                purge_range_step(
                    set,
                    EdgePostingKey::prefix_lower(property_id, &[]),
                    edge_property_upper(property_id),
                    &resume_key,
                    |key| key.label_id == label_id,
                    budget,
                )
            }),
        };

        let next = step
            .resume_key
            .map(|resume_key| IndexPostingPurgeCursor { resume_key });
        IndexPostingPurgeStepResult {
            done: next.is_none(),
            next,
            examined: step.examined,
            removed: step.removed,
        }
    }

    /// Performs one bounded step of a `DROP INDEX` posting purge. The router
    /// resumes from [`IndexPostingPurgeStepResult::next`] until `done`. For
    /// vertex purges `label_id` is ignored.
    pub fn admin_purge_property_postings(
        &self,
        caller: Principal,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
    ) -> Result<IndexPostingPurgeStepResult, IndexError> {
        self.assert_router_caller(caller)?;
        Ok(self.commit_purge_property_postings_step(
            kind,
            property_id,
            label_id,
            resume,
            MAX_PURGE_EXAMINE_PER_STEP,
        ))
    }

    #[cfg(test)]
    pub fn purge_property_postings_step_for_test(
        &self,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
        budget: u32,
    ) -> IndexPostingPurgeStepResult {
        self.commit_purge_property_postings_step(kind, property_id, label_id, resume, budget)
    }
}
