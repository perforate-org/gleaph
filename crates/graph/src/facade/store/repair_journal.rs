//! GraphStore access to the durable index repair journal (ADR 0023 D5).

use super::super::stable::INDEX_REPAIR_JOURNAL;
use super::super::stable::repair_journal::RepairPostingOp;
use super::GraphStore;
use gleaph_graph_kernel::federation::IndexSyncStatus;

impl GraphStore {
    /// Snapshot of durable derived-index work awaiting delivery to graph-index: the
    /// first-delivery outbox plus the failed-flush repair journal. `converged` is true
    /// only when both queues are empty. Seeding/backfill orchestration polls this to
    /// know when index-backed `MATCH` anchors reflect canonical vertex state.
    pub(crate) fn index_sync_status(&self) -> IndexSyncStatus {
        IndexSyncStatus::new(self.derived_index_outbox_len(), self.repair_journal_len())
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
}
