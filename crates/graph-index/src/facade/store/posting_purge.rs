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
use crate::posting_range::property_end_exclusive;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind,
};
use gleaph_graph_kernel::index::PhysicalIndexId;
use ic_stable_structures::{BTreeSet, Memory, Storable};
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
    resume_key: Option<K>,
    matches: impl Fn(&K) -> bool,
    budget: u32,
) -> PurgeStep
where
    K: Storable + Ord + Clone,
    M: Memory,
{
    let lower = match resume_key {
        Some(key) => Bound::Excluded(key),
        None => Bound::Included(range_lower),
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

fn decode_vertex_resume(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<Option<PostingKey>, IndexError> {
    let (cursor_physical_index_id, cursor_property_id, resume_key) = match resume {
        None => return Ok(None),
        Some(IndexPostingPurgeCursor::Vertex {
            physical_index_id,
            property_id,
            resume_key,
        }) => (physical_index_id, property_id, resume_key),
        Some(IndexPostingPurgeCursor::Edge { .. }) => {
            return Err(IndexError::InvalidPostingPurgeCursor);
        }
    };
    if cursor_physical_index_id != physical_index_id || cursor_property_id != property_id {
        return Err(IndexError::InvalidPostingPurgeCursor);
    }
    let key = PostingKey::decode(&resume_key).ok_or(IndexError::InvalidPostingPurgeCursor)?;
    if key.encode() != resume_key
        || key.physical_index_id != physical_index_id
        || key.property_id != property_id
    {
        return Err(IndexError::InvalidPostingPurgeCursor);
    }
    Ok(Some(key))
}

fn decode_edge_resume(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<Option<EdgePostingKey>, IndexError> {
    let (cursor_physical_index_id, cursor_property_id, cursor_label_id, resume_key) = match resume {
        None => return Ok(None),
        Some(IndexPostingPurgeCursor::Edge {
            physical_index_id,
            property_id,
            label_id,
            resume_key,
        }) => (physical_index_id, property_id, label_id, resume_key),
        Some(IndexPostingPurgeCursor::Vertex { .. }) => {
            return Err(IndexError::InvalidPostingPurgeCursor);
        }
    };
    if cursor_physical_index_id != physical_index_id
        || cursor_property_id != property_id
        || cursor_label_id != label_id
    {
        return Err(IndexError::InvalidPostingPurgeCursor);
    }
    let key = EdgePostingKey::decode(&resume_key).ok_or(IndexError::InvalidPostingPurgeCursor)?;
    if key.encode() != resume_key
        || key.physical_index_id != physical_index_id
        || key.property_id != property_id
    {
        return Err(IndexError::InvalidPostingPurgeCursor);
    }
    Ok(Some(key))
}

/// Exclusive upper bound for the contiguous `property_id` range of edge keys.
fn edge_property_upper(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
) -> Bound<EdgePostingKey> {
    match property_id.checked_add(1) {
        Some(next) => Bound::Excluded(EdgePostingKey::prefix_lower(physical_index_id, next, &[])),
        None => physical_index_id
            .checked_next()
            .map_or(Bound::Unbounded, |next| {
                Bound::Excluded(EdgePostingKey::prefix_lower(next, 0, &[]))
            }),
    }
}

impl IndexStore {
    fn commit_purge_property_postings_step(
        &self,
        physical_index_id: PhysicalIndexId,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
        budget: u32,
    ) -> Result<IndexPostingPurgeStepResult, IndexError> {
        let step = match kind {
            IndexPurgeKind::Vertex => {
                let resume_key = decode_vertex_resume(physical_index_id, property_id, resume)?;
                INDEX_VERTEX_POSTINGS.with_borrow_mut(|set| {
                    purge_range_step(
                        set,
                        PostingKey::prefix_lower(physical_index_id, property_id, &[]),
                        property_end_exclusive(physical_index_id, property_id),
                        resume_key,
                        |_key| true,
                        budget,
                    )
                })
            }
            IndexPurgeKind::Edge => {
                let resume_key =
                    decode_edge_resume(physical_index_id, property_id, label_id, resume)?;
                INDEX_EDGE_POSTINGS.with_borrow_mut(|set| {
                    purge_range_step(
                        set,
                        EdgePostingKey::prefix_lower(physical_index_id, property_id, &[]),
                        edge_property_upper(physical_index_id, property_id),
                        resume_key,
                        |key| key.label_id == label_id,
                        budget,
                    )
                })
            }
        };

        let next = step.resume_key.map(|resume_key| match kind {
            IndexPurgeKind::Vertex => IndexPostingPurgeCursor::Vertex {
                physical_index_id,
                property_id,
                resume_key,
            },
            IndexPurgeKind::Edge => IndexPostingPurgeCursor::Edge {
                physical_index_id,
                property_id,
                label_id,
                resume_key,
            },
        });
        Ok(IndexPostingPurgeStepResult {
            done: next.is_none(),
            next,
            examined: step.examined,
            removed: step.removed,
        })
    }

    /// Performs one bounded step of a `DROP INDEX` posting purge. The router
    /// resumes from [`IndexPostingPurgeStepResult::next`] until `done`. For
    /// vertex purges `label_id` is ignored.
    pub fn admin_purge_property_postings(
        &self,
        caller: Principal,
        physical_index_id: PhysicalIndexId,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
    ) -> Result<IndexPostingPurgeStepResult, IndexError> {
        self.assert_router_caller(caller)?;
        self.commit_purge_property_postings_step(
            physical_index_id,
            kind,
            property_id,
            label_id,
            resume,
            MAX_PURGE_EXAMINE_PER_STEP,
        )
    }

    #[cfg(test)]
    pub fn purge_property_postings_step_for_test(
        &self,
        physical_index_id: PhysicalIndexId,
        kind: IndexPurgeKind,
        property_id: u32,
        label_id: u16,
        resume: Option<IndexPostingPurgeCursor>,
        budget: u32,
    ) -> Result<IndexPostingPurgeStepResult, IndexError> {
        self.commit_purge_property_postings_step(
            physical_index_id,
            kind,
            property_id,
            label_id,
            resume,
            budget,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::IndexInitArgs;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;

    const PHYSICAL_INDEX_ID: PhysicalIndexId = PhysicalIndexId::new(1).expect("test physical id");

    fn setup() -> (IndexStore, Principal) {
        let store = IndexStore::new();
        let router = Principal::from_slice(&[91]);
        let shard = Principal::from_slice(&[92]);
        store
            .init_from_args(&IndexInitArgs {
                router_canister: router,
            })
            .expect("initialize graph-index");
        store
            .admin_attach_shard_canister(router, GraphId::from_raw(1), 2, 0, ShardId::new(0), shard)
            .expect("attach test shard");
        (store, shard)
    }

    fn seed_vertex_postings(store: &IndexStore, shard: Principal, count: u32) {
        for vertex_id in 1..=count {
            store
                .posting_insert(
                    shard,
                    ShardId::new(0),
                    PHYSICAL_INDEX_ID,
                    42,
                    b"v".to_vec(),
                    vertex_id,
                )
                .expect("insert vertex posting");
        }
    }

    fn seed_edge_postings(store: &IndexStore, shard: Principal, count: u32, label_id: u16) {
        for owner in 1..=count {
            store
                .edge_posting_insert(
                    shard,
                    ShardId::new(0),
                    PHYSICAL_INDEX_ID,
                    42,
                    b"e".to_vec(),
                    label_id,
                    owner,
                    0,
                )
                .expect("insert edge posting");
        }
    }

    fn first_vertex_cursor(store: &IndexStore) -> IndexPostingPurgeCursor {
        store
            .purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                None,
                1,
            )
            .expect("vertex purge step")
            .next
            .expect("vertex purge resumes")
    }

    fn first_edge_cursor(store: &IndexStore) -> IndexPostingPurgeCursor {
        store
            .purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Edge,
                42,
                3,
                None,
                1,
            )
            .expect("edge purge step")
            .next
            .expect("edge purge resumes")
    }

    #[test]
    fn purge_rejects_cursor_of_the_other_kind() {
        let (store, shard) = setup();
        seed_vertex_postings(&store, shard, 2);
        seed_edge_postings(&store, shard, 2, 3);
        let vertex_cursor = first_vertex_cursor(&store);
        let edge_cursor = first_edge_cursor(&store);

        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(edge_cursor),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Edge,
                42,
                3,
                Some(vertex_cursor),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
    }

    #[test]
    fn purge_rejects_cursor_from_another_physical_index_id() {
        let (store, shard) = setup();
        seed_vertex_postings(&store, shard, 2);
        let IndexPostingPurgeCursor::Vertex {
            property_id,
            resume_key,
            ..
        } = first_vertex_cursor(&store)
        else {
            unreachable!("vertex cursor");
        };
        let foreign = IndexPostingPurgeCursor::Vertex {
            physical_index_id: PhysicalIndexId::new(999).expect("other namespace"),
            property_id,
            resume_key,
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(foreign),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
    }

    #[test]
    fn purge_rejects_cursor_from_another_property_id() {
        let (store, shard) = setup();
        seed_vertex_postings(&store, shard, 2);
        let IndexPostingPurgeCursor::Vertex {
            physical_index_id,
            resume_key,
            ..
        } = first_vertex_cursor(&store)
        else {
            unreachable!("vertex cursor");
        };
        let foreign = IndexPostingPurgeCursor::Vertex {
            physical_index_id,
            property_id: 43,
            resume_key,
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(foreign),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
    }

    #[test]
    fn edge_purge_rejects_cursor_from_another_label_id() {
        let (store, shard) = setup();
        seed_edge_postings(&store, shard, 2, 3);
        let IndexPostingPurgeCursor::Edge {
            physical_index_id,
            property_id,
            resume_key,
            ..
        } = first_edge_cursor(&store)
        else {
            unreachable!("edge cursor");
        };
        let foreign = IndexPostingPurgeCursor::Edge {
            physical_index_id,
            property_id,
            label_id: 7,
            resume_key,
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Edge,
                42,
                3,
                Some(foreign),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
    }

    #[test]
    fn purge_rejects_tampered_or_undecodable_resume_key() {
        let (store, shard) = setup();
        seed_vertex_postings(&store, shard, 2);
        seed_edge_postings(&store, shard, 2, 3);
        let vertex_cursor = first_vertex_cursor(&store);
        let edge_cursor = first_edge_cursor(&store);

        // Bytes that do not decode as a PostingKey / EdgePostingKey.
        let undecodable = IndexPostingPurgeCursor::Vertex {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 42,
            resume_key: vec![0x00, 0x01],
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(undecodable),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
        let undecodable_edge = IndexPostingPurgeCursor::Edge {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 42,
            label_id: 3,
            resume_key: Vec::new(),
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Edge,
                42,
                3,
                Some(undecodable_edge),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );

        // A decodable key whose identity contradicts the purge scope is rejected even though the
        // envelope fields match.
        let IndexPostingPurgeCursor::Vertex { resume_key, .. } = vertex_cursor else {
            unreachable!("vertex cursor");
        };
        let mut wrong_property_key = PostingKey::decode(&resume_key).expect("cursor key decodes");
        wrong_property_key.property_id = 99;
        let wrong_property = IndexPostingPurgeCursor::Vertex {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 42,
            resume_key: wrong_property_key.encode(),
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(wrong_property),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
        let mut wrong_namespace_key = PostingKey::decode(&resume_key).expect("cursor key decodes");
        wrong_namespace_key.physical_index_id = PhysicalIndexId::new(999).expect("other namespace");
        let wrong_namespace = IndexPostingPurgeCursor::Vertex {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 42,
            resume_key: wrong_namespace_key.encode(),
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Vertex,
                42,
                0,
                Some(wrong_namespace),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );

        let IndexPostingPurgeCursor::Edge { resume_key, .. } = edge_cursor else {
            unreachable!("edge cursor");
        };
        let mut wrong_edge_key =
            EdgePostingKey::decode(&resume_key).expect("edge cursor key decodes");
        wrong_edge_key.property_id = 99;
        let wrong_edge_property = IndexPostingPurgeCursor::Edge {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 42,
            label_id: 3,
            resume_key: wrong_edge_key.encode(),
        };
        assert_eq!(
            store.purge_property_postings_step_for_test(
                PHYSICAL_INDEX_ID,
                IndexPurgeKind::Edge,
                42,
                3,
                Some(wrong_edge_property),
                100,
            ),
            Err(IndexError::InvalidPostingPurgeCursor)
        );
    }

    #[test]
    fn edge_purge_resume_continues_after_last_examined_key_without_loss_or_duplicate() {
        let (store, shard) = setup();
        // Alternate labels so a resume key may be a non-matching examined key: the resume must
        // continue after the last *examined* key, not after the last removal.
        for (owner, label) in [(1u32, 3u16), (2, 7), (3, 3), (4, 7)] {
            store
                .edge_posting_insert(
                    shard,
                    ShardId::new(0),
                    PHYSICAL_INDEX_ID,
                    42,
                    b"e".to_vec(),
                    label,
                    owner,
                    0,
                )
                .expect("insert edge posting");
        }

        let mut resume = None;
        let mut steps = 0u32;
        let mut examined_total = 0u32;
        let mut removed_total = 0u32;
        loop {
            let step = store
                .purge_property_postings_step_for_test(
                    PHYSICAL_INDEX_ID,
                    IndexPurgeKind::Edge,
                    42,
                    3,
                    resume,
                    1,
                )
                .expect("bounded edge purge step");
            steps += 1;
            assert!(step.examined <= 1);
            examined_total += step.examined;
            removed_total += step.removed;
            match step.next {
                Some(cursor) => resume = Some(cursor),
                None => break,
            }
            assert!(steps < 100, "bounded edge purge did not converge");
        }
        // Every key is examined exactly once and only label-3 keys are removed. A resume that
        // re-scanned or skipped a key would change these totals.
        assert_eq!(steps, 4);
        assert_eq!(examined_total, 4);
        assert_eq!(removed_total, 2);
        assert!(
            store
                .lookup_edge_equal(PHYSICAL_INDEX_ID, 42, b"e", Some(3))
                .expect("lookup label 3")
                .is_empty()
        );
        assert_eq!(
            store
                .lookup_edge_equal(PHYSICAL_INDEX_ID, 42, b"e", Some(7))
                .expect("lookup label 7")
                .len(),
            2
        );
    }
}
