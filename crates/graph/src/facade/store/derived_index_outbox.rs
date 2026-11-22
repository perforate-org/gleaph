//! GraphStore access to the durable derived-index outbox (0088).

use super::super::stable::DERIVED_INDEX_OUTBOX;
use super::super::stable::derived_index_outbox::{DerivedIndexOutboxEntry, TextDocumentOp};
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

    /// The oldest pending rows of the head-ordered class; text rows are skipped (see
    /// [`crate::facade::stable::derived_index_outbox`]).
    pub(crate) fn derived_index_outbox_peek_ordered(
        &self,
        limit: usize,
    ) -> Vec<(u64, DerivedIndexOutboxEntry)> {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.peek_ordered(limit))
    }

    pub(crate) fn derived_index_outbox_ordered_pending_is_empty(&self) -> bool {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.ordered_pending_is_empty())
    }

    pub(crate) fn derived_index_outbox_pending_text(&self) -> Vec<(u64, TextDocumentOp)> {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.pending_text())
    }

    pub(crate) fn derived_index_outbox_has_deliverable(
        &self,
        text_deliverable: impl Fn((u16, u32)) -> bool,
    ) -> bool {
        DERIVED_INDEX_OUTBOX.with_borrow(|outbox| outbox.has_deliverable(text_deliverable))
    }
}
