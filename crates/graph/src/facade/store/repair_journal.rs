//! GraphStore access to the durable index repair journal (ADR 0023 D5).

use super::super::stable::INDEX_REPAIR_JOURNAL;
use super::super::stable::repair_journal::RepairPostingOp;
use super::GraphStore;

impl GraphStore {
    /// Persists failed-flush postings to stable memory so the store-ahead delta
    /// survives upgrade / trap until the maintenance driver re-applies it. `mutation_id`
    /// links the batch to its originating federated mutation (`0` = untracked; ADR 0029
    /// Phase 2).
    pub(crate) fn repair_journal_append(
        &self,
        mutation_id: u64,
        ops: impl IntoIterator<Item = RepairPostingOp>,
    ) {
        INDEX_REPAIR_JOURNAL.with_borrow_mut(|journal| journal.append_all(mutation_id, ops));
    }

    /// Smallest tracked mutation id whose graph-index postings are not yet applied, or
    /// `None` when all tracked index work has drained (ADR 0029 Phase 2 watermark). A
    /// read for mutation `M` is index-satisfied on this shard iff this is `None` or
    /// `M < value`.
    pub(crate) fn index_pending_min_mutation_id(&self) -> Option<u64> {
        INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.min_tracked_mutation_id())
    }

    pub(crate) fn repair_journal_is_empty(&self) -> bool {
        INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.is_empty())
    }

    pub(crate) fn repair_journal_len(&self) -> u64 {
        INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.len())
    }

    pub(crate) fn repair_journal_peek(&self, limit: usize) -> Vec<(u64, RepairPostingOp)> {
        INDEX_REPAIR_JOURNAL.with_borrow(|journal| journal.peek(limit))
    }

    pub(crate) fn repair_journal_remove(&self, seq: u64) {
        INDEX_REPAIR_JOURNAL.with_borrow_mut(|journal| journal.remove(seq));
    }
}
