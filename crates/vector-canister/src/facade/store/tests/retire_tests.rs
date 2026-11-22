//! ADR 0037 §Resource deletion (shared vector target): `admin_retire_vector_index` purges one
//! index's rows, leaves every other index of the canister intact, and fences the retired id
//! against late sync ops for good.

use super::*;
use crate::facade::stable::VECTOR_DELETED_SUBJECTS;
use crate::records::VectorIndexDefinitionEntry;
use gleaph_graph_kernel::vector_index::{
    VectorIndexRetireCursor, VectorIndexRetireStep, VectorSyncBatchOutcome,
};

const OTHER_INDEX: u32 = 2;

fn op_for(index_id: u32, op: VectorEmbeddingSyncOp) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp { index_id, ..op }
}

/// Index 1 holds two live subjects and one tombstone (a deleted-list entry); index 2 holds one
/// live subject.
fn two_populated_indexes() {
    fresh_store();
    for vertex in [1, 2, 3] {
        vector_upsert(shard_canister(), &upsert_op(vertex, 1, vertex as u8)).expect("upsert");
    }
    vector_remove(shard_canister(), &remove_op(3, 2)).expect("remove");
    vector_upsert(
        shard_canister(),
        &op_for(OTHER_INDEX, upsert_op(1, 1, 0x40)),
    )
    .expect("other index upsert");
}

fn entry(index_id: u32) -> Option<VectorIndexDefinitionEntry> {
    definition_store::entry(index_id).expect("definition region readable")
}

fn step(index_id: u32, resume: Option<VectorIndexRetireCursor>) -> VectorIndexRetireStep {
    retire_step_with_budget(index_id, resume, 1).expect("retire step")
}

/// Drives the retirement to completion one subject or page at a time.
fn retire_fully(index_id: u32) -> u32 {
    let mut steps = 0;
    let mut resume = None;
    loop {
        steps += 1;
        match step(index_id, resume).next {
            Some(next) => resume = Some(next),
            None => return steps,
        }
    }
}

fn deleted_list_holds(index_id: u32) -> bool {
    VECTOR_DELETED_SUBJECTS.with_borrow(|list| {
        list.iter()
            .any(|entry| entry.key().subject.index_id == index_id)
    })
}

fn indexes_with_pages() -> Vec<u32> {
    admin_vector_slab_stats(router(), None)
        .expect("slab stats")
        .versions
        .iter()
        .map(|version| version.index_id)
        .collect()
}

#[test]
fn retirement_purges_only_the_retired_index() {
    two_populated_indexes();
    assert!(deleted_list_holds(INDEX_ID));
    assert!(indexes_with_pages().contains(&INDEX_ID));

    let steps = retire_fully(INDEX_ID);

    assert!(
        steps > 2,
        "a one-unit budget must take several bounded steps"
    );
    assert_eq!(entry(INDEX_ID), Some(VectorIndexDefinitionEntry::Retired));
    assert!(def_for_test(INDEX_ID).is_none());
    for vertex in [1, 2, 3] {
        assert!(subject_entry_for_test(INDEX_ID, subject(vertex)).is_none());
    }
    assert!(!deleted_list_holds(INDEX_ID));
    assert!(partition_head_for_test(INDEX_ID, INITIAL_INDEX_VERSION).is_none());
    assert!(!indexes_with_pages().contains(&INDEX_ID));
    assert!(
        crate::facade::stable::IVF_CENTROID_META
            .with_borrow(|meta| meta.get(&INDEX_ID))
            .is_none()
    );

    // The neighbour index keeps its definition, subject, pages and search results.
    assert!(def_for_test(OTHER_INDEX).is_some());
    assert!(subject_entry_for_test(OTHER_INDEX, subject(1)).is_some());
    assert!(indexes_with_pages().contains(&OTHER_INDEX));
    let hits = vector_search(&VectorSearchRequest {
        index_id: OTHER_INDEX,
        ..search_value(0.0, 1)
    })
    .expect("neighbour search");
    assert_eq!(hits.hits.len(), 1);
}

#[test]
fn a_retired_index_refuses_upserts_and_acknowledges_removes_without_a_tombstone() {
    two_populated_indexes();
    retire_fully(INDEX_ID);

    assert_eq!(
        vector_upsert(shard_canister(), &upsert_op(9, 5, 0x11)),
        Err(VectorCanisterError::IndexRetired)
    );
    assert_eq!(vector_remove(shard_canister(), &remove_op(9, 5)), Ok(()));
    let batch =
        crate::canister::vector_sync_batch_outcome_for_caller(shard_canister(), &[remove_op(8, 6)])
            .expect("typed remove batch");
    assert_eq!(batch, VectorSyncBatchOutcome::Progress { applied: 1 });

    assert_eq!(entry(INDEX_ID), Some(VectorIndexDefinitionEntry::Retired));
    for vertex in [8, 9] {
        assert!(subject_entry_for_test(INDEX_ID, subject(vertex)).is_none());
    }
    assert!(!deleted_list_holds(INDEX_ID));
}

#[test]
fn a_retiring_index_is_hidden_and_a_lost_cursor_restarts_the_purge() {
    two_populated_indexes();
    let first = step(INDEX_ID, None);
    assert!(first.next.is_some());
    assert!(matches!(
        entry(INDEX_ID),
        Some(VectorIndexDefinitionEntry::Retiring(_))
    ));

    // Every reader sees no definition; writes are fenced exactly as after retirement.
    assert!(def_for_test(INDEX_ID).is_none());
    assert_eq!(
        vector_search(&search_value(0.0, 1)).expect("search").hits,
        Vec::new(),
        "a definitionless index answers with no hits"
    );
    assert_eq!(
        vector_upsert(shard_canister(), &upsert_op(1, 9, 0x22)),
        Err(VectorCanisterError::IndexRetired)
    );
    assert_eq!(vector_remove(shard_canister(), &remove_op(2, 9)), Ok(()));

    // The Router lost the cursor: a fresh `None` resumes the same retirement to completion.
    retire_fully(INDEX_ID);
    assert_eq!(entry(INDEX_ID), Some(VectorIndexDefinitionEntry::Retired));
    assert!(!indexes_with_pages().contains(&INDEX_ID));
}

#[test]
fn the_slab_reopens_while_a_retiring_index_still_has_pages() {
    two_populated_indexes();
    let mut resume = None;
    // Run the subject sub-stage to its end, leaving every page in place.
    loop {
        let next = step(INDEX_ID, resume).next.expect("pages remain");
        if matches!(next, VectorIndexRetireCursor::Pages { .. }) {
            break;
        }
        resume = Some(next);
    }
    assert!(indexes_with_pages().contains(&INDEX_ID));

    // An upgrade between steps reopens the slab: the retiring definition still vouches for the
    // geometry of its remaining pages.
    let _reopened = crate::facade::stable::page_store::VectorSlabStore::init();

    retire_fully(INDEX_ID);
    assert!(!indexes_with_pages().contains(&INDEX_ID));
}

#[test]
fn a_finished_retirement_replays_as_done_and_a_never_seen_index_is_fenced() {
    two_populated_indexes();
    retire_fully(INDEX_ID);
    assert_eq!(step(INDEX_ID, None).next, None);

    let unseen = 77;
    assert_eq!(step(unseen, None).next, None);
    assert_eq!(entry(unseen), Some(VectorIndexDefinitionEntry::Retired));
    assert_eq!(
        vector_upsert(shard_canister(), &op_for(unseen, upsert_op(1, 1, 0x33))),
        Err(VectorCanisterError::IndexRetired)
    );
    assert!(entry(unseen) == Some(VectorIndexDefinitionEntry::Retired));
}

#[test]
fn retirement_is_refused_while_a_rebuild_owns_a_second_generation() {
    two_populated_indexes();
    admin_start_vector_rebuild(router(), INDEX_ID, 2, 100).expect("rebuild starts");

    assert_eq!(
        retire_step_with_budget(INDEX_ID, None, 1),
        Err(VectorCanisterError::RebuildAlreadyActive)
    );
    assert!(matches!(
        entry(INDEX_ID),
        Some(VectorIndexDefinitionEntry::Live(_))
    ));
}

#[test]
fn a_cursor_for_an_index_that_is_not_retiring_is_stale() {
    two_populated_indexes();
    let cursor = VectorIndexRetireCursor::Pages { resume_key: None };
    for index_id in [INDEX_ID, 55] {
        assert_eq!(
            retire_step_with_budget(index_id, Some(cursor.clone()), 1),
            Err(VectorCanisterError::StaleRetireCursor)
        );
    }
    assert!(matches!(
        entry(INDEX_ID),
        Some(VectorIndexDefinitionEntry::Live(_))
    ));
    assert_eq!(entry(55), None);
}

#[test]
fn only_the_router_may_retire_an_index() {
    two_populated_indexes();
    assert_eq!(
        admin_retire_vector_index(shard_canister(), INDEX_ID, None),
        Err(VectorCanisterError::Unauthorized)
    );
    assert!(matches!(
        entry(INDEX_ID),
        Some(VectorIndexDefinitionEntry::Live(_))
    ));
}
