//! Changes to the store that the rest of the shard follows: removing an edge, rewriting its
//! value, deleting a vertex, and the slot moves compaction makes.
//!
//! A removal leaves a hole and moves nothing; positions move only when the work list compacts a
//! bucket, which hands the moves back as data ([`SlotMoves`], from [`super::work::step`]) for the
//! caller to apply to the sidecars and indexes (ADR 0136, Decisions 4 and 6). A vertex delete
//! starts here and finishes in the work list's detach steps.

use gleaph_graph_kernel::entry::{TaggedEdgeLabelId, VertexId};
use lara_graph::{Direction, Progress, Removal};

use super::{
    CanonicalEdgeOccurrence, TopologyError, edge_ref, node, occurrence_of, with_graph,
    with_graph_mut, work,
};

/// One canonical half's move within its bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EdgeSlotMove {
    /// The bucket's tagged label.
    pub label_id: TaggedEdgeLabelId,
    /// The position it left.
    pub old_slot_index: u32,
    /// The position it took.
    pub new_slot_index: u32,
}

/// The canonical halves one compaction step moved in one bucket of one vertex, from the lowest
/// old position. Moves of other halves are left out: nothing is keyed by them.
#[must_use = "slot moves must be applied to the sidecars and indexes"]
#[derive(Debug)]
pub(crate) struct SlotMoves {
    vertex_id: VertexId,
    moves: Vec<EdgeSlotMove>,
}

impl SlotMoves {
    pub(super) fn new(vertex_id: VertexId, moves: Vec<EdgeSlotMove>) -> Self {
        Self { vertex_id, moves }
    }

    /// The vertex whose bucket moved, and the moves in the order to apply them.
    pub(crate) fn into_parts(self) -> (VertexId, Vec<EdgeSlotMove>) {
        (self.vertex_id, self.moves)
    }
}

/// An edge a vertex delete removed, both halves: its canonical half and the value it carried.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RemovedEdge {
    pub canonical: CanonicalEdgeOccurrence,
    pub value: Vec<u8>,
}

impl RemovedEdge {
    pub(super) fn of(removed: &lara_graph::RemovedEdge) -> Self {
        Self {
            canonical: occurrence_of(removed.halves().canonical()),
            value: removed.value().to_vec(),
        }
    }
}

/// Removes the edge whose canonical half is `canonical`, both halves; `false` when no live edge
/// is there. A bucket the removal leaves with as many holes as live edges is listed for
/// compaction.
pub(crate) fn remove_edge(canonical: CanonicalEdgeOccurrence) -> Result<bool, TopologyError> {
    let half = edge_ref(canonical);
    with_graph_mut(|graph| {
        match graph
            .remove_edge(half)
            .map_err(|error| TopologyError::from_store(graph, error))?
        {
            Removal::Absent => Ok(false),
            Removal::Removed(removed) => {
                work::list_after_removal(&removed);
                Ok(true)
            }
        }
    })
}

/// Replaces the inline value of the live edge `half` belongs to, in both halves; `false` when no
/// live edge is there.
pub(crate) fn write_value(
    half: CanonicalEdgeOccurrence,
    value: &[u8],
) -> Result<bool, TopologyError> {
    let half = edge_ref(half);
    with_graph_mut(|graph| {
        graph
            .set_edge_value(half, value)
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}

/// Starts deleting `vertex_id` (DETACH DELETE): it takes no new edges, and the work list removes
/// its edges, reporting each ([`super::work::WorkStep::Detached`]), then deletes it.
pub(crate) fn begin_vertex_delete(vertex_id: VertexId) -> Result<(), TopologyError> {
    with_graph_mut(|graph| {
        graph
            .begin_delete(node(vertex_id))
            .map_err(|error| TopologyError::from_store(graph, error))
    })?;
    work::list_detach(vertex_id);
    Ok(())
}

/// Deletes `vertex_id` now if it has no edges; `false` when it still has some.
pub(crate) fn delete_detached_vertex(vertex_id: VertexId) -> Result<bool, TopologyError> {
    let vertex = node(vertex_id);
    with_graph_mut(|graph| {
        let map = |graph: &super::Store, error| TopologyError::from_store(graph, error);
        if graph.has_edges(vertex).map_err(|error| map(graph, error))? {
            return Ok(false);
        }
        graph
            .begin_delete(vertex)
            .map_err(|error| map(graph, error))?;
        let step = graph
            .delete_step(vertex, core::num::NonZeroU32::MIN)
            .map_err(|error| map(graph, error))?;
        debug_assert_eq!(
            step.progress(),
            Progress::Done,
            "a vertex without edges is deleted at once"
        );
        Ok(true)
    })
}

/// Which side of a vertex [`list_compaction_of_holes`] looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionSide {
    /// Its directed out-edges and undirected edges.
    Forward,
    /// Its directed in-edges.
    Reverse,
}

/// Lists for compaction every bucket of `vertex_id` on `side` that has a hole (bulk-ingest
/// finalize: `bulk_load` and fresh inserts leave none, so this usually lists nothing). Returns
/// whether it listed any.
pub(crate) fn list_compaction_of_holes(
    vertex_id: VertexId,
    side: CompactionSide,
) -> Result<bool, TopologyError> {
    let directions: &[Direction] = match side {
        CompactionSide::Forward => &[Direction::Outgoing, Direction::Undirected],
        CompactionSide::Reverse => &[Direction::Incoming],
    };
    with_graph(|graph| work::list_buckets_with_holes(graph, vertex_id, directions))
}

/// How many entries the maintenance work list holds.
pub(crate) fn maintenance_queue_len() -> u64 {
    work::len()
}
