//! GraphStore access to the durable index repair journal (ADR 0023 D5).

use super::super::stable::INDEX_REPAIR_JOURNAL;
use super::super::stable::repair_journal::RepairPostingOp;
use super::GraphStore;

impl GraphStore {
    /// Persists failed-flush postings to stable memory so the store-ahead delta
    /// survives upgrade / trap until the maintenance driver re-applies it.
    pub(crate) fn repair_journal_append(&self, ops: impl IntoIterator<Item = RepairPostingOp>) {
        INDEX_REPAIR_JOURNAL.with_borrow_mut(|journal| journal.append_all(ops));
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
