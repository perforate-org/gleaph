//! Gleaph's maintenance work list (ADR 0136, Decision 6): the buckets to compact and the vertices
//! to detach, kept in their own memory because lara-graph keeps no queue.
//!
//! A key is 8 bytes, `[kind][node u32][label u16][direction]` big-endian, so the map's order is
//! the order work is taken in: detaching vertices first, then migrations (reserved: nothing
//! changes a label's width before the Router allows it, Decision 7), then compactions. A key
//! names its work, so listing the same work twice lists it once.
//!
//! [`step`] runs one bounded store call for the first entry and hands back what it changed, for
//! the caller to apply to the sidecars and indexes before anything reads them; the entry leaves
//! the list when its work is done. The caller decides how many steps fit its instruction budget.

use core::cell::RefCell;
use core::num::NonZeroU32;
use std::borrow::Cow;

use gleaph_graph_kernel::entry::VertexId;
use ic_stable_structures::StableBTreeMap;
use ic_stable_structures::storable::{Bound, Storable};
use lara_graph::{Adjacency, BucketFill, Direction, EdgeLabel, EdgeRef, NodeId, Progress};

use super::change::{EdgeSlotMove, RemovedEdge, SlotMoves};
use super::{Store, TopologyError, node, tagged, vertex_of, with_graph_mut};
use crate::facade::stable::memory::{self, Memory};

/// How many edges one detach step removes at most.
const DETACH_STEP_EDGES: NonZeroU32 = NonZeroU32::new(64).unwrap();

const DETACH: u8 = 0;
/// Reserved for width migrations (Decision 7).
#[expect(
    dead_code,
    reason = "reserved key kind: width migrations come with the Router's change"
)]
const MIGRATE: u8 = 1;
const COMPACT: u8 = 2;

/// One entry of the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct WorkKey([u8; 8]);

/// What an entry asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Work {
    Detach(NodeId),
    Compact(NodeId, Adjacency),
}

const fn direction_byte(direction: Direction) -> u8 {
    match direction {
        Direction::Outgoing => 0,
        Direction::Incoming => 1,
        Direction::Undirected => 2,
    }
}

impl WorkKey {
    fn of(work: Work) -> Self {
        let (kind, node, label, direction) = match work {
            Work::Detach(node) => (DETACH, node, 0, 0),
            Work::Compact(node, adjacency) => (
                COMPACT,
                node,
                adjacency.label().get(),
                direction_byte(adjacency.direction()),
            ),
        };
        let mut bytes = [0; 8];
        bytes[0] = kind;
        bytes[1..5].copy_from_slice(&node.index().to_be_bytes());
        bytes[5..7].copy_from_slice(&label.to_be_bytes());
        bytes[7] = direction;
        Self(bytes)
    }

    fn work(self) -> Work {
        let bytes = self.0;
        let node = NodeId::new(u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]));
        match bytes[0] {
            DETACH => Work::Detach(node),
            COMPACT => {
                let label = EdgeLabel::new(u16::from_be_bytes([bytes[5], bytes[6]]));
                let direction = match bytes[7] {
                    0 => Direction::Outgoing,
                    1 => Direction::Incoming,
                    2 => Direction::Undirected,
                    other => unreachable!("work list key with direction byte {other}"),
                };
                Work::Compact(node, Adjacency::new(label, direction))
            }
            other => unreachable!("work list key of kind {other}"),
        }
    }
}

impl Storable for WorkKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut key = [0; 8];
        key.copy_from_slice(&bytes);
        Self(key)
    }
}

thread_local! {
    static WORK: RefCell<StableBTreeMap<WorkKey, (), Memory>> =
        RefCell::new(StableBTreeMap::init(memory::work_list_memory()));
}

/// Opens the work list again on its persisted memory, as an upgrade does.
#[cfg(feature = "canbench")]
pub(super) fn reopen_for_bench() {
    core::hint::black_box(StableBTreeMap::<WorkKey, (), Memory>::init(
        memory::work_list_memory(),
    ));
}

fn list(work: Work) {
    WORK.with_borrow_mut(|list| list.insert(WorkKey::of(work), ()));
}

/// How many entries the list holds.
pub(crate) fn len() -> u64 {
    WORK.with_borrow(StableBTreeMap::len)
}

/// Lists `vertex_id` for detaching.
pub(super) fn list_detach(vertex_id: VertexId) {
    list(Work::Detach(node(vertex_id)));
}

/// Lists a bucket for compaction when a removal left it with at least as many holes as live
/// edges, whatever its placement: refilling holes alone keeps the most positions a bucket ever
/// had (Decision 6). An emptied bucket is listed too; its compaction drops it.
fn list_if_sparse(half: EdgeRef, fill: BucketFill) {
    if fill.holes() > 0 && fill.holes() >= fill.live() {
        list(Work::Compact(half.node(), half.adjacency()));
    }
}

/// Lists the buckets `removed` left sparse, at both its halves.
pub(super) fn list_after_removal(removed: &lara_graph::RemovedEdge) {
    let halves = removed.halves();
    list_if_sparse(halves.canonical(), removed.canonical_fill());
    list_if_sparse(halves.other(), removed.other_fill());
}

/// Lists for compaction every bucket of `vertex_id` on `directions` that has a hole; returns
/// whether it listed any.
pub(super) fn list_buckets_with_holes(
    graph: &Store,
    vertex_id: VertexId,
    directions: &[Direction],
) -> Result<bool, TopologyError> {
    let vertex = node(vertex_id);
    let map = |error| TopologyError::from_store(graph, error);
    let mut labels = Vec::new();
    let mut listed = false;
    for &direction in directions {
        graph
            .edge_labels(vertex, direction, &mut labels)
            .map_err(map)?;
        for &label in &labels {
            let adjacency = Adjacency::new(label, direction);
            if graph.holes(vertex, adjacency).map_err(map)? > 0 {
                list(Work::Compact(vertex, adjacency));
                listed = true;
            }
        }
    }
    Ok(listed)
}

/// What one step changed, for the caller to apply before anything reads the sidecars.
#[derive(Debug)]
#[must_use = "a work step's changes must be applied to the sidecars and indexes"]
pub(crate) enum WorkStep {
    /// A compaction step moved these canonical halves.
    Compacted(SlotMoves),
    /// A detach step removed these edges, both halves of each; `deleted` is the vertex once it
    /// has no edges left and is deleted.
    Detached {
        removed: Vec<RemovedEdge>,
        deleted: Option<VertexId>,
    },
}

/// Runs one bounded step of the first entry and returns what it changed; `None` when the list
/// is empty.
pub(crate) fn step() -> Result<Option<WorkStep>, TopologyError> {
    let Some(key) = WORK.with_borrow(|list| list.first_key_value().map(|(key, ())| key)) else {
        return Ok(None);
    };
    let (step, progress) = with_graph_mut(|graph| match key.work() {
        Work::Detach(vertex) => {
            let step = graph
                .delete_step(vertex, DETACH_STEP_EDGES)
                .map_err(|error| TopologyError::from_store(graph, error))?;
            let removed = step
                .changes()
                .iter()
                .map(|removed| {
                    list_after_removal(removed);
                    RemovedEdge::of(removed)
                })
                .collect();
            let deleted = (step.progress() == Progress::Done).then(|| vertex_of(vertex));
            Ok::<_, TopologyError>((WorkStep::Detached { removed, deleted }, step.progress()))
        }
        Work::Compact(vertex, key) => {
            let step = graph
                .compact(vertex, key)
                .map_err(|error| TopologyError::from_store(graph, error))?;
            let (label_id, _) = tagged(key);
            let mut moves: Vec<EdgeSlotMove> = step
                .changes()
                .iter()
                .filter(|moved| {
                    let (_, after) = moved.edge_refs(vertex, key);
                    after.is_canonical(moved.node())
                })
                .map(|moved| EdgeSlotMove {
                    label_id,
                    old_slot_index: moved.from().index(),
                    new_slot_index: moved.to().index(),
                })
                .collect();
            // Compaction keeps live edges in order and moves them toward the start, so applying
            // the moves from the lowest old position never lands one on a position another
            // move has yet to vacate.
            moves.sort_unstable_by_key(|moved| moved.old_slot_index);
            Ok((
                WorkStep::Compacted(SlotMoves::new(vertex_of(vertex), moves)),
                step.progress(),
            ))
        }
    })?;
    if progress == Progress::Done {
        WORK.with_borrow_mut(|list| list.remove(&key));
    }
    Ok(Some(step))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_order_detaching_before_compaction_and_round_trip() {
        let detach = WorkKey::of(Work::Detach(NodeId::new(9)));
        let compact = Work::Compact(NodeId::new(1), EdgeLabel::new(3).incoming());
        let compact_key = WorkKey::of(compact);
        assert!(detach < compact_key);
        assert_eq!(detach.work(), Work::Detach(NodeId::new(9)));
        assert_eq!(compact_key.work(), compact);
        assert_eq!(
            WorkKey::from_bytes(compact_key.to_bytes()),
            compact_key,
            "a key reads back as written"
        );
    }
}
