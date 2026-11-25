//! Reads of the store: a vertex's edges on one side, under one label or all of them, with or
//! without their inline values, from the start, past an offset or from a slot.
//!
//! The variants a read needs are parameters here, not names: a side ([`EdgeOrientation`]), a
//! [`BucketRead`], a [`SkipScope`], [`AllEdges`]. Each combination calls the store method the
//! facade called before (ADR 0136, Decision 11), so reads behave and cost as they did; the
//! backend swap reimplements these functions alone.

use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeOrder, EdgeOrientation, EdgeSlotIndex, TaggedEdgeLabelId, VertexId,
};
use ic_stable_lara::labeled::BucketDirectedness;

use super::{
    EdgeInlinePropertyBatch, EdgeInlinePropertyBatchScratch, InlinePropertyValueBatch,
    InlinePropertyValueBatchScratch, TopologyError, with_graph,
};

/// Whether a read checks the vertex first, or the caller already has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VertexCheck {
    Checked,
    Unchecked,
}

/// How [`visit_bucket`] reads one label's bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BucketRead {
    /// Every edge with its inline values, in the store's default order.
    Default,
    /// Every edge with its inline values, in `order`.
    Ordered(EdgeOrder),
    /// As `Default`, the vertex already checked.
    Unchecked,
    /// Every edge without its inline values, in `order`. The reverse side checks the vertex
    /// either way: the store has no unchecked topology read there.
    Topology {
        order: EdgeOrder,
        vertex: VertexCheck,
    },
}

/// Calls `visit` with each edge of `vertex_id`'s `label` bucket on `side`.
pub(crate) fn visit_bucket(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    read: BucketRead,
    visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    with_graph(|graph| match (side, read) {
        (EdgeOrientation::Forward, BucketRead::Default) => {
            graph.for_each_out_edges_for_label(vertex_id, label, visit)
        }
        (EdgeOrientation::Forward, BucketRead::Ordered(order)) => {
            graph.for_each_out_edges_for_label_ordered(vertex_id, label, order, visit)
        }
        (EdgeOrientation::Forward, BucketRead::Unchecked) => {
            graph.for_each_out_edges_for_label_unchecked(vertex_id, label, visit)
        }
        (
            EdgeOrientation::Forward,
            BucketRead::Topology {
                order,
                vertex: VertexCheck::Checked,
            },
        ) => graph.for_each_out_edges_for_label_topology_ordered(vertex_id, label, order, visit),
        (
            EdgeOrientation::Forward,
            BucketRead::Topology {
                order,
                vertex: VertexCheck::Unchecked,
            },
        ) => graph.for_each_out_edges_for_label_topology_unchecked(vertex_id, label, order, visit),
        (EdgeOrientation::Reverse, BucketRead::Default) => {
            graph.for_each_in_edges_for_label(vertex_id, label, visit)
        }
        (EdgeOrientation::Reverse, BucketRead::Ordered(order)) => {
            graph.for_each_in_edges_for_label_ordered(vertex_id, label, order, visit)
        }
        (EdgeOrientation::Reverse, BucketRead::Unchecked) => {
            graph.for_each_in_edges_for_label_unchecked(vertex_id, label, visit)
        }
        (EdgeOrientation::Reverse, BucketRead::Topology { order, .. }) => {
            graph.for_each_in_edges_for_label_topology_ordered(vertex_id, label, order, visit)
        }
    })
}

/// Calls `visit` with batches of the positions and inline values of `vertex_id`'s `label`
/// bucket on `side`, in `order`, reusing `scratch`.
pub(crate) fn visit_value_batches<Visit>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    order: EdgeOrder,
    scratch: &mut InlinePropertyValueBatchScratch,
    visit: Visit,
) -> Result<(), TopologyError>
where
    Visit: for<'b> FnMut(InlinePropertyValueBatch<'b>),
{
    with_graph(|graph| match side {
        EdgeOrientation::Forward => graph
            .visit_out_inline_property_batches_for_label(vertex_id, label, order, scratch, visit),
        EdgeOrientation::Reverse => graph
            .visit_in_inline_property_batches_for_label(vertex_id, label, order, scratch, visit),
    })
}

/// Calls `visit` with batches of the edges and inline values of `vertex_id`'s `label` bucket on
/// `side`, in `order`, reusing `scratch`.
pub(crate) fn visit_edge_value_batches<Visit>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    order: EdgeOrder,
    scratch: &mut EdgeInlinePropertyBatchScratch<Edge>,
    visit: Visit,
) -> Result<(), TopologyError>
where
    Visit: for<'b> FnMut(EdgeInlinePropertyBatch<'b, Edge>),
{
    with_graph(|graph| match side {
        EdgeOrientation::Forward => graph.visit_out_edge_inline_property_batches_for_label(
            vertex_id, label, order, scratch, visit,
        ),
        EdgeOrientation::Reverse => graph.visit_in_edge_inline_property_batches_for_label(
            vertex_id, label, order, scratch, visit,
        ),
    })
}

/// Calls `visit` with the edges at `slots` of `vertex_id`'s `label` bucket on `side`, in `order`.
pub(crate) fn read_slots(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    slots: &[EdgeSlotIndex],
    order: EdgeOrder,
    visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    with_graph(|graph| match side {
        EdgeOrientation::Forward => {
            graph.read_out_edge_slots_for_label(vertex_id, label, slots, order, visit)
        }
        EdgeOrientation::Reverse => {
            graph.read_in_edge_slots_for_label(vertex_id, label, slots, order, visit)
        }
    })
}

/// A batched read a bucket may qualify for. Both describe ic-stable-lara's slab layout and go
/// with it at the backend swap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BatchEligibility {
    /// Its inline values read as one contiguous span.
    DenseValueBatches,
    /// A predicate on inline values can run before the edges are decoded.
    ValueBytesFirstPredicate,
}

/// Whether `vertex_id`'s `label` bucket on `side` qualifies for `batch`.
pub(crate) fn bucket_eligible(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    batch: BatchEligibility,
) -> Result<bool, TopologyError> {
    with_graph(|graph| match (side, batch) {
        (EdgeOrientation::Forward, BatchEligibility::DenseValueBatches) => {
            graph.out_bucket_dense_inline_property_batch_eligible(vertex_id, label)
        }
        (EdgeOrientation::Reverse, BatchEligibility::DenseValueBatches) => {
            graph.in_bucket_dense_inline_property_batch_eligible(vertex_id, label)
        }
        (EdgeOrientation::Forward, BatchEligibility::ValueBytesFirstPredicate) => {
            graph.out_bucket_inline_property_bytes_first_predicate_eligible(vertex_id, label)
        }
        (EdgeOrientation::Reverse, BatchEligibility::ValueBytesFirstPredicate) => {
            graph.in_bucket_inline_property_bytes_first_predicate_eligible(vertex_id, label)
        }
    })
}

/// Which buckets [`skip_then_visit`] walks: one label's, or every label's of one directedness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipScope {
    Label(TaggedEdgeLabelId),
    Directedness(EdgeDirectedness),
}

/// Skips `offset_remaining` edges of `vertex_id` on `side` within `scope`, then calls `visit`
/// with each later one until it returns `Ok(false)` or an error. `offset_remaining` counts down
/// across calls, so one OFFSET spans several vertices.
pub(crate) fn skip_then_visit<Visit, Err>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    scope: SkipScope,
    offset_remaining: &mut usize,
    visit: Visit,
) -> Result<Result<bool, Err>, TopologyError>
where
    Visit: FnMut(Edge) -> Result<bool, Err>,
{
    with_graph(|graph| match (side, scope) {
        (EdgeOrientation::Forward, SkipScope::Label(label)) => graph
            .skip_then_visit_each_forward_out_edge_for_label(
                vertex_id,
                label,
                offset_remaining,
                visit,
            ),
        (EdgeOrientation::Forward, SkipScope::Directedness(directedness)) => graph
            .skip_then_visit_each_forward_out_edge_by_directedness(
                vertex_id,
                bucket_directedness(directedness),
                offset_remaining,
                visit,
            ),
        (EdgeOrientation::Reverse, SkipScope::Label(label)) => graph
            .skip_then_visit_each_reverse_out_edge_for_label(
                vertex_id,
                label,
                offset_remaining,
                visit,
            ),
        (EdgeOrientation::Reverse, SkipScope::Directedness(directedness)) => graph
            .skip_then_visit_each_reverse_out_edge_by_directedness(
                vertex_id,
                bucket_directedness(directedness),
                offset_remaining,
                visit,
            ),
    })
}

fn bucket_directedness(directedness: EdgeDirectedness) -> BucketDirectedness {
    match directedness {
        EdgeDirectedness::Directed => BucketDirectedness::Directed,
        EdgeDirectedness::Undirected => BucketDirectedness::Undirected,
    }
}

/// Which of a vertex's edges [`visit_all`] walks, under every label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllEdges {
    /// Its directed edges on one side; the vertex is checked.
    Directed(EdgeOrientation),
    /// Its undirected edges.
    Undirected(VertexCheck),
}

/// Calls `visit` with each of `vertex_id`'s `edges`, labels and positions in `order`.
pub(crate) fn visit_all(
    vertex_id: VertexId,
    edges: AllEdges,
    order: EdgeOrder,
    visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    with_graph(|graph| match edges {
        AllEdges::Directed(EdgeOrientation::Forward) => {
            graph.for_each_directed_out_edges(vertex_id, order, visit)
        }
        AllEdges::Directed(EdgeOrientation::Reverse) => {
            graph.for_each_directed_in_edges(vertex_id, order, visit)
        }
        AllEdges::Undirected(VertexCheck::Checked) => {
            graph.for_each_undirected_edges(vertex_id, order, visit)
        }
        AllEdges::Undirected(VertexCheck::Unchecked) => {
            graph.for_each_undirected_edges_unchecked(vertex_id, order, visit)
        }
    })
}

/// Visits at most `limit` slots of `vertex_id`'s forward `label` bucket from `start_slot`, holes
/// included, and returns the next slot and whether the bucket is exhausted.
pub(crate) fn visit_forward_window(
    vertex_id: VertexId,
    label: TaggedEdgeLabelId,
    start_slot: u32,
    limit: u32,
    mut visit: impl FnMut(Edge),
) -> Result<(u32, bool), TopologyError> {
    with_graph(|graph| {
        graph.visit_forward_edges_from_slot_with_inline_property(
            vertex_id,
            label,
            start_slot,
            limit,
            |_, edge| visit(edge),
        )
    })
}

/// How many vertices the store holds, deleted ones included.
pub(crate) fn vertex_count() -> u32 {
    with_graph(|graph| u32::from(graph.vertex_count()))
}

/// The label of the forward bucket of `vertex_id` that holds `edge`, if one does.
pub(crate) fn find_forward_label(
    vertex_id: VertexId,
    edge: &Edge,
) -> Result<Option<TaggedEdgeLabelId>, TopologyError> {
    with_graph(|graph| graph.find_forward_edge_label(vertex_id, edge))
}
