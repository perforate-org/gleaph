//! GraphStore access to the durable derived-index outbox (0088).

use super::super::stable::DERIVED_INDEX_OUTBOX;
use super::super::stable::derived_index_outbox::DerivedIndexOutboxEntry;
use super::GraphStore;

impl GraphStore {
    pub(crate) fn derived_index_outbox_is_empty(&self) -> bool {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.is_empty())
    }

    pub(crate) fn derived_index_outbox_pending_is_empty(&self) -> bool {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.pending_is_empty())
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
}
