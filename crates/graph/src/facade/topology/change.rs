//! Changes to the store that the rest of the shard follows: removing an edge half, and the slot
//! moves and purge steps the store reports.
//!
//! A removal returns its slot moves as data ([`SlotMoves`]), for the caller to apply to the
//! sidecars and indexes before anything reads them; lara-graph moves positions only in `compact`,
//! which returns them the same way (ADR 0136, Decisions 4 and 11). ic-stable-lara's deferred
//! maintenance also moves slots and purges edges on its own, while it drains; [`run_maintenance`]
//! reports those as they happen to a closure the caller passes, because the caller's handler reads
//! the store at that moment (a moved edge's inline values at its new slot). The drain goes with
//! ic-stable-lara.

use core::cell::RefCell;

use gleaph_graph_kernel::entry::{Edge, EdgeOrientation, TaggedEdgeLabelId, VertexId};
use ic_stable_lara::labeled::{DeleteEdgeObserver, EdgeSlotMoveObserver};

use super::{TopologyError, with_graph, with_graph_mut};

/// An edge half a vertex purge removed, as the store reports it.
pub(crate) use ic_stable_lara::labeled::DeletedEdge as RemovedHalf;
/// One edge's slot change within a bucket.
pub(crate) use ic_stable_lara::labeled::EdgeSlotMove;
/// How much deferred maintenance one drain may do, and what it did.
pub(crate) use ic_stable_lara::{
    MaintenanceBudget, labeled::LabeledBidirectionalMaintenanceReport as MaintenanceReport,
};

/// The slot moves one removal caused in one bucket side of one vertex.
#[must_use = "a removal's slot moves must be applied to the sidecars and indexes"]
#[derive(Debug)]
pub(crate) struct SlotMoves {
    side: EdgeOrientation,
    vertex_id: VertexId,
    moves: Vec<EdgeSlotMove>,
}

impl SlotMoves {
    /// The side and vertex of the moved slots, and the moves in the order they happened.
    pub(crate) fn into_parts(self) -> (EdgeOrientation, VertexId, Vec<EdgeSlotMove>) {
        (self.side, self.vertex_id, self.moves)
    }
}

/// Removes the edge half at `slot` of `vertex_id`'s `label` bucket on `side`, and returns the
/// slot moves it caused; `None` when the slot holds no edge.
pub(crate) fn remove_half(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    slot: u32,
) -> Result<Option<SlotMoves>, TopologyError> {
    let removal = with_graph_mut(|graph| match side {
        EdgeOrientation::Forward => {
            graph.remove_forward_edge_at_slot_with_move(vertex_id, label, slot)
        }
        EdgeOrientation::Reverse => {
            graph.remove_reverse_edge_at_slot_with_move(vertex_id, label, slot)
        }
    })?;
    Ok(removal.map(|removal| SlotMoves {
        side,
        vertex_id,
        moves: removal.moves,
    }))
}

/// A change deferred maintenance made, reported while it drains.
#[derive(Debug)]
pub(crate) enum TopologyChange {
    /// An edge's slot moved in `vertex_id`'s bucket on `side`.
    SlotMoved {
        side: EdgeOrientation,
        vertex_id: VertexId,
        moved: EdgeSlotMove,
    },
    /// A vertex purge removed an edge half, with its counterpart when the store found it.
    HalfRemoved {
        removed: RemovedHalf<Edge>,
        counterpart: Option<RemovedHalf<Edge>>,
    },
    /// A vertex purge finished.
    PurgeCompleted(VertexId),
}

/// Drains deferred maintenance within `budget`, calling `on_change` with each change as it
/// happens, inside the store's borrow: `on_change` may read the store but not write it.
pub(crate) fn run_maintenance(
    budget: MaintenanceBudget,
    on_change: impl FnMut(TopologyChange),
) -> Result<MaintenanceReport, TopologyError> {
    let on_change = RefCell::new(on_change);
    let mut moves = Report(&on_change);
    let mut removals = Report(&on_change);
    with_graph(|graph| graph.maintenance_with_observers(budget, &mut moves, &mut removals))
}

/// The store's observers, forwarding each notification to one closure. The store calls one
/// observer at a time, so the closure is never borrowed twice.
struct Report<'a, F>(&'a RefCell<F>);

impl<F: FnMut(TopologyChange)> EdgeSlotMoveObserver for Report<'_, F> {
    fn edge_slot_moved(&mut self, side: EdgeOrientation, vertex_id: VertexId, moved: EdgeSlotMove) {
        (self.0.borrow_mut())(TopologyChange::SlotMoved {
            side,
            vertex_id,
            moved,
        });
    }
}

impl<F: FnMut(TopologyChange)> DeleteEdgeObserver<Edge> for Report<'_, F> {
    fn on_delete_edge(
        &mut self,
        removed: RemovedHalf<Edge>,
        counterpart: Option<RemovedHalf<Edge>>,
    ) {
        (self.0.borrow_mut())(TopologyChange::HalfRemoved {
            removed,
            counterpart,
        });
    }

    fn on_vertex_purge_completed(&mut self, vertex_id: VertexId) {
        (self.0.borrow_mut())(TopologyChange::PurgeCompleted(vertex_id));
    }
}
