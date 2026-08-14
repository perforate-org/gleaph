//! Durable FIFO outbox for derived property, label, and vector-index work.
//!
//! The outbox is the persistence boundary for successful Graph mutations.  The mutation path may
//! collect derived-index deltas in heap memory while it runs, but once the mutation commits those
//! deltas are appended here before the message returns.  Maintenance removes only acknowledged
//! prefixes, so an upgrade or retry cannot discard unacknowledged work.

use super::repair_journal::RepairPostingOp;
use candid::{Decode, Encode};
use gleaph_graph_kernel::index::IndexBuildDmlRequest;
#[cfg(test)]
use gleaph_graph_kernel::index::{IndexMaintenancePhase, PhysicalIndexId};
use gleaph_graph_kernel::vector_index::{VectorSyncBatchOutcome, VectorSyncTerminalError};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

/// One durable derived-index operation awaiting its first delivery attempt.
///
/// Build DML is intentionally a distinct outbox-only variant. Keeping it out of
/// [`RepairPostingOp`] makes it impossible for the failure-only MemoryId 41 journal to persist
/// or replay a Building envelope; exact build retries have one source of truth in MemoryId 46.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum DerivedIndexOutboxOp {
    Ordinary(RepairPostingOp),
    IndexBuildDml { request: IndexBuildDmlRequest },
}

/// One durable derived-index operation awaiting its first delivery attempt.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct DerivedIndexOutboxEntry {
    pub mutation_id: u64,
    pub op: DerivedIndexOutboxOp,
}

impl Storable for DerivedIndexOutboxEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode legacy DerivedIndexOutboxEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode legacy DerivedIndexOutboxEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode legacy DerivedIndexOutboxEntry")
    }
}

/// Bounded reason why one outbox row cannot ever be admitted by its target.
///
/// This is deliberately payload-free: MemoryId 46 never persists an unbounded target diagnostic.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum DerivedIndexQuarantineReason {
    IndexDefinitionTablePressure,
    SubjectTablePressure,
}

/// Durable delivery state for one MemoryId 46 row.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
enum DerivedIndexOutboxState {
    Pending(DerivedIndexOutboxEntry),
    Quarantined {
        entry: DerivedIndexOutboxEntry,
        reason: DerivedIndexQuarantineReason,
    },
}

impl Storable for DerivedIndexOutboxState {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode DerivedIndexOutboxState"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode DerivedIndexOutboxState")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).unwrap_or_else(|_| {
            // MemoryId 46 originally stored this exact bare entry. It remains readable as pending;
            // there is no speculative fallback for any earlier value shape.
            Self::Pending(
                Decode!(bytes.as_ref(), DerivedIndexOutboxEntry)
                    .expect("decode DerivedIndexOutboxState or legacy DerivedIndexOutboxEntry"),
            )
        })
    }
}

/// Stable FIFO keyed by a monotonic sequence.  The sequence is the durable cursor identity;
/// callers acknowledge only entries they know the target canister accepted.
pub struct DerivedIndexOutbox<M: Memory> {
    map: StableBTreeMap<u64, DerivedIndexOutboxState, M>,
}

impl<M: Memory> DerivedIndexOutbox<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    fn next_seq(&self) -> u64 {
        self.map.last_key_value().map_or(0, |(seq, _)| {
            seq.checked_add(1)
                .expect("derived-index outbox sequence overflow")
        })
    }

    /// Appends one DML's derived-index operations in their existing order.
    pub fn append_all(&mut self, mutation_id: u64, ops: impl IntoIterator<Item = RepairPostingOp>) {
        let mut next = self.next_seq();
        for op in ops {
            self.map.insert(
                next,
                DerivedIndexOutboxState::Pending(DerivedIndexOutboxEntry {
                    mutation_id,
                    op: DerivedIndexOutboxOp::Ordinary(op),
                }),
            );
            next = next
                .checked_add(1)
                .expect("derived-index outbox sequence overflow");
        }
    }

    /// Appends exact Building/Sealing envelopes without making them representable in the
    /// failure-only repair journal.
    pub fn append_build_dml(
        &mut self,
        mutation_id: u64,
        requests: impl IntoIterator<Item = IndexBuildDmlRequest>,
    ) {
        let mut next = self.next_seq();
        for request in requests {
            self.map.insert(
                next,
                DerivedIndexOutboxState::Pending(DerivedIndexOutboxEntry {
                    mutation_id,
                    op: DerivedIndexOutboxOp::IndexBuildDml { request },
                }),
            );
            next = next
                .checked_add(1)
                .expect("derived-index outbox sequence overflow");
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn pending_is_empty(&self) -> bool {
        self.map
            .iter()
            .all(|row| matches!(row.value(), DerivedIndexOutboxState::Quarantined { .. }))
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    /// Returns the oldest durable prefix without advancing the cursor.
    pub fn peek(&self, limit: usize) -> Vec<(u64, DerivedIndexOutboxEntry)> {
        self.map
            .iter()
            .filter_map(|row| match row.value() {
                DerivedIndexOutboxState::Pending(entry) => Some((*row.key(), entry)),
                DerivedIndexOutboxState::Quarantined { .. } => None,
            })
            .take(limit)
            .collect()
    }

    /// Applies one prevalidated vector delivery result as a single owner-level state transition.
    /// Every durable identity is checked before the first write.
    pub fn apply_vector_outcome(
        &mut self,
        submitted: &[(u64, DerivedIndexOutboxEntry)],
        outcome: VectorSyncBatchOutcome,
    ) -> Result<(), &'static str> {
        outcome.validate(submitted.len())?;
        if submitted.is_empty()
            || submitted.iter().any(|(_, entry)| {
                !matches!(
                    &entry.op,
                    DerivedIndexOutboxOp::Ordinary(RepairPostingOp::VectorEmbedding { .. })
                )
            })
        {
            return Err("vector outcome requires a nonempty vector outbox prefix");
        }
        if self.peek(submitted.len()) != submitted {
            return Err("submitted vector entries are not the current pending outbox prefix");
        }
        let (applied, failed) = match outcome {
            VectorSyncBatchOutcome::Progress { applied } => (applied as usize, None),
            VectorSyncBatchOutcome::Terminal {
                applied,
                failed_index,
                error,
            } => (applied as usize, Some((failed_index as usize, error))),
        };

        for (seq, _) in &submitted[..applied] {
            self.map.remove(seq);
        }
        if let Some((failed_index, error)) = failed {
            let (seq, entry) = &submitted[failed_index];
            let reason = match error {
                VectorSyncTerminalError::IndexDefinitionTablePressure => {
                    DerivedIndexQuarantineReason::IndexDefinitionTablePressure
                }
                VectorSyncTerminalError::SubjectTablePressure => {
                    DerivedIndexQuarantineReason::SubjectTablePressure
                }
            };
            self.map.insert(
                *seq,
                DerivedIndexOutboxState::Quarantined {
                    entry: entry.clone(),
                    reason,
                },
            );
        }
        Ok(())
    }

    /// Acknowledges one entry.  The maintenance owner must call this only for an accepted entry.
    pub fn remove(&mut self, seq: u64) {
        self.map.remove(&seq);
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        let sequences: Vec<_> = self.map.iter().map(|row| *row.key()).collect();
        for seq in sequences {
            self.map.remove(&seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn entry(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VertexProperty {
            physical_index_id: PhysicalIndexId::new(900_102).expect("test physical id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            remove: false,
            property_id: 7,
            payload_bytes: vec![1, 2, 3],
            vertex_id,
        }
    }

    fn vector_entry(vertex_id: u32) -> RepairPostingOp {
        use gleaph_graph_kernel::federation::ShardId;
        use gleaph_graph_kernel::vector_index::{
            VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
        };
        RepairPostingOp::VectorEmbedding {
            op: VectorEmbeddingSyncOp {
                index_id: 1,
                embedding_name_id: 2,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id: 11,
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::L2Squared,
                bytes: vec![0; 4],
                remove: false,
            },
        }
    }

    #[test]
    fn appends_in_order_and_acknowledges_only_removed_entries() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [entry(3), entry(4)]);

        assert_eq!(outbox.len(), 2);
        let first = outbox.peek(1);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, 0);
        assert_eq!(first[0].1.mutation_id, 11);
        assert_eq!(first[0].1.op, DerivedIndexOutboxOp::Ordinary(entry(3)));

        outbox.remove(first[0].0);
        let remaining = outbox.peek(10);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].0, 1);
        assert_eq!(remaining[0].1.op, DerivedIndexOutboxOp::Ordinary(entry(4)));
    }

    #[test]
    fn sequence_continues_after_a_prefix_is_removed() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(1, [entry(1), entry(2)]);
        outbox.remove(0);
        outbox.append_all(2, [entry(3)]);

        let entries = outbox.peek(10);
        assert_eq!(
            entries.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(entries[1].1.mutation_id, 2);
    }

    #[test]
    fn legacy_bare_entry_reopens_as_pending() {
        let legacy = DerivedIndexOutboxEntry {
            mutation_id: 11,
            op: DerivedIndexOutboxOp::Ordinary(entry(3)),
        };
        let memory = VectorMemory::default();
        let mut old_map = StableBTreeMap::<u64, DerivedIndexOutboxEntry, _>::init(memory.clone());
        old_map.insert(0, legacy.clone());
        drop(old_map);

        let reopened = DerivedIndexOutbox::init(memory);
        assert_eq!(reopened.peek(10), vec![(0, legacy)]);
    }

    #[test]
    fn terminal_outcome_removes_prefix_quarantines_failure_and_leaves_suffix_pending() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(
            11,
            [
                vector_entry(1),
                vector_entry(2),
                vector_entry(3),
                vector_entry(4),
            ],
        );
        let submitted = outbox.peek(10);

        outbox
            .apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 2,
                    failed_index: 2,
                    error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::IndexDefinitionTablePressure,
                },
            )
            .expect("valid terminal transition");

        assert_eq!(outbox.peek(10), vec![(3, submitted[3].1.clone())]);
        assert_eq!(outbox.len(), 2, "quarantine remains durable and observable");
        assert!(matches!(
            outbox.map.get(&2),
            Some(DerivedIndexOutboxState::Quarantined {
                reason: DerivedIndexQuarantineReason::IndexDefinitionTablePressure,
                ..
            })
        ));
    }

    #[test]
    fn terminal_outcome_identity_mismatch_changes_nothing() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [vector_entry(1), vector_entry(2), vector_entry(3)]);
        let mut submitted = outbox.peek(10);
        submitted[1].1.mutation_id = 99;
        let before: Vec<_> = outbox
            .map
            .iter()
            .map(|row| (*row.key(), row.value()))
            .collect();

        assert_eq!(
            outbox.apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 1,
                    failed_index: 1,
                    error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::IndexDefinitionTablePressure,
                },
            ),
            Err("submitted vector entries are not the current pending outbox prefix")
        );
        assert_eq!(
            outbox
                .map
                .iter()
                .map(|row| (*row.key(), row.value()))
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn terminal_at_zero_quarantines_head_without_removing_suffix() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [vector_entry(1), vector_entry(2)]);
        let submitted = outbox.peek(10);

        outbox
            .apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 0,
                    failed_index: 0,
                    error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::IndexDefinitionTablePressure,
                },
            )
            .expect("valid terminal transition at zero");

        assert_eq!(outbox.peek(10), vec![(1, submitted[1].1.clone())]);
        assert_eq!(outbox.len(), 2);
        assert_eq!(
            outbox.peek(10),
            vec![(1, submitted[1].1.clone())],
            "a second maintenance read must not retry the quarantined row"
        );
    }

    #[test]
    fn subject_pressure_terminal_quarantines_exact_failed_row() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [vector_entry(1), vector_entry(2)]);
        let submitted = outbox.peek(10);

        outbox
            .apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 0,
                    failed_index: 0,
                    error: VectorSyncTerminalError::SubjectTablePressure,
                },
            )
            .expect("subject pressure terminal transition");

        assert_eq!(outbox.peek(10), vec![(1, submitted[1].1.clone())]);
        assert!(matches!(
            outbox.map.get(&0),
            Some(DerivedIndexOutboxState::Quarantined {
                reason: DerivedIndexQuarantineReason::SubjectTablePressure,
                ..
            })
        ));
    }

    #[test]
    fn malformed_outcome_changes_nothing() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [vector_entry(1), vector_entry(2)]);
        let submitted = outbox.peek(10);
        let before: Vec<_> = outbox
            .map
            .iter()
            .map(|row| (*row.key(), row.value()))
            .collect();

        assert!(
            outbox
                .apply_vector_outcome(
                    &submitted,
                    VectorSyncBatchOutcome::Terminal {
                        applied: 1,
                        failed_index: 0,
                        error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::IndexDefinitionTablePressure,
                    },
                )
                .is_err()
        );
        assert_eq!(
            outbox
                .map
                .iter()
                .map(|row| (*row.key(), row.value()))
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn noncontiguous_submitted_rows_change_nothing() {
        let mut outbox = DerivedIndexOutbox::init(VectorMemory::default());
        outbox.append_all(11, [vector_entry(1), vector_entry(2), vector_entry(3)]);
        let all = outbox.peek(10);
        let submitted = vec![all[0].clone(), all[2].clone()];
        let before: Vec<_> = outbox
            .map
            .iter()
            .map(|row| (*row.key(), row.value()))
            .collect();

        assert_eq!(
            outbox.apply_vector_outcome(
                &submitted,
                VectorSyncBatchOutcome::Terminal {
                    applied: 1,
                    failed_index: 1,
                        error: gleaph_graph_kernel::vector_index::VectorSyncTerminalError::IndexDefinitionTablePressure,
                },
            ),
            Err("submitted vector entries are not the current pending outbox prefix")
        );
        assert_eq!(
            outbox
                .map
                .iter()
                .map(|row| (*row.key(), row.value()))
                .collect::<Vec<_>>(),
            before
        );
    }
}
