//! The two questions a row walk answers, as types.
//!
//! ADR 0100's Visit family kept conflating them: "which rows does this walk report?" was a `RowVisibility` argument,
//! so a caller could ask for one answer and compare against the other and the compiler had nothing to say. The
//! conflation surfaced three times - at a call site, at an observation helper, and at a helper shared by two tests
//! that disagreed - because a tombstone was represented as *an edge value* (the sentinel), which both answers can
//! carry.
//!
//! These types make the answer a property of what is returned:
//!
//! - [`LiveRow`] has no hole, so a read cannot see one;
//! - [`SlotRow`] states the hole as a variant, so a caller that needs holes must match on it;
//! - a helper's signature names the question, and mixing the two is a type error rather than a re-read of the test.

use super::BucketEntryPosition;
use crate::traits::CsrEdgeTombstone;

/// A row that a reader sees: it holds a live edge, and there is no value of this type that represents a hole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveRow<E> {
    /// The row's tombstone-inclusive position.
    pub(crate) slot: BucketEntryPosition,
    /// The row's edge.
    pub(crate) edge: E,
}

/// What a stored slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState<E> {
    /// A live edge.
    Live(E),
    /// A hole: a deleted or tombstoned cell.
    Hole,
}

/// A stored slot: every position in the walked range, whatever it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotRow<E> {
    /// The row's tombstone-inclusive position.
    pub(crate) slot: BucketEntryPosition,
    /// What the slot holds.
    pub(crate) state: SlotState<E>,
}

impl<E> SlotRow<E>
where
    E: CsrEdgeTombstone,
{
    /// The edge this slot reports in the flat form: the live edge, or the tombstone encoding.
    pub(crate) fn into_edge(self) -> E {
        match self.state {
            SlotState::Live(edge) => edge,
            SlotState::Hole => E::tombstone_edge(),
        }
    }
}

/// Splits a flat `(position, edge)` from the walker into the typed answer.
///
/// The walker still yields the flat pair - that is the form every one of its arms produces - and this is the single
/// place that decides whether the pair means "a row" or "a slot".
pub(crate) fn slot_row<E>(slot: BucketEntryPosition, edge: E) -> SlotRow<E>
where
    E: CsrEdgeTombstone,
{
    let state = if edge.is_deleted_slot() || edge.is_tombstone_edge() {
        SlotState::Hole
    } else {
        SlotState::Live(edge)
    };
    SlotRow { slot, state }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::graph::test_support::TestEdge;

    /// A live edge becomes a live row, and the tombstone encoding becomes a hole - the one place the two forms meet.
    #[test]
    fn a_slot_row_states_what_the_slot_holds() {
        let live = slot_row(BucketEntryPosition::new(3), TestEdge { target: 7 });
        assert_eq!(live.slot.raw(), 3);
        assert!(matches!(
            live.state,
            SlotState::Live(TestEdge { target: 7 })
        ));

        let hole = slot_row(
            BucketEntryPosition::new(4),
            <TestEdge as CsrEdgeTombstone>::tombstone_edge(),
        );
        assert!(matches!(hole.state, SlotState::Hole));
        assert!(
            hole.into_edge().is_tombstone_edge(),
            "the flat form still says what it holds"
        );
    }
}
