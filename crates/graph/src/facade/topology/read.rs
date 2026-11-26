//! Reads of the store: a vertex's edges on one side, under one label or all of them, with or
//! without their inline values, from the start, past an offset or from a slot.
//!
//! A label's inline values have one width in every bucket (lara-graph's `declare_width`), so a
//! bucket's values read as fixed-width chunks wherever it lives; reads that want them ask the
//! store for them alongside the edges, and reads that do not, read the edges alone.

use core::num::NonZeroU32;
use core::ops::ControlFlow;

use gleaph_graph_kernel::entry::{
    Edge, EdgeDirectedness, EdgeOrder, EdgeOrientation, EdgeSlotIndex, TaggedEdgeLabelId, VertexId,
};
use lara_graph::{Adjacency, Cursor, Direction, EdgeRef, NodeId, Order, Position};

use super::{
    CanonicalEdgeOccurrence, Store, TopologyError, bucket, edge_of, edge_ref, node, tagged,
    with_graph,
};

/// How many edges one batch of [`visit_value_batches`] or [`visit_edge_value_batches`] holds at
/// most, so that a hub's bucket is not copied whole before the caller sees any of it.
const BATCH_EDGES: usize = 256;

/// How many positions [`skip_then_visit`] reads per window once it has skipped its offset.
const SKIP_WINDOW: NonZeroU32 = NonZeroU32::new(64).unwrap();

const fn order(order: EdgeOrder) -> Order {
    match order {
        EdgeOrder::Ascending => Order::Ascending,
        EdgeOrder::Descending => Order::Descending,
    }
}

/// The direction of `side`'s directed buckets.
const fn directed(side: EdgeOrientation) -> Direction {
    match side {
        EdgeOrientation::Forward => Direction::Outgoing,
        EdgeOrientation::Reverse => Direction::Incoming,
    }
}

/// How [`visit_bucket`] reads one label's bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BucketRead {
    /// Every edge with its inline values, in the order.
    WithValues(EdgeOrder),
    /// Every edge without its inline values, in the order.
    Topology(EdgeOrder),
}

/// Calls `visit` with each edge of `vertex_id`'s `label` bucket on `side`.
pub(crate) fn visit_bucket(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    read: BucketRead,
    mut visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    let key = bucket(side, label);
    with_graph(|graph| {
        let read = match read {
            BucketRead::WithValues(edge_order) => graph.visit_neighbors_with_values(
                node(vertex_id),
                key,
                order(edge_order),
                |neighbor, value| {
                    visit(edge_of(label, neighbor.node(), neighbor.position(), value));
                    ControlFlow::<()>::Continue(())
                },
            ),
            BucketRead::Topology(edge_order) => {
                graph.visit_neighbors(node(vertex_id), key, order(edge_order), |neighbor| {
                    visit(edge_of(label, neighbor.node(), neighbor.position(), &[]));
                    ControlFlow::<()>::Continue(())
                })
            }
        };
        read.map(|_| ())
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}

/// The edge at `occurrence`, with its inline values; `None` for a hole or a position past the
/// bucket.
pub(crate) fn edge_at(occurrence: CanonicalEdgeOccurrence) -> Result<Option<Edge>, TopologyError> {
    let half = edge_ref(occurrence);
    with_graph(|graph| {
        let map = |error| TopologyError::from_store(graph, error);
        let Some(target) = graph.other_end(half).map_err(map)? else {
            return Ok(None);
        };
        let value = match graph.width(half.label()) {
            Some(_) => graph.edge_value(half).map_err(map)?.unwrap_or_default(),
            None => Vec::new(),
        };
        Ok(Some(edge_of(
            occurrence.label_id,
            target,
            half.position(),
            &value,
        )))
    })
}

/// Reusable buffers for [`visit_value_batches`].
#[derive(Clone, Debug, Default)]
pub(crate) struct InlinePropertyValueBatchScratch {
    /// The position of each value chunk.
    pub slot_indices: Vec<u32>,
    /// The values, `slot_indices.len() * byte_width` bytes.
    pub values: Vec<u8>,
}

impl InlinePropertyValueBatchScratch {
    /// Empties both buffers, keeping their capacity.
    pub(crate) fn clear(&mut self) {
        self.slot_indices.clear();
        self.values.clear();
    }
}

/// One batch of positions and their inline values in one bucket.
pub(crate) struct InlinePropertyValueBatch<'a> {
    /// The width of each value chunk.
    pub byte_width: u16,
    /// The position of each value chunk.
    pub slot_indices: &'a [u32],
    /// The values, in the same order.
    pub values: &'a [u8],
}

/// Calls `visit` with batches of the positions and inline values of `vertex_id`'s `label` bucket
/// on `side`, in `order`, reusing `scratch`: the first phase of a values-first read. It reads the
/// values alone, so a batch may hold positions whose edge was removed (with the value it last
/// had): the second phase, [`read_slots`], reads the edges and drops those. A label without
/// inline values gives no batch.
pub(crate) fn visit_value_batches<Visit>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    edge_order: EdgeOrder,
    scratch: &mut InlinePropertyValueBatchScratch,
    mut visit: Visit,
) -> Result<(), TopologyError>
where
    Visit: for<'b> FnMut(InlinePropertyValueBatch<'b>),
{
    let key = bucket(side, label);
    with_graph(|graph| {
        let Some(width) = graph.width(key.label()) else {
            return Ok(());
        };
        let byte_width = width.get();
        scratch.clear();
        let mut emit = |scratch: &mut InlinePropertyValueBatchScratch| {
            if !scratch.slot_indices.is_empty() {
                visit(InlinePropertyValueBatch {
                    byte_width,
                    slot_indices: &scratch.slot_indices,
                    values: &scratch.values,
                });
                scratch.clear();
            }
        };
        graph
            .visit_values(
                node(vertex_id),
                key,
                order(edge_order),
                |position, value| {
                    scratch.slot_indices.push(position.index());
                    scratch.values.extend_from_slice(value);
                    if scratch.slot_indices.len() == BATCH_EDGES {
                        emit(scratch);
                    }
                    ControlFlow::<()>::Continue(())
                },
            )
            .map_err(|error| TopologyError::from_store(graph, error))?;
        emit(scratch);
        Ok(())
    })
}

/// Reusable buffers for [`visit_edge_value_batches`].
#[derive(Clone, Debug, Default)]
pub(crate) struct EdgeInlinePropertyBatchScratch {
    /// The edges, without their values.
    pub edges: Vec<Edge>,
    /// Their values, `edges.len() * byte_width` bytes.
    pub inline_property_bytes: Vec<u8>,
}

impl EdgeInlinePropertyBatchScratch {
    /// Empties both buffers, keeping their capacity.
    pub(crate) fn clear(&mut self) {
        self.edges.clear();
        self.inline_property_bytes.clear();
    }
}

/// One batch of edges and their inline values in one bucket.
pub(crate) struct EdgeInlinePropertyBatch<'a> {
    /// The width of each edge's value.
    pub byte_width: u16,
    /// The edges, without their values.
    pub edges: &'a [Edge],
    /// Their values, in the same order.
    pub inline_property_bytes: &'a [u8],
}

/// Calls `visit` with batches of the live edges and inline values of `vertex_id`'s `label` bucket
/// on `side`, in `order`, reusing `scratch`. A label without inline values gives no batch.
pub(crate) fn visit_edge_value_batches<Visit>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    edge_order: EdgeOrder,
    scratch: &mut EdgeInlinePropertyBatchScratch,
    mut visit: Visit,
) -> Result<(), TopologyError>
where
    Visit: for<'b> FnMut(EdgeInlinePropertyBatch<'b>),
{
    let key = bucket(side, label);
    with_graph(|graph| {
        let Some(width) = graph.width(key.label()) else {
            return Ok(());
        };
        let byte_width = width.get();
        scratch.clear();
        let mut emit = |scratch: &mut EdgeInlinePropertyBatchScratch| {
            if !scratch.edges.is_empty() {
                visit(EdgeInlinePropertyBatch {
                    byte_width,
                    edges: &scratch.edges,
                    inline_property_bytes: &scratch.inline_property_bytes,
                });
                scratch.clear();
            }
        };
        graph
            .visit_neighbors_with_values(
                node(vertex_id),
                key,
                order(edge_order),
                |neighbor, value| {
                    scratch
                        .edges
                        .push(edge_of(label, neighbor.node(), neighbor.position(), &[]));
                    scratch.inline_property_bytes.extend_from_slice(value);
                    if scratch.edges.len() == BATCH_EDGES {
                        emit(scratch);
                    }
                    ControlFlow::<()>::Continue(())
                },
            )
            .map_err(|error| TopologyError::from_store(graph, error))?;
        emit(scratch);
        Ok(())
    })
}

/// Calls `visit` with the live edges at `slots` of `vertex_id`'s `label` bucket on `side`, with
/// their inline values: each position once, in `order`, holes left out.
pub(crate) fn read_slots(
    vertex_id: VertexId,
    side: EdgeOrientation,
    label: TaggedEdgeLabelId,
    slots: &[EdgeSlotIndex],
    edge_order: EdgeOrder,
    mut visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    if slots.is_empty() {
        return Ok(());
    }
    let mut positions: Vec<Position> = slots.iter().map(|slot| Position::new(slot.raw())).collect();
    positions.sort_unstable_by_key(|position| position.index());
    positions.dedup();
    if edge_order == EdgeOrder::Descending {
        positions.reverse();
    }
    let key = bucket(side, label);
    with_graph(|graph| {
        visit_positions(
            graph,
            node(vertex_id),
            key,
            &positions,
            |target, position, value| {
                visit(edge_of(label, target, position, value));
            },
        )
    })
}

/// Calls `visit` with the target, position and value of each live edge at `positions` of
/// `vertex`'s `key` bucket, in the order given.
fn visit_positions(
    graph: &Store,
    vertex: NodeId,
    key: Adjacency,
    positions: &[Position],
    mut visit: impl FnMut(NodeId, Position, &[u8]),
) -> Result<(), TopologyError> {
    let map = |error| TopologyError::from_store(graph, error);
    let mut targets = Vec::with_capacity(positions.len());
    graph
        .neighbors_at(vertex, key, positions, &mut targets)
        .map_err(map)?;
    let width = graph
        .width(key.label())
        .map_or(0, |width| usize::from(width.get()));
    let mut values = Vec::new();
    if width > 0 {
        graph
            .values_at(vertex, key, positions, &mut values)
            .map_err(map)?;
    }
    for (index, (&position, target)) in positions.iter().zip(&targets).enumerate() {
        if let Some(target) = *target {
            visit(
                target,
                position,
                &values[index * width..(index + 1) * width],
            );
        }
    }
    Ok(())
}

/// Which buckets [`skip_then_visit`] walks: one label's, or every label's of one directedness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SkipScope {
    Label(TaggedEdgeLabelId),
    Directedness(EdgeDirectedness),
}

/// Skips `offset_remaining` live edges of `vertex_id` on `side` within `scope`, walking labels
/// and positions descending, then calls `visit` with each later one, with its inline values,
/// until it returns `Ok(true)` (stop) or an error. `offset_remaining` counts down across calls,
/// so one OFFSET spans several vertices; a whole bucket is skipped by its degree and the rest by
/// `nth_position`, without reading the skipped edges (ADR 0136, Decision 5). Returns `Ok(true)`
/// when `visit` stopped the walk and `Ok(false)` when the edges ran out.
pub(crate) fn skip_then_visit<Visit, Err>(
    vertex_id: VertexId,
    side: EdgeOrientation,
    scope: SkipScope,
    offset_remaining: &mut usize,
    mut visit: Visit,
) -> Result<Result<bool, Err>, TopologyError>
where
    Visit: FnMut(Edge) -> Result<bool, Err>,
{
    let vertex = node(vertex_id);
    with_graph(|graph| {
        let map = |error| TopologyError::from_store(graph, error);
        let keys: Vec<Adjacency> = match scope {
            SkipScope::Label(label) => vec![bucket(side, label)],
            SkipScope::Directedness(directedness) => {
                let direction = match directedness {
                    EdgeDirectedness::Undirected => Direction::Undirected,
                    EdgeDirectedness::Directed => directed(side),
                };
                let mut labels = Vec::new();
                graph
                    .edge_labels(vertex, direction, &mut labels)
                    .map_err(map)?;
                labels.sort_unstable_by_key(|label| core::cmp::Reverse(label.get()));
                labels
                    .into_iter()
                    .map(|label| Adjacency::new(label, direction))
                    .collect()
            }
        };
        for key in keys {
            let degree = graph.degree(vertex, key).map_err(map)? as usize;
            if *offset_remaining >= degree {
                *offset_remaining -= degree;
                continue;
            }
            let skip = u32::try_from(*offset_remaining).expect("an offset below a degree fits u32");
            *offset_remaining = 0;
            let Some(start) = graph
                .nth_position(vertex, key, Order::Descending, skip)
                .map_err(map)?
            else {
                continue;
            };
            let (label, _) = tagged(key);
            let mut cursor = Some(Cursor::at(Order::Descending, start, SKIP_WINDOW));
            let mut window = Vec::new();
            while let Some(at) = cursor {
                window.clear();
                cursor = graph
                    .visit_window(vertex, key, at, |neighbor| window.push(neighbor.position()))
                    .map_err(map)?;
                let mut outcome = None;
                visit_positions(graph, vertex, key, &window, |target, position, value| {
                    if outcome.is_none() {
                        match visit(edge_of(label, target, position, value)) {
                            Ok(false) => {}
                            Ok(true) => outcome = Some(Ok(true)),
                            Err(error) => outcome = Some(Err(error)),
                        }
                    }
                })?;
                if let Some(outcome) = outcome {
                    return Ok(outcome);
                }
            }
        }
        Ok(Ok(false))
    })
}

/// Which of a vertex's edges [`visit_all`] walks, under every label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AllEdges {
    /// Its directed edges on one side.
    Directed(EdgeOrientation),
    /// Its undirected edges.
    Undirected,
}

/// Calls `visit` with each of `vertex_id`'s `edges`, with their inline values, labels and
/// positions in `order`. When none of the vertex's labels there has inline values, the edges are
/// read in one walk of its label bucket list (`visit_all_neighbors`); otherwise bucket by bucket.
pub(crate) fn visit_all(
    vertex_id: VertexId,
    edges: AllEdges,
    edge_order: EdgeOrder,
    mut visit: impl FnMut(Edge),
) -> Result<(), TopologyError> {
    let direction = match edges {
        AllEdges::Directed(side) => directed(side),
        AllEdges::Undirected => Direction::Undirected,
    };
    let vertex = node(vertex_id);
    with_graph(|graph| {
        let map = |error| TopologyError::from_store(graph, error);
        let mut labels = Vec::new();
        graph
            .edge_labels(vertex, direction, &mut labels)
            .map_err(map)?;
        if labels.iter().all(|&label| graph.width(label).is_none()) {
            graph
                .visit_all_neighbors(vertex, direction, order(edge_order), |label, neighbor| {
                    let (label, _) = tagged(Adjacency::new(label, direction));
                    visit(edge_of(label, neighbor.node(), neighbor.position(), &[]));
                    ControlFlow::<()>::Continue(())
                })
                .map_err(map)?;
            return Ok(());
        }
        labels.sort_unstable_by_key(|label| label.get());
        if edge_order == EdgeOrder::Descending {
            labels.reverse();
        }
        for label in labels {
            let adjacency = Adjacency::new(label, direction);
            let (tag, _) = tagged(adjacency);
            graph
                .visit_neighbors_with_values(
                    vertex,
                    adjacency,
                    order(edge_order),
                    |neighbor, value| {
                        visit(edge_of(tag, neighbor.node(), neighbor.position(), value));
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map_err(map)?;
        }
        Ok(())
    })
}

/// Visits at most `limit` positions of `vertex_id`'s forward `label` bucket from `start_slot`,
/// holes included, with their inline values, and returns the next position and whether the
/// bucket is exhausted: a cursor an export can persist, valid until the bucket is compacted.
pub(crate) fn visit_forward_window(
    vertex_id: VertexId,
    label: TaggedEdgeLabelId,
    start_slot: u32,
    limit: u32,
    mut visit: impl FnMut(Edge),
) -> Result<(u32, bool), TopologyError> {
    let Some(span) = NonZeroU32::new(limit) else {
        return Ok((start_slot, false));
    };
    let key = bucket(EdgeOrientation::Forward, label);
    let vertex = node(vertex_id);
    with_graph(|graph| {
        let mut window = Vec::new();
        let next = graph
            .visit_window(
                vertex,
                key,
                Cursor::at(Order::Ascending, Position::new(start_slot), span),
                |neighbor| window.push(neighbor.position()),
            )
            .map_err(|error| TopologyError::from_store(graph, error))?;
        visit_positions(graph, vertex, key, &window, |target, position, value| {
            visit(edge_of(label, target, position, value));
        })?;
        // An exhausted bucket ends after its last live edge in the window (or where the window
        // started, when it holds none), so a caller charging the positions read is not charged
        // the whole window for an empty or missing bucket.
        Ok(match next.and_then(|cursor| cursor.start()) {
            Some(position) => (position.index(), false),
            None => (
                window
                    .last()
                    .map_or(start_slot, |position| position.index() + 1)
                    .max(start_slot),
                true,
            ),
        })
    })
}

/// The label of the bucket of `vertex_id` on the forward side (directed out-edges and undirected
/// edges) whose position `edge.edge_slot_index` holds an edge to `edge`'s neighbor, if one does:
/// the edge's own label when its bucket holds it there (an edge a read returned), else the first
/// label whose bucket does (an edge built by a caller, with no label).
pub(crate) fn find_forward_label(
    vertex_id: VertexId,
    edge: &Edge,
) -> Result<Option<TaggedEdgeLabelId>, TopologyError> {
    let vertex = node(vertex_id);
    let target = node(edge.neighbor_vid());
    let position = Position::new(edge.edge_slot_index.raw());
    with_graph(|graph| {
        let map = |error| TopologyError::from_store(graph, error);
        let own = TaggedEdgeLabelId::from_raw(edge.label_id);
        if graph
            .other_end(EdgeRef::new(
                vertex,
                bucket(EdgeOrientation::Forward, own),
                position,
            ))
            .map_err(map)?
            == Some(target)
        {
            return Ok(Some(own));
        }
        let mut labels = Vec::new();
        for direction in [Direction::Outgoing, Direction::Undirected] {
            graph
                .edge_labels(vertex, direction, &mut labels)
                .map_err(map)?;
            for &label in &labels {
                let adjacency = Adjacency::new(label, direction);
                let half = EdgeRef::new(vertex, adjacency, position);
                if graph.other_end(half).map_err(map)? == Some(target) {
                    return Ok(Some(tagged(adjacency).0));
                }
            }
        }
        Ok(None)
    })
}

/// Whether `vertex_id` has any edge, on either side.
pub(crate) fn has_incident_edges(vertex_id: VertexId) -> Result<bool, TopologyError> {
    with_graph(|graph| {
        graph
            .has_edges(node(vertex_id))
            .map_err(|error| TopologyError::from_store(graph, error))
    })
}
