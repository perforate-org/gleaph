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
pub(crate) struct LiveRow<E, P = ()> {
    /// The row's tombstone-inclusive position.
    pub(crate) slot: BucketEntryPosition,
    /// The row's edge.
    pub(crate) edge: E,
    /// Whatever the walk carried alongside the row: inline property values, or nothing.
    pub(crate) payload: P,
}

/// What a stored slot holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotState<E, P = ()> {
    /// A live edge.
    Live { edge: E, payload: P },
    /// A hole: a deleted or tombstoned cell.
    Hole { payload: P },
}

/// A stored slot: every position in the walked range, whatever it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlotRow<E, P = ()> {
    /// The row's tombstone-inclusive position.
    pub(crate) slot: BucketEntryPosition,
    /// What the slot holds.
    pub(crate) state: SlotState<E, P>,
}

impl<E, P> SlotRow<E, P>
where
    E: CsrEdgeTombstone,
{
    /// The edge this slot reports in the flat form: the live edge, or the tombstone encoding.
    pub(crate) fn into_edge(self) -> E {
        match self.state {
            SlotState::Live { edge, .. } => edge,
            SlotState::Hole { .. } => E::tombstone_edge(),
        }
    }

    /// The payload, whichever state the slot is in.
    pub(crate) fn payload(self) -> P {
        match self.state {
            SlotState::Live { payload, .. } | SlotState::Hole { payload } => payload,
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
        SlotState::Hole { payload: () }
    } else {
        SlotState::Live { edge, payload: () }
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
            SlotState::Live {
                edge: TestEdge { target: 7 },
                ..
            }
        ));

        let hole = slot_row(
            BucketEntryPosition::new(4),
            <TestEdge as CsrEdgeTombstone>::tombstone_edge(),
        );
        assert!(matches!(hole.state, SlotState::Hole { .. }));
        assert!(
            hole.into_edge().is_tombstone_edge(),
            "the flat form still says what it holds"
        );
    }

    /// The typed entry is usable end to end: a reader gets rows whose payload is the row's own values, and the type
    /// says that a hole is not among them.
    #[test]
    fn a_live_read_yields_rows_with_their_own_values() {
        use crate::labeled::graph::test_support::{
            InlinePropertyTestEdge, inline_property_test_graph,
        };
        use crate::labeled::record::LabeledVertex;
        use crate::labeled::{bucket_label_key::BucketLabelKey, graph::EdgePlacementPolicy};
        use std::ops::ControlFlow;

        let graph = inline_property_test_graph();
        let vid = graph.push_vertex(LabeledVertex::default()).expect("push");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 2)
            .expect("declare the width");
        for slot in 0..8u32 {
            let value = u16::try_from(100 + slot).expect("fits").to_le_bytes();
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    InlinePropertyTestEdge::with_bytes(100 + slot, &value),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        graph
            .remove_edge_at_slot(vid, label, 3)
            .expect("remove")
            .expect("the row existed");

        let mut rows: Vec<LiveRow<InlinePropertyTestEdge, Vec<u8>>> = Vec::new();
        let flow = graph
            .visit_live_rows_with_values(
                vid,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                |row| {
                    rows.push(row);
                    ControlFlow::<()>::Continue(())
                },
            )
            .expect("live rows");
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(rows.len(), 7, "the hole is not a row");
        assert!(rows.iter().all(|row| row.slot.raw() != 3));
        for row in &rows {
            let expected = u16::try_from(100 + row.slot.raw())
                .expect("fits")
                .to_le_bytes();
            assert_eq!(row.payload, expected, "each row carries its own values");
        }
    }
}
