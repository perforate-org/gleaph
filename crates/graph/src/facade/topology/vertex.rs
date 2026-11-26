//! The store's vertices: lara-graph nodes, one to one (ADR 0136, Decision 2). Ids are dense in
//! creation order and never reused; a vertex keeps no row of its own, only a state.

use core::num::NonZeroU32;

use gleaph_graph_kernel::entry::VertexId;
use lara_graph::NodeState;

use super::{Store, TopologyError, VERTEX_LIMIT, node, vertex_of, with_graph, with_graph_mut};

/// Where a vertex is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VertexState {
    Live,
    /// Being deleted (DETACH DELETE): it takes no new edges while the work list removes its
    /// edges. Readers treat it as deleted, as they did a tombstoned row from the start of DETACH.
    Detaching,
    Deleted,
}

impl VertexState {
    /// Whether readers see the vertex: neither deleted nor being deleted.
    pub(crate) const fn is_live(self) -> bool {
        matches!(self, Self::Live)
    }
}

/// How many vertices the store holds, deleted ones included: ids below it exist.
pub(crate) fn count() -> u32 {
    with_graph(|graph| graph.node_count())
}

/// Fails when `count` more vertices would pass [`VERTEX_LIMIT`].
fn ensure_room(graph: &Store, count: u32) -> Result<(), TopologyError> {
    if graph
        .node_count()
        .checked_add(count)
        .is_none_or(|total| total > VERTEX_LIMIT)
    {
        return Err(TopologyError::AddressSpaceExhausted);
    }
    Ok(())
}

/// Adds a vertex and returns its id.
pub(crate) fn add() -> Result<VertexId, TopologyError> {
    with_graph_mut(|graph| {
        ensure_room(graph, 1)?;
        graph
            .add_node()
            .map(vertex_of)
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}

/// Adds `count` vertices and returns their ids, in order.
pub(crate) fn add_many(count: u32) -> Result<Vec<VertexId>, TopologyError> {
    let Some(count) = NonZeroU32::new(count) else {
        return Ok(Vec::new());
    };
    let first = with_graph_mut(|graph| {
        ensure_room(graph, count.get())?;
        graph
            .add_nodes(count)
            .map_err(|error| TopologyError::from_store(graph, error))
    })?;
    Ok((first.index()..first.index() + count.get())
        .map(VertexId::from)
        .collect())
}

/// `vertex_id`'s state; `None` when no vertex has that id.
pub(crate) fn state(vertex_id: VertexId) -> Option<VertexState> {
    with_graph(|graph| graph.node_state(node(vertex_id)).ok()).map(|state| match state {
        NodeState::Live => VertexState::Live,
        NodeState::Detaching => VertexState::Detaching,
        NodeState::Deleted => VertexState::Deleted,
    })
}
