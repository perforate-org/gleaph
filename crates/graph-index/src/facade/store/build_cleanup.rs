//! Bounded, resumable cleanup for one aborted ADR 0059 physical namespace.

use std::borrow::Cow;
use std::ops::Bound;

use candid::Principal;
use gleaph_graph_kernel::index::{
    IndexBuildCleanupStatus, IndexBuildControlRequest, PhysicalIndexId,
};
use ic_stable_structures::{BTreeSet, Memory, Storable};

use super::IndexStore;
use super::build_state::ensure_control;
use crate::build_key::IndexBuildTouchedKey;
use crate::edge_key::EdgePostingKey;
use crate::facade::stable::build_state::{
    IndexBuildCleanupCursor, IndexBuildCleanupPhase, IndexBuildLifecycle,
};
use crate::facade::stable::{
    INDEX_BUILD_STATES, INDEX_BUILD_TOUCHED_SUBJECTS, INDEX_EDGE_POSTINGS, INDEX_VERTEX_POSTINGS,
};
use crate::key::PostingKey;
use crate::state::IndexError;

const MAX_BUILD_CLEANUP_KEYS_PER_STEP: u32 = 10_000;

trait PhysicalNamespaceKey: Storable + Ord + Clone {
    fn physical_index_id(&self) -> PhysicalIndexId;
    fn namespace_lower(physical_index_id: PhysicalIndexId) -> Self;
}

impl PhysicalNamespaceKey for PostingKey {
    fn physical_index_id(&self) -> PhysicalIndexId {
        self.physical_index_id
    }

    fn namespace_lower(physical_index_id: PhysicalIndexId) -> Self {
        Self::prefix_lower(physical_index_id, 0, &[])
    }
}

impl PhysicalNamespaceKey for EdgePostingKey {
    fn physical_index_id(&self) -> PhysicalIndexId {
        self.physical_index_id
    }

    fn namespace_lower(physical_index_id: PhysicalIndexId) -> Self {
        Self::prefix_lower(physical_index_id, 0, &[])
    }
}

impl PhysicalNamespaceKey for IndexBuildTouchedKey {
    fn physical_index_id(&self) -> PhysicalIndexId {
        self.physical_index_id
    }

    fn namespace_lower(physical_index_id: PhysicalIndexId) -> Self {
        Self::prefix_lower(physical_index_id)
    }
}

fn purge_namespace_step<K, M>(
    set: &mut BTreeSet<K, M>,
    physical_index_id: PhysicalIndexId,
    resume_key: &[u8],
    budget: u32,
) -> Option<Vec<u8>>
where
    K: PhysicalNamespaceKey,
    M: Memory,
{
    let lower = if resume_key.is_empty() {
        Bound::Included(K::namespace_lower(physical_index_id))
    } else {
        Bound::Excluded(K::from_bytes(Cow::Borrowed(resume_key)))
    };
    let upper = physical_index_id
        .checked_next()
        .map_or(Bound::Unbounded, |next| {
            Bound::Excluded(K::namespace_lower(next))
        });
    let mut to_remove = Vec::new();
    let mut last = None;
    let mut exhausted = true;
    for key in set.range((lower, upper)) {
        if to_remove.len() >= budget as usize {
            exhausted = false;
            break;
        }
        debug_assert_eq!(key.physical_index_id(), physical_index_id);
        last = Some(key.clone());
        to_remove.push(key);
    }
    for key in &to_remove {
        set.remove(key);
    }
    if exhausted {
        None
    } else {
        last.map(Storable::into_bytes)
    }
}

impl IndexStore {
    fn commit_abort_index_build_step(
        &self,
        physical_index_id: PhysicalIndexId,
        mut state: crate::facade::stable::build_state::IndexBuildState,
        budget: u32,
    ) -> IndexBuildCleanupStatus {
        if matches!(&state.lifecycle, IndexBuildLifecycle::Aborted) {
            return IndexBuildCleanupStatus { done: true };
        }
        let cleanup = match &state.lifecycle {
            IndexBuildLifecycle::Aborting { cleanup } => cleanup.clone(),
            IndexBuildLifecycle::Building | IndexBuildLifecycle::Sealing { .. } => {
                IndexBuildCleanupCursor {
                    phase: IndexBuildCleanupPhase::VertexPostings,
                    resume_key: Vec::new(),
                }
            }
            IndexBuildLifecycle::Aborted => unreachable!(),
        };

        let next_key = match cleanup.phase {
            IndexBuildCleanupPhase::VertexPostings => {
                INDEX_VERTEX_POSTINGS.with_borrow_mut(|set| {
                    purge_namespace_step(set, physical_index_id, &cleanup.resume_key, budget)
                })
            }
            IndexBuildCleanupPhase::EdgePostings => INDEX_EDGE_POSTINGS.with_borrow_mut(|set| {
                purge_namespace_step(set, physical_index_id, &cleanup.resume_key, budget)
            }),
            IndexBuildCleanupPhase::TouchedSubjects => INDEX_BUILD_TOUCHED_SUBJECTS
                .with_borrow_mut(|set| {
                    purge_namespace_step(set, physical_index_id, &cleanup.resume_key, budget)
                }),
        };

        state.lifecycle = match next_key {
            Some(resume_key) => IndexBuildLifecycle::Aborting {
                cleanup: IndexBuildCleanupCursor {
                    phase: cleanup.phase,
                    resume_key,
                },
            },
            None => match cleanup.phase {
                IndexBuildCleanupPhase::VertexPostings => IndexBuildLifecycle::Aborting {
                    cleanup: IndexBuildCleanupCursor {
                        phase: IndexBuildCleanupPhase::EdgePostings,
                        resume_key: Vec::new(),
                    },
                },
                IndexBuildCleanupPhase::EdgePostings => IndexBuildLifecycle::Aborting {
                    cleanup: IndexBuildCleanupCursor {
                        phase: IndexBuildCleanupPhase::TouchedSubjects,
                        resume_key: Vec::new(),
                    },
                },
                IndexBuildCleanupPhase::TouchedSubjects => IndexBuildLifecycle::Aborted,
            },
        };
        let done = matches!(&state.lifecycle, IndexBuildLifecycle::Aborted);
        INDEX_BUILD_STATES.with_borrow_mut(|states| {
            states.insert(physical_index_id, state);
        });
        IndexBuildCleanupStatus { done }
    }

    pub fn abort_index_build(
        &self,
        caller: Principal,
        control: &IndexBuildControlRequest,
    ) -> Result<IndexBuildCleanupStatus, IndexError> {
        self.assert_router_caller(caller)?;
        let physical_index_id = control.registration.physical_index_id;
        let state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        ensure_control(&state, control)?;
        Ok(self.commit_abort_index_build_step(
            physical_index_id,
            state,
            MAX_BUILD_CLEANUP_KEYS_PER_STEP,
        ))
    }

    #[cfg(test)]
    pub(crate) fn abort_index_build_step_for_test(
        &self,
        caller: Principal,
        control: &IndexBuildControlRequest,
        budget: u32,
    ) -> Result<IndexBuildCleanupStatus, IndexError> {
        self.assert_router_caller(caller)?;
        let physical_index_id = control.registration.physical_index_id;
        let state = INDEX_BUILD_STATES
            .with_borrow(|states| states.get(&physical_index_id))
            .ok_or(IndexError::UnknownIndexBuild)?;
        ensure_control(&state, control)?;
        Ok(self.commit_abort_index_build_step(physical_index_id, state, budget))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_lower_keys_keep_physical_id_first() {
        let first = PhysicalIndexId::new(1).expect("first id");
        let second = PhysicalIndexId::new(2).expect("second id");
        assert!(PostingKey::namespace_lower(first) < PostingKey::namespace_lower(second));
        assert!(EdgePostingKey::namespace_lower(first) < EdgePostingKey::namespace_lower(second));
        assert!(
            IndexBuildTouchedKey::namespace_lower(first)
                < IndexBuildTouchedKey::namespace_lower(second)
        );
    }
}
