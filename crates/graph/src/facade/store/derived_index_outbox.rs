//! GraphStore access to the durable derived-index outbox (0088).

use super::super::stable::DERIVED_INDEX_OUTBOX;
use super::super::stable::derived_index_outbox::DerivedIndexOutboxEntry;
use super::super::stable::repair_journal::RepairPostingOp;
use super::GraphStore;

impl GraphStore {
    pub(crate) fn derived_index_outbox_append(
        &self,
        mutation_id: u64,
        ops: impl IntoIterator<Item = RepairPostingOp>,
    ) {
        DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.append_all(mutation_id, ops));
    }

    pub(crate) fn derived_index_outbox_is_empty(&self) -> bool {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.is_empty())
    }

    pub(crate) fn derived_index_outbox_len(&self) -> u64 {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.len())
    }

    pub(crate) fn derived_index_outbox_peek(
        &self,
        limit: usize,
    ) -> Vec<(u64, DerivedIndexOutboxEntry)> {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.peek(limit))
    }

    pub(crate) fn derived_index_outbox_remove(&self, seq: u64) {
        DERIVED_INDEX_OUTBOX.with_borrow_mut(|outbox| outbox.remove(seq));
    }
}
