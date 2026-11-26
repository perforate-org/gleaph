//! Vertex labels, stored with the vertices: each vertex's label set and the label -> vertex
//! postings, updated together by one store call (ADR 0136, Decision 8). `VertexLabelStore` is the
//! only writer; index fencing and `label_pending` events stay in the facade around it.

use core::ops::ControlFlow;

use gleaph_graph_kernel::entry::{VertexId, VertexLabelId};
use lara_graph::Label;

use super::{TopologyError, node, vertex_of, with_graph, with_graph_mut};

const fn store_label(label: VertexLabelId) -> Label {
    Label::new(label.raw())
}

/// Replaces `vertex_id`'s labels with `labels` and updates the postings.
pub(crate) fn set(vertex_id: VertexId, labels: &[VertexLabelId]) -> Result<(), TopologyError> {
    let labels: Vec<Label> = labels.iter().copied().map(store_label).collect();
    with_graph_mut(|graph| {
        graph
            .set_node_labels(node(vertex_id), &labels)
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}

/// `vertex_id`'s labels, ascending.
pub(crate) fn labels_of(vertex_id: VertexId) -> Result<Vec<VertexLabelId>, TopologyError> {
    let mut labels = Vec::new();
    with_graph(|graph| {
        graph
            .node_labels(node(vertex_id), &mut labels)
            .map_err(|error| TopologyError::from_store(graph, error))
    })?;
    let mut labels: Vec<VertexLabelId> = labels
        .into_iter()
        .map(|label| VertexLabelId::from_raw(label.get()))
        .collect();
    labels.sort_unstable();
    Ok(labels)
}

/// Whether `vertex_id` has `label`.
pub(crate) fn has(vertex_id: VertexId, label: VertexLabelId) -> Result<bool, TopologyError> {
    with_graph(|graph| {
        graph
            .has_node_label(node(vertex_id), store_label(label))
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}

/// Calls `visit` with each vertex that has `label`, ascending, after `after` when given, until it
/// breaks.
pub(crate) fn visit_with_label(
    label: VertexLabelId,
    after: Option<VertexId>,
    mut visit: impl FnMut(VertexId) -> ControlFlow<()>,
) {
    with_graph(|graph| {
        graph.visit_nodes_with_label(store_label(label), after.map(node), |vertex| {
            visit(vertex_of(vertex))
        })
    });
}

/// How many vertices have `label`.
pub(crate) fn count_with_label(label: VertexLabelId) -> u32 {
    with_graph(|graph| graph.node_label_count(store_label(label)))
}
