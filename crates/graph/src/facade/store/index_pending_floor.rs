//! Atomic owner-local co-updates for the ordinary derived-index pending floor.

use super::super::stable::derived_index_outbox::{DerivedIndexOutboxEntry, DerivedIndexOutboxOp};
use super::super::stable::index_pending_floor::{IndexPendingFloorKey, IndexPendingFloorOwner};
use super::super::stable::repair_journal::{RepairJournalEntry, RepairPostingOp};
use super::super::stable::{DERIVED_INDEX_OUTBOX, INDEX_PENDING_FLOOR, INDEX_REPAIR_JOURNAL};
use super::GraphStore;
use gleaph_graph_kernel::index::IndexBuildDmlRequest;
use gleaph_graph_kernel::vector_index::VectorSyncBatchOutcome;

enum DurableSourceRow<'a> {
    RepairJournal(&'a RepairJournalEntry),
    DerivedIndexOutbox(&'a DerivedIndexOutboxEntry),
}

fn tracked_floor_key(
    sequence: u64,
    row: DurableSourceRow<'_>,
) -> Result<Option<IndexPendingFloorKey>, &'static str> {
    let (mutation_id, owner, ordinary) = match row {
        DurableSourceRow::RepairJournal(entry) => (
            entry.mutation_id,
            IndexPendingFloorOwner::RepairJournal,
            true,
        ),
        DurableSourceRow::DerivedIndexOutbox(entry) => (
            entry.mutation_id,
            IndexPendingFloorOwner::DerivedIndexOutbox,
            matches!(entry.op, DerivedIndexOutboxOp::Ordinary(_)),
        ),
    };
    if mutation_id == 0 || !ordinary {
        return Ok(None);
    }
    IndexPendingFloorKey::new(mutation_id, owner, sequence).map(Some)
}

fn preflight_new_keys(keys: &[IndexPendingFloorKey]) {
    INDEX_PENDING_FLOOR.with_borrow(|floor| {
        assert!(
            keys.iter().all(|key| !floor.contains(key)),
            "index pending floor source identity already exists"
        );
    });
}

fn preflight_existing_keys(keys: &[IndexPendingFloorKey]) {
    INDEX_PENDING_FLOOR.with_borrow(|floor| {
        assert!(
            keys.iter().all(|key| floor.contains(key)),
            "index pending floor source identity is missing"
        );
    });
}

impl GraphStore {
    pub(crate) fn index_pending_min_mutation_id(&self) -> Option<u64> {
        INDEX_PENDING_FLOOR.with_borrow(|floor| floor.min_mutation_id())
    }

    pub(crate) fn repair_journal_append(
        &self,
        mutation_id: u64,
        ops: impl IntoIterator<Item = RepairPostingOp>,
    ) {
        let rows = INDEX_REPAIR_JOURNAL
            .with_borrow(|journal| journal.prepare_append_all(mutation_id, ops))
            .unwrap_or_else(|error| panic!("prevalidate repair journal append: {error}"));
        let keys: Vec<_> = rows
            .iter()
            .filter_map(|(sequence, entry)| {
                tracked_floor_key(*sequence, DurableSourceRow::RepairJournal(entry))
                    .expect("derive repair journal floor key")
            })
            .collect();
        preflight_new_keys(&keys);

        INDEX_REPAIR_JOURNAL.with_borrow_mut(|journal| journal.append_prepared(rows));
        INDEX_PENDING_FLOOR.with_borrow_mut(|floor| {
            for key in keys {
                floor.insert(key);
            }
        });
    }

    pub(crate) fn derived_index_outbox_append(
        &self,
        mutation_id: u64,
        ops: impl IntoIterator<Item = RepairPostingOp>,
    ) {
        let rows = DERIVED_INDEX_OUTBOX
            .with_borrow(|outbox| outbox.prepare_append_all(mutation_id, ops))
            .unwrap_or_else(|error| panic!("prevalidate derived-index outbox append: {error}"));
        self.append_prepared_outbox_rows(rows);
    }

    pub(crate) fn derived_index_build_outbox_append(
        &self,
        mutation_id: u64,
        requests: impl IntoIterator<Item = IndexBuildDmlRequest>,
    ) {
        let rows = DERIVED_INDEX_OUTBOX
            .with_borrow(|outbox| outbox.prepare_append_build_dml(mutation_id, requests))
            .unwrap_or_else(|error| {
                panic!("prevalidate derived-index build outbox append: {error}")
            });
        self.append_prepared_outbox_rows(rows);
    }

    fn append_prepared_outbox_rows(&self, rows: Vec<(u64, DerivedIndexOutboxEntry)>) {
        let keys: Vec<_> = rows
            .iter()
            .filter_map(|(sequence, entry)| {
                tracked_floor_key(*sequence, DurableSourceRow::DerivedIndexOutbox(entry))
                    .expect("derive derived-index outbox floor key")
            })
            .collect();
        preflight_new_keys(&keys);

        DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.append_prepared(rows));
        INDEX_PENDING_FLOOR.with_borrow_mut(|floor| {
            for key in keys {
                floor.insert(key);
            }
        });
    }

    pub(crate) fn repair_journal_remove(&self, sequence: u64) {
        let Some(entry) = INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.get(sequence)) else {
            return;
        };
        let key = tracked_floor_key(sequence, DurableSourceRow::RepairJournal(&entry))
            .expect("derive repair journal removal key");
        if let Some(key) = key {
            preflight_existing_keys(&[key]);
            let removed = INDEX_REPAIR_JOURNAL
                .with_borrow_mut(|journal| journal.remove(sequence))
                .expect("remove prevalidated repair journal row");
            assert_eq!(removed, (sequence, entry));
            INDEX_PENDING_FLOOR.with_borrow_mut(|floor| floor.remove(&key));
        } else {
            let removed = INDEX_REPAIR_JOURNAL
                .with_borrow_mut(|journal| journal.remove(sequence))
                .expect("remove prevalidated untracked repair journal row");
            assert_eq!(removed, (sequence, entry));
        }
    }

    pub(crate) fn derived_index_outbox_remove(&self, sequence: u64) {
        let Some(entry) = DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.get(sequence)) else {
            return;
        };
        let key = tracked_floor_key(sequence, DurableSourceRow::DerivedIndexOutbox(&entry))
            .expect("derive derived-index outbox removal key");
        if let Some(key) = key {
            preflight_existing_keys(&[key]);
            let removed = DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.remove(sequence));
            assert_eq!(removed, (sequence, entry));
            INDEX_PENDING_FLOOR.with_borrow_mut(|floor| floor.remove(&key));
        } else {
            let removed = DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.remove(sequence));
            assert_eq!(removed, (sequence, entry));
        }
    }

    pub(crate) fn derived_index_outbox_apply_vector_outcome(
        &self,
        submitted: &[(u64, DerivedIndexOutboxEntry)],
        outcome: VectorSyncBatchOutcome,
    ) -> Result<(), &'static str> {
        let transition = DERIVED_INDEX_OUTBOX
            .with_borrow(|outbox| outbox.plan_vector_outcome(submitted, outcome))?;
        let keys: Vec<_> = transition
            .removed
            .iter()
            .filter_map(|(sequence, entry)| {
                tracked_floor_key(*sequence, DurableSourceRow::DerivedIndexOutbox(entry))
                    .expect("derive vector outcome floor key")
            })
            .collect();
        preflight_existing_keys(&keys);

        DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| {
            outbox.apply_vector_transition(transition);
        });
        INDEX_PENDING_FLOOR.with_borrow_mut(|floor| {
            for key in keys {
                floor.remove(&key);
            }
        });
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn derived_index_outbox_clear(&self) {
        let outbox_sequences = DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.sequences());
        for sequence in outbox_sequences {
            self.derived_index_outbox_remove(sequence);
        }
    }

    #[cfg(test)]
    pub(crate) fn derived_index_owners_clear(&self) {
        self.derived_index_outbox_clear();
        let repair_sequences = INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.sequences());
        for sequence in repair_sequences {
            self.repair_journal_remove(sequence);
        }
        assert_eq!(INDEX_PENDING_FLOOR.with_borrow(|floor| floor.len()), 0);
    }

    #[cfg(test)]
    fn seed_outbox_sequence_overflow(&self, entry: DerivedIndexOutboxEntry) {
        assert!(
            tracked_floor_key(u64::MAX, DurableSourceRow::DerivedIndexOutbox(&entry),)
                .expect("classify overflow fixture")
                .is_none(),
            "overflow fixture must not participate in the floor"
        );
        DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.insert_test_entry(u64::MAX, entry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{IndexBuildSubject, IndexMaintenancePhase, PhysicalIndexId};
    use gleaph_graph_kernel::vector_index::{
        VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject, VectorSyncTerminalError,
    };
    use std::panic::catch_unwind;

    fn ordinary(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VertexProperty {
            physical_index_id: PhysicalIndexId::new(900_263).expect("test physical index id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            remove: false,
            property_id: 7,
            payload_bytes: vec![1, 2, 3],
            vertex_id,
        }
    }

    fn vector(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VectorEmbedding {
            op: VectorEmbeddingSyncOp {
                index_id: 1,
                embedding_name_id: 2,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id: 7,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::L2Squared,
                bytes: vec![0; 4],
                remove: false,
            },
        }
    }

    fn build_request() -> IndexBuildDmlRequest {
        IndexBuildDmlRequest {
            physical_index_id: PhysicalIndexId::new(900_264).expect("test physical index id"),
            catalog_epoch: 1,
            shard_sequence: 1,
            subject: IndexBuildSubject::Vertex {
                shard_id: 0,
                vertex_id: 1,
            },
            removals: Vec::new(),
            insertions: vec![vec![1]],
        }
    }

    fn floor_len() -> u64 {
        INDEX_PENDING_FLOOR.with_borrow(|floor| floor.len())
    }

    fn outbox_rows() -> Vec<(u64, DerivedIndexOutboxEntry)> {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| {
            outbox
                .sequences()
                .into_iter()
                .map(|sequence| {
                    let entry = outbox.get(sequence).expect("snapshot outbox row");
                    (sequence, entry)
                })
                .collect()
        })
    }

    fn repair_rows() -> Vec<(u64, RepairJournalEntry)> {
        INDEX_REPAIR_JOURNAL.with_borrow(|journal| {
            journal
                .sequences()
                .into_iter()
                .map(|sequence| {
                    let entry = journal.get(sequence).expect("snapshot repair row");
                    (sequence, entry)
                })
                .collect()
        })
    }

    fn floor_keys(
        outbox_rows: &[(u64, DerivedIndexOutboxEntry)],
        repair_rows: &[(u64, RepairJournalEntry)],
    ) -> Vec<IndexPendingFloorKey> {
        let mut keys: Vec<_> = outbox_rows
            .iter()
            .filter_map(|(sequence, entry)| {
                tracked_floor_key(*sequence, DurableSourceRow::DerivedIndexOutbox(entry))
                    .expect("derive outbox floor key")
            })
            .chain(repair_rows.iter().filter_map(|(sequence, entry)| {
                tracked_floor_key(*sequence, DurableSourceRow::RepairJournal(entry))
                    .expect("derive repair floor key")
            }))
            .collect();
        keys.sort_unstable();
        keys
    }

    fn assert_exact_floor_keys(expected: &[IndexPendingFloorKey]) {
        INDEX_PENDING_FLOOR.with_borrow(|floor| {
            assert_eq!(
                floor.len(),
                u64::try_from(expected.len()).expect("floor snapshot length")
            );
            assert!(
                expected.iter().all(|key| floor.contains(key)),
                "floor identities differ from source-row identities"
            );
        });
    }

    #[test]
    fn co_updates_outbox_and_repair_without_zero_or_build_rows() {
        let store = GraphStore::new();
        store.derived_index_owners_clear();

        store.derived_index_outbox_append(9, [ordinary(1)]);
        store.derived_index_outbox_append(0, [ordinary(2)]);
        store.derived_index_build_outbox_append(1, [build_request()]);
        store.repair_journal_append(7, [ordinary(3)]);
        store.repair_journal_append(0, [ordinary(4)]);

        assert_eq!(store.derived_index_outbox_len(), 3);
        assert_eq!(store.repair_journal_len(), 2);
        assert_eq!(floor_len(), 2);
        assert_eq!(store.index_pending_min_mutation_id(), Some(7));

        store.repair_journal_remove(0);
        assert_eq!(store.index_pending_min_mutation_id(), Some(9));
        store.derived_index_outbox_remove(0);
        assert_eq!(store.index_pending_min_mutation_id(), None);
        assert_eq!(floor_len(), 0);
        assert_eq!(store.derived_index_outbox_len(), 2);
        assert_eq!(store.repair_journal_len(), 1);

        store.derived_index_owners_clear();
    }

    #[test]
    fn quarantine_and_partial_ack_preserve_exact_multiplicity() {
        let store = GraphStore::new();
        store.derived_index_owners_clear();

        store.derived_index_outbox_append(7, [vector(1), vector(2)]);
        store.derived_index_outbox_append(9, [ordinary(3)]);
        store.repair_journal_append(7, [ordinary(4)]);
        assert_eq!(floor_len(), 4);
        assert_eq!(store.index_pending_min_mutation_id(), Some(7));

        let submitted = store.derived_index_outbox_peek(2);
        store
            .derived_index_outbox_apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 1,
                    failed_index: 1,
                    error: VectorSyncTerminalError::SubjectTablePressure,
                },
            )
            .expect("valid partial acknowledgement and quarantine");
        assert_eq!(floor_len(), 3);
        assert_eq!(store.index_pending_min_mutation_id(), Some(7));

        store.repair_journal_remove(0);
        assert_eq!(floor_len(), 2);
        assert_eq!(store.index_pending_min_mutation_id(), Some(7));
        store.derived_index_outbox_remove(1);
        assert_eq!(floor_len(), 1);
        assert_eq!(store.index_pending_min_mutation_id(), Some(9));
        store.derived_index_outbox_remove(2);
        assert_eq!(store.index_pending_min_mutation_id(), None);

        store.derived_index_owners_clear();
    }

    #[test]
    fn prevalidation_failure_leaves_owners_and_floor_unchanged() {
        let store = GraphStore::new();
        store.derived_index_owners_clear();
        store.derived_index_outbox_append(5, [ordinary(1)]);
        store.repair_journal_append(7, [ordinary(2)]);
        store.seed_outbox_sequence_overflow(DerivedIndexOutboxEntry {
            mutation_id: 1,
            op: DerivedIndexOutboxOp::IndexBuildDml {
                request: build_request(),
            },
        });
        let before_outbox = outbox_rows();
        let before_repair = repair_rows();
        let before_floor = floor_keys(&before_outbox, &before_repair);
        assert_exact_floor_keys(&before_floor);

        assert!(catch_unwind(|| store.derived_index_outbox_append(3, [ordinary(3)])).is_err());
        assert_eq!(outbox_rows(), before_outbox);
        assert_eq!(repair_rows(), before_repair);
        assert_exact_floor_keys(&before_floor);
        assert_eq!(store.index_pending_min_mutation_id(), Some(5));

        store.derived_index_owners_clear();
    }
}
