//! Label stats delta log and graph mutation journal (ADR 0015).

use super::GraphStore;
use crate::facade::stable::label_stats_delta::GraphMutationJournalEntry;
use crate::facade::stable::{GRAPH_MUTATION_JOURNAL, LABEL_STATS_DELTA_LOG, LABEL_STATS_DELTA_SEQ};
use gleaph_graph_kernel::plan_exec::{
    GraphMutationJournalEntryWire, LabelStatsDelta, LabelStatsDeltaEventWire, MutationId,
    ShardEventSeq,
};

impl GraphStore {
    pub(crate) fn mutation_journal_entry(
        &self,
        mutation_id: MutationId,
    ) -> Option<GraphMutationJournalEntry> {
        GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.get(mutation_id))
    }

    pub fn get_mutation_journal_entry(
        &self,
        mutation_id: MutationId,
    ) -> Option<GraphMutationJournalEntryWire> {
        self.mutation_journal_entry(mutation_id)
            .map(|entry| entry.wire())
    }

    pub(crate) fn commit_record_incomplete_mutation_journal(
        &self,
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(GraphMutationJournalEntry::incomplete(
                mutation_id,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
            ));
        });
    }

    pub(crate) fn commit_record_completed_mutation_journal(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(GraphMutationJournalEntry::completed(
                mutation_id,
                row_count,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
            ));
        });
    }

    pub(crate) fn commit_append_label_stats_delta(
        &self,
        mutation_id: MutationId,
        label_stats_delta: LabelStatsDelta,
    ) -> Result<LabelStatsDeltaEventWire, String> {
        let shard_event_seq = LABEL_STATS_DELTA_SEQ.with_borrow_mut(|seq| {
            let current = *seq.get();
            let next = current
                .checked_add(1)
                .ok_or_else(|| "label stats delta sequence exhausted".to_string())?;
            if next == 0 {
                return Err("label stats delta sequence exhausted".to_string());
            }
            seq.set(next);
            Ok::<ShardEventSeq, String>(next)
        })?;
        let event = LabelStatsDeltaEventWire {
            mutation_id,
            shard_event_seq,
            label_stats_delta,
        };
        LABEL_STATS_DELTA_LOG.with_borrow_mut(|log| {
            log.insert(event.clone());
        });
        Ok(event)
    }

    pub fn ack_label_stats_deltas_through(&self, through_seq: ShardEventSeq) {
        LABEL_STATS_DELTA_LOG.with_borrow_mut(|log| {
            log.remove_through(through_seq);
        });
    }

    pub fn pending_label_stats_deltas(
        &self,
        from_seq: ShardEventSeq,
        limit: u32,
    ) -> Vec<LabelStatsDeltaEventWire> {
        LABEL_STATS_DELTA_LOG.with_borrow(|log| log.list_from(from_seq, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::VertexLabelId;

    #[test]
    fn persists_lists_and_acks_label_stats_deltas() {
        let store = GraphStore::new();
        let event = store
            .commit_append_label_stats_delta(
                7,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(3), 2)],
                    edge: vec![],
                },
            )
            .expect("persist delta");

        assert_eq!(event.mutation_id, 7);
        assert!(event.shard_event_seq > 0);
        assert_eq!(
            store.pending_label_stats_deltas(event.shard_event_seq, 10),
            vec![event.clone()]
        );

        store.ack_label_stats_deltas_through(event.shard_event_seq);
        assert!(
            !store
                .pending_label_stats_deltas(event.shard_event_seq, 10)
                .iter()
                .any(|pending| pending.shard_event_seq == event.shard_event_seq)
        );
    }

    #[test]
    fn ack_label_stats_deltas_through_removes_prefix() {
        let store = GraphStore::new();
        let first = store
            .commit_append_label_stats_delta(
                1,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 1)],
                    edge: vec![],
                },
            )
            .expect("first delta");
        let second = store
            .commit_append_label_stats_delta(
                1,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 1)],
                    edge: vec![],
                },
            )
            .expect("second delta");

        store.ack_label_stats_deltas_through(first.shard_event_seq);
        assert_eq!(
            store.pending_label_stats_deltas(0, 10),
            vec![second.clone()]
        );
    }

    #[test]
    fn mutation_journal_roundtrips_seq_range() {
        let store = GraphStore::new();
        let event = store
            .commit_append_label_stats_delta(
                11,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(4), 1)],
                    edge: vec![],
                },
            )
            .expect("persist delta");
        store.commit_record_completed_mutation_journal(
            11,
            5,
            Some(event.shard_event_seq),
            Some(event.shard_event_seq),
        );

        let journal = store.mutation_journal_entry(11).expect("journal entry");
        assert!(journal.is_completed());
        assert_eq!(journal.row_count, 5);
        assert_eq!(journal.emitted_delta_first_seq, Some(event.shard_event_seq));
        assert_eq!(journal.emitted_delta_last_seq, Some(event.shard_event_seq));

        let wire = store.get_mutation_journal_entry(11).expect("journal wire");
        assert_eq!(wire.emitted_delta_first_seq, Some(event.shard_event_seq));
        assert_eq!(wire.emitted_delta_last_seq, Some(event.shard_event_seq));
    }
}
