//! Label stats delta log and graph mutation journal (ADR 0015).

use super::GraphStore;
use crate::facade::stable::label_stats_delta::GraphMutationJournalEntry;
use crate::facade::stable::{GRAPH_MUTATION_JOURNAL, LABEL_STATS_DELTA_LOG, LABEL_STATS_DELTA_SEQ};
use gleaph_graph_kernel::federation::LocalVertexId;
use gleaph_graph_kernel::plan_exec::{
    GraphMutationJournalEntryWire, LabelStatsDelta, LabelStatsDeltaEventWire, MutationId,
    ShardEventSeq,
};
use std::cell::RefCell;

/// Retention window for completed/incomplete mutation journal entries (ADR 0027). A
/// `mutation_id` can only be replayed by the router while the router's client-key
/// idempotency record lives — at most `CLIENT_MUTATION_KEY_TTL_NS` (7 days, ADR 0025)
/// from creation, after which the same client key allocates a *fresh* id and the old id
/// is retired forever. This constant is a one-directional lower bound on that router TTL
/// (router TTL + margin), deliberately not an exact duplicate: it only needs to be
/// `>= router TTL` for dedup safety. Crates cannot share the constant (graph must not
/// depend on router), so the coupling is documented here and in ADR 0027.
const GRAPH_MUTATION_JOURNAL_RETENTION_NS: u64 = 9 * 24 * 60 * 60 * 1_000_000_000;

/// Entries examined per amortized GC step on the completed-journal write path
/// (ADR 0025 mechanism B, mirrored). Each completed mutation evicts up to this many
/// expired entries, keeping eviction in pace with the sole growth source (new mutations).
const MUTATION_JOURNAL_GC_BUDGET: usize = 2;

thread_local! {
    /// Ephemeral round-robin cursor for amortized journal GC (ADR 0027). Heap-only on
    /// purpose: resetting to the start on upgrade just restarts the lap, and the journal
    /// itself (region 39, Canonical) is fully stable.
    static MUTATION_JOURNAL_GC_CURSOR: RefCell<Option<MutationId>> = const { RefCell::new(None) };
}

fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

#[cfg(test)]
pub(crate) fn reset_mutation_journal_gc_cursor_for_test() {
    MUTATION_JOURNAL_GC_CURSOR.with_borrow_mut(|cursor| *cursor = None);
}

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
        self.commit_record_incomplete_mutation_journal_at(
            ic_time_ns(),
            mutation_id,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
        );
    }

    pub(crate) fn commit_record_incomplete_mutation_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(GraphMutationJournalEntry::incomplete(
                mutation_id,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
                now_ns,
            ));
        });
    }

    pub(crate) fn commit_record_completed_mutation_journal(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        self.commit_record_completed_mutation_journal_at(
            ic_time_ns(),
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
        );
    }

    pub(crate) fn commit_record_completed_mutation_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(GraphMutationJournalEntry::completed(
                mutation_id,
                row_count,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
                hot_forward_vertices,
                now_ns,
            ));
        });
        // Amortized retention sweep: the completed-journal write is the per-mutation
        // growth source, so each one funds one bounded eviction step (ADR 0027).
        self.gc_mutation_journal_at(now_ns);
    }

    /// One bounded, round-robin retention step over the mutation journal (ADR 0027).
    /// Advances a heap-only cursor; wraps to the start once the keyspace is exhausted.
    pub(crate) fn gc_mutation_journal_at(&self, now_ns: u64) {
        let start = MUTATION_JOURNAL_GC_CURSOR.with_borrow(|cursor| *cursor);
        let (scanned, _removed, last_key) = GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.evict_expired(
                start,
                MUTATION_JOURNAL_GC_BUDGET,
                now_ns,
                GRAPH_MUTATION_JOURNAL_RETENTION_NS,
            )
        });
        let next = if (scanned as usize) < MUTATION_JOURNAL_GC_BUDGET {
            None
        } else {
            last_key
        };
        MUTATION_JOURNAL_GC_CURSOR.with_borrow_mut(|cursor| *cursor = next);
    }

    /// Count of entries in the graph mutation journal (PocketIC E2E only).
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn e2e_mutation_journal_len(&self) -> u64 {
        GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.len())
    }

    /// Run a full retention sweep over the whole journal keyspace at the current IC time and return
    /// the remaining entry count (PocketIC E2E only). Drives the production [`evict_expired`] path
    /// with the real 9-day [`GRAPH_MUTATION_JOURNAL_RETENTION_NS`] window — unlike the amortized,
    /// budgeted [`gc_mutation_journal_at`] step it loops until the keyspace is exhausted, so a test
    /// can advance past the window and observe eviction deterministically.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn e2e_evict_mutation_journal(&self) -> u64 {
        let now = ic_time_ns();
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            let mut cursor = None;
            loop {
                let (scanned, _removed, last) = m.evict_expired(
                    cursor,
                    MUTATION_JOURNAL_GC_BUDGET,
                    now,
                    GRAPH_MUTATION_JOURNAL_RETENTION_NS,
                );
                if (scanned as usize) < MUTATION_JOURNAL_GC_BUDGET {
                    break;
                }
                cursor = last;
            }
            m.len()
        })
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

    const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

    #[test]
    fn completed_write_path_stamps_and_amortized_gc_evicts() {
        super::reset_mutation_journal_gc_cursor_for_test();
        let store = GraphStore::new();
        let id = 9_000_001u64;
        // Recorded "long ago" via the timestamped write path.
        store.commit_record_completed_mutation_journal_at(0, id, 3, None, None, Vec::new());
        let entry = store.mutation_journal_entry(id).expect("entry recorded");
        assert_eq!(entry.recorded_at_ns, Some(0));

        // Drive amortized GC forward at a time well past retention; the round-robin cursor
        // reaches the aged entry within a bounded number of budgeted steps and evicts it.
        let now = 100 * DAY_NS;
        for _ in 0..256 {
            if store.mutation_journal_entry(id).is_none() {
                break;
            }
            store.gc_mutation_journal_at(now);
        }
        assert!(
            store.mutation_journal_entry(id).is_none(),
            "aged entry should be evicted by amortized GC"
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
            vec![7, 42],
        );

        let journal = store.mutation_journal_entry(11).expect("journal entry");
        assert!(journal.is_completed());
        assert_eq!(journal.row_count, 5);
        assert_eq!(journal.emitted_delta_first_seq, Some(event.shard_event_seq));
        assert_eq!(journal.emitted_delta_last_seq, Some(event.shard_event_seq));
        assert_eq!(journal.hot_forward_vertices, vec![7, 42]);

        let wire = store.get_mutation_journal_entry(11).expect("journal wire");
        assert_eq!(wire.emitted_delta_first_seq, Some(event.shard_event_seq));
        assert_eq!(wire.emitted_delta_last_seq, Some(event.shard_event_seq));
        assert_eq!(wire.hot_forward_vertices, vec![7, 42]);
    }
}
