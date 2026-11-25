//! The store's vertices: how many, adding them, and their rows.
//!
//! A vertex row is ic-stable-lara's (`Vertex`, its label buckets and tombstone); lara-graph keeps
//! a node record instead and the rows go at the backend swap (ADR 0136, Decisions 2 and 11).

use gleaph_graph_kernel::entry::{Vertex, VertexId};

use super::{TopologyError, with_graph, with_graph_mut};

/// How many vertices the store holds, deleted ones included.
pub(crate) use ic_stable_lara::VertexCount;

/// How many vertices the store holds, deleted ones included.
pub(crate) fn count() -> VertexCount {
    with_graph(|graph| graph.vertex_count())
}

/// Appends a vertex with `row` and returns its id.
pub(crate) fn push(row: Vertex) -> Result<VertexId, TopologyError> {
    with_graph_mut(|graph| graph.push_vertex_row(row.into()))
}

/// Appends a vertex for each of `rows`, in order, and returns their ids.
pub(crate) fn push_all(
    rows: impl IntoIterator<Item = Vertex>,
) -> Result<Vec<VertexId>, TopologyError> {
    with_graph_mut(|graph| graph.push_vertices(rows.into_iter().map(Into::into)))
}

/// `vertex_id`'s row, if it has one.
pub(crate) fn row(vertex_id: VertexId) -> Option<Vertex> {
    with_graph(|graph| graph.vertex_row(vertex_id).ok().map(Vertex::from))
}

/// Replaces `vertex_id`'s row.
pub(crate) fn set_row(vertex_id: VertexId, row: Vertex) -> Result<(), TopologyError> {
    with_graph(|graph| graph.set_vertex_row(vertex_id, &row.into()))
}

/// Replaces the rows of consecutive vertices from `start`.
pub(crate) fn set_rows(
    start: VertexId,
    rows: impl IntoIterator<Item = Vertex>,
) -> Result<(), TopologyError> {
    with_graph(|graph| graph.set_vertex_rows(start, rows.into_iter().map(Into::into)))
}
