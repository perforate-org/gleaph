//! The Graph shard's topology: its vertices, edges, their labels and inline values, held by one
//! lara-graph store. This module is the one owner of that store's state and the only code that
//! names it (ADR 0136, Decisions 1 and 11): the rest of the crate speaks Gleaph's vocabulary
//! (`VertexId`, `TaggedEdgeLabelId`, `EdgeSlotIndex`, `EdgeOrientation`) and this module
//! translates.
//!
//! - A vertex is a node, one to one (Decision 2).
//! - A tagged label is a `(Label, Direction)` bucket: `0x8000 | l` on the forward side is
//!   `(l, Outgoing)`, on the reverse side `(l, Incoming)`; an undirected `l` is `(l, Undirected)`
//!   on either side (Decision 3).
//! - An edge slot is a position. Positions move only when a bucket is compacted, and the work
//!   list that compacts reports every move ([`work`], Decision 6).

pub(crate) mod change;
pub(crate) mod counterpart;
mod error;
pub(crate) mod insert;
pub(crate) mod labels;
pub(crate) mod read;
pub(crate) mod vertex;
pub(crate) mod work;

use core::cell::RefCell;

use gleaph_graph_kernel::entry::vertex_ref::MAX_PAYLOAD_VERTEX_ID;
use gleaph_graph_kernel::entry::{
    Edge, EdgeInlinePropertyBytes, EdgeOrientation, EdgeSlotIndex, TaggedEdgeLabelId, VertexId,
    VertexRef,
};
use lara_graph::{Direction, EdgeRef, Graph, Label, Memories, NodeId, Position};

use super::stable::memory::{self, Memory};

pub(crate) use error::{CounterpartLookupError, TopologyError};
pub(crate) use read::{
    EdgeInlinePropertyBatch, EdgeInlinePropertyBatchScratch, InlinePropertyValueBatch,
    InlinePropertyValueBatchScratch,
};

/// One half of an edge: the side and bucket it is in and its position there. The canonical half
/// is the edge's handle (Decision 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalEdgeOccurrence {
    /// The forward side for a directed edge's source half and for either half of an undirected
    /// edge; the reverse side for a directed edge's target half.
    pub orientation: EdgeOrientation,
    /// The vertex whose bucket holds the half.
    pub owner_vertex_id: VertexId,
    /// The bucket's tagged label.
    pub label_id: TaggedEdgeLabelId,
    /// The half's position in the bucket.
    pub slot_index: EdgeSlotIndex,
}

type Store = Graph<Memory>;

thread_local! {
    static GRAPH: RefCell<Store> = RefCell::new(open());
}

/// Opens the store in its memories, or formats it on a fresh install.
fn open() -> Store {
    let graph = Graph::try_init(Memories::from_fn(|role| {
        memory::topology_memory(role.index())
    }))
    .unwrap_or_else(|error| {
        // A store of another layout version on an upgrade: Gleaph keeps no migration before
        // launch (ADR 0135), so the canister needs a fresh install.
        panic!("graph topology store cannot be opened ({error:?}); a fresh install is required")
    });
    crate::facade::init_ic_gql_extensions();
    graph
}

/// Opens the store now, so that a layout mismatch traps where this is called (an upgrade) rather
/// than at the first read.
pub(crate) fn initialize() {
    with_graph(|_| {});
}

/// Opens the store again on its persisted memories, as an upgrade does (`bench::stable_layout`).
#[cfg(feature = "canbench")]
pub(crate) fn reopen_for_bench() {
    core::hint::black_box(open());
    work::reopen_for_bench();
}

fn with_graph<R>(f: impl FnOnce(&Store) -> R) -> R {
    GRAPH.with_borrow(f)
}

fn with_graph_mut<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    GRAPH.with_borrow_mut(f)
}

/// One past the last vertex id the shard mints: local ids stay within the 30-bit payload of
/// `VertexRef` and the vector page store (ADR 0136, Decision 2), below lara-graph's 2^31.
const VERTEX_LIMIT: u32 = MAX_PAYLOAD_VERTEX_ID + 1;

/// The node a vertex is. An id past [`VERTEX_LIMIT`] (an unchecked id from a request) names
/// the first id no vertex can have, so the store answers it as an unknown node.
fn node(vertex_id: VertexId) -> NodeId {
    NodeId::new(u32::from(vertex_id).min(VERTEX_LIMIT))
}

/// The local vertex a node is. Nothing adds a proxy yet (Decision 10).
fn vertex_of(node: NodeId) -> VertexId {
    debug_assert!(!node.is_proxy(), "no proxy is added before sharding");
    VertexId::from(node.index())
}

/// The bucket a tagged label names on `side`.
fn bucket(side: EdgeOrientation, label: TaggedEdgeLabelId) -> (Label, Direction) {
    let direction = if label.is_undirected() {
        Direction::Undirected
    } else {
        match side {
            EdgeOrientation::Forward => Direction::Outgoing,
            EdgeOrientation::Reverse => Direction::Incoming,
        }
    };
    (Label::new(label.label_index()), direction)
}

/// The tagged label and side of a bucket.
fn tagged(label: Label, direction: Direction) -> (TaggedEdgeLabelId, EdgeOrientation) {
    match direction {
        Direction::Outgoing => (
            TaggedEdgeLabelId::directed_from_index(label.get()),
            EdgeOrientation::Forward,
        ),
        Direction::Incoming => (
            TaggedEdgeLabelId::directed_from_index(label.get()),
            EdgeOrientation::Reverse,
        ),
        Direction::Undirected => (
            TaggedEdgeLabelId::undirected_from_index(label.get()),
            EdgeOrientation::Forward,
        ),
    }
}

fn edge_ref(occurrence: CanonicalEdgeOccurrence) -> EdgeRef {
    let (label, direction) = bucket(occurrence.orientation, occurrence.label_id);
    EdgeRef::new(
        node(occurrence.owner_vertex_id),
        label,
        direction,
        Position::new(occurrence.slot_index.raw()),
    )
}

fn occurrence_of(half: EdgeRef) -> CanonicalEdgeOccurrence {
    let (label_id, orientation) = tagged(half.label(), half.direction());
    CanonicalEdgeOccurrence {
        orientation,
        owner_vertex_id: vertex_of(half.node()),
        label_id,
        slot_index: EdgeSlotIndex::from_raw(half.position().index()),
    }
}

/// The edge a reader receives at `position` of a `label` bucket, to `target`, carrying `value`.
fn edge_of(label: TaggedEdgeLabelId, target: NodeId, position: Position, value: &[u8]) -> Edge {
    Edge {
        target: VertexRef::local(vertex_of(target)),
        edge_slot_index: EdgeSlotIndex::from_raw(position.index()),
        label_id: label.raw(),
        inline_property: if value.is_empty() {
            EdgeInlinePropertyBytes::EMPTY
        } else {
            EdgeInlinePropertyBytes::from_slice(value)
        },
    }
}
