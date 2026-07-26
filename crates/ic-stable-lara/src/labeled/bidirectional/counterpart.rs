//! Persistence-free canonical counterpart occurrence resolution for bidirectional labeled LARA.
//!
//! The adjacency rows remain the source of truth.  This module derives a counterpart by
//! selecting the same live equal-neighbor occurrence rank in the counterpart bucket; it does
//! not allocate or persist an index.

use super::{Orientation, mate::CanonicalEdgeOccurrence};
use crate::{
    VertexId,
    labeled::{BucketEntryPosition, BucketLabelKey, LabeledLaraGraph, graph::EdgeSlotState},
    traits::CsrEdgeTombstone,
};
use ic_stable_structures::Memory;
use std::fmt;

use std::cell::Cell;

thread_local! {
    static CANONICAL_COUNTERPART_LOOKUPS: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn reset_canonical_counterpart_lookup_count() {
    CANONICAL_COUNTERPART_LOOKUPS.with(|count| count.set(0));
}

pub(crate) fn canonical_counterpart_lookup_count() -> u32 {
    CANONICAL_COUNTERPART_LOOKUPS.with(Cell::get)
}

/// Canonical edge handle without orientation.
///
/// This identifies the canonical sidecar occurrence used by Graph for properties and derived indexes.
/// It carries no raw slab or overflow-log location; the containing bucket supplies orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeHandle {
    /// Vertex owning the label bucket row.
    pub owner_vertex_id: VertexId,
    /// Storage label of the bucket containing the row.
    pub label_id: BucketLabelKey,
    /// Bucket entry position, including tombstone positions, inside the label row.
    pub slot_index: BucketEntryPosition,
}

impl EdgeHandle {
    /// Construct a handle at a specific slot.
    pub const fn at_slot(
        owner_vertex_id: VertexId,
        label_id: BucketLabelKey,
        slot_index: BucketEntryPosition,
    ) -> Self {
        Self {
            owner_vertex_id,
            label_id,
            slot_index,
        }
    }
}

impl CanonicalEdgeOccurrence {
    /// Return the canonical sidecar handle for this occurrence.
    pub fn handle(self) -> EdgeHandle {
        EdgeHandle {
            owner_vertex_id: self.owner_vertex_id,
            label_id: self.label_id,
            slot_index: self.slot_index,
        }
    }
}

/// Zero-based position of a row in the live equal-neighbor subsequence of its relation.
///
/// PairOrdinal is derived, never persisted. It is invalidated by tombstones and compaction
/// renumbering and must not outlive the mutation boundary that produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairOrdinal(pub u32);

/// Fail-closed errors returned by live-only counterpart resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CounterpartLookupError {
    /// The requested bucket entry position is not present in the declared bucket.
    SourceNotFound(CanonicalEdgeOccurrence),
    /// The requested row is tombstoned; only live rows have a PairOrdinal.
    SourceNotLive(CanonicalEdgeOccurrence),
    /// The counterpart bucket has no matching occurrence at the source rank.
    CounterpartNotFound(CanonicalEdgeOccurrence),
    /// A canonical source occurrence was observed more than once during selection.
    AmbiguousSource(CanonicalEdgeOccurrence),
    /// The two projections disagree about the number of live equal-neighbor rows.
    InconsistentRelation {
        /// Source canonical occurrence.
        source: CanonicalEdgeOccurrence,
        /// Owner vertex of the counterpart bucket.
        counterpart_owner: VertexId,
        /// Number of matching source rows.
        source_count: u32,
        /// Number of matching counterpart rows.
        counterpart_count: u32,
    },
    /// The occurrence uses an impossible orientation for its bucket kind.
    InvalidOrientation(CanonicalEdgeOccurrence),
    /// The underlying LARA scan failed.
    ReadFailed(String),
}

impl fmt::Display for CounterpartLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound(edge) => write!(f, "source edge is not present: {edge:?}"),
            Self::SourceNotLive(edge) => write!(f, "source edge is not live: {edge:?}"),
            Self::CounterpartNotFound(edge) => {
                write!(f, "counterpart edge is missing for {edge:?}")
            }
            Self::AmbiguousSource(edge) => write!(f, "source edge is ambiguous: {edge:?}"),
            Self::InconsistentRelation {
                source,
                counterpart_owner,
                source_count,
                counterpart_count,
            } => write!(
                f,
                "inconsistent relation for {source:?}: counterpart owner={counterpart_owner:?}, source_count={source_count}, counterpart_count={counterpart_count}"
            ),
            Self::InvalidOrientation(edge) => write!(f, "invalid orientation for {edge:?}"),
            Self::ReadFailed(message) => write!(f, "counterpart scan failed: {message}"),
        }
    }
}

impl std::error::Error for CounterpartLookupError {}

/// Computes the equal-target PairOrdinal of a live source row using streaming scans.
fn scan_pair_ordinal<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    edge: CanonicalEdgeOccurrence,
) -> Result<(VertexId, PairOrdinal, u32), CounterpartLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let target = match graph
        .read_edge_slot_state_for_label(edge.owner_vertex_id, edge.label_id, edge.slot_index.raw())
        .map_err(|err| CounterpartLookupError::ReadFailed(err.to_string()))?
    {
        EdgeSlotState::Live(row) => row.neighbor_vid(),
        EdgeSlotState::Tombstone => return Err(CounterpartLookupError::SourceNotLive(edge)),
        EdgeSlotState::Missing => return Err(CounterpartLookupError::SourceNotFound(edge)),
    };

    let mut ordinal = 0u32;
    let mut total = 0u32;
    let mut passed_source = false;
    graph
        .for_each_live_edge_slot_for_label(edge.owner_vertex_id, edge.label_id, |slot, row| {
            if row.neighbor_vid() != target {
                return;
            }
            if slot == edge.slot_index.raw() {
                passed_source = true;
            } else if !passed_source {
                ordinal = ordinal.saturating_add(1);
            }
            total = total.saturating_add(1);
        })
        .map_err(|err| CounterpartLookupError::ReadFailed(err.to_string()))?;

    if !passed_source {
        return Err(CounterpartLookupError::SourceNotFound(edge));
    }
    Ok((target, PairOrdinal(ordinal), total))
}

/// Selects the live row at `ordinal` from the counterpart bucket's equal-neighbor subsequence.
fn select_counterpart<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    owner: VertexId,
    label: BucketLabelKey,
    source_owner: VertexId,
    ordinal: PairOrdinal,
) -> Result<(CanonicalEdgeOccurrence, u32), CounterpartLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let mut matching = 0u32;
    let mut candidate: Option<u32> = None;
    graph
        .for_each_live_edge_slot_for_label(owner, label, |slot, row| {
            if row.neighbor_vid() != source_owner {
                return;
            }
            if matching == ordinal.0 {
                candidate = Some(slot);
            }
            matching = matching.saturating_add(1);
        })
        .map_err(|err| CounterpartLookupError::ReadFailed(err.to_string()))?;

    let Some(slot) = candidate else {
        return Err(CounterpartLookupError::CounterpartNotFound(
            CanonicalEdgeOccurrence {
                orientation: Orientation::Forward,
                owner_vertex_id: owner,
                label_id: label,
                slot_index: BucketEntryPosition::new(u32::MAX),
            },
        ));
    };

    Ok((
        CanonicalEdgeOccurrence {
            orientation: Orientation::Forward,
            owner_vertex_id: owner,
            label_id: label,
            slot_index: BucketEntryPosition::new(slot),
        },
        matching,
    ))
}
/// Determines canonical ownership from a canonical occurrence and its resolved counterpart.
pub fn canonical_from_counterpart(
    source: CanonicalEdgeOccurrence,
    counterpart: CanonicalEdgeOccurrence,
) -> EdgeHandle {
    if source.label_id.is_directed() {
        return match source.orientation {
            Orientation::Forward => source.handle(),
            Orientation::Reverse => counterpart.handle(),
        };
    }
    // Undirected: canonical owner is max(endpoint). The two endpoints are source.owner
    // (the row owner) and counterpart.owner (the neighbor, which for undirected is also an
    // endpoint). For a non-self undirected edge, one endpoint owns the forward row and the
    // other endpoint owns the counterpart forward row, so max(endpoint) is the right choice.
    if source.owner_vertex_id >= counterpart.owner_vertex_id {
        source.handle()
    } else {
        counterpart.handle()
    }
}

/// Resolves the exact paired physical entry by equal-neighbor occurrence rank.
pub fn counterpart_of<E, M>(
    forward: &LabeledLaraGraph<E, M>,
    reverse: &LabeledLaraGraph<E, M>,
    edge: CanonicalEdgeOccurrence,
) -> Result<CanonicalEdgeOccurrence, CounterpartLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    #[cfg(test)]
    CANONICAL_COUNTERPART_LOOKUPS.with(|count| count.set(count.get().saturating_add(1)));

    if edge.label_id.is_undirected() && matches!(edge.orientation, Orientation::Reverse) {
        return Err(CounterpartLookupError::InvalidOrientation(edge));
    }

    let source_graph = match edge.orientation {
        Orientation::Forward => forward,
        Orientation::Reverse => reverse,
    };

    let (target, pair_ordinal, source_count) = scan_pair_ordinal(source_graph, edge)?;

    // Undirected self-loop is its own counterpart.
    if edge.label_id.is_undirected() && target == edge.owner_vertex_id {
        return Ok(edge);
    }

    let (counterpart_orientation, counterpart_owner) = if edge.label_id.is_directed() {
        match edge.orientation {
            Orientation::Forward => (Orientation::Reverse, target),
            Orientation::Reverse => (Orientation::Forward, target),
        }
    } else {
        (Orientation::Forward, target)
    };

    let counterpart_graph = match counterpart_orientation {
        Orientation::Forward => forward,
        Orientation::Reverse => reverse,
    };

    let (counterpart, counterpart_count) = select_counterpart(
        counterpart_graph,
        counterpart_owner,
        edge.label_id,
        edge.owner_vertex_id,
        pair_ordinal,
    )?;

    if source_count != counterpart_count {
        return Err(CounterpartLookupError::InconsistentRelation {
            source: edge,
            counterpart_owner,
            source_count,
            counterpart_count,
        });
    }

    Ok(CanonicalEdgeOccurrence {
        orientation: counterpart_orientation,
        ..counterpart
    })
}

/// Resolves the canonical physical entry for an edge without persistent metadata.
pub fn canonical_handle<E, M>(
    forward: &LabeledLaraGraph<E, M>,
    reverse: &LabeledLaraGraph<E, M>,
    edge: CanonicalEdgeOccurrence,
) -> Result<EdgeHandle, CounterpartLookupError>
where
    E: CsrEdgeTombstone,
    M: Memory,
{
    let counterpart = counterpart_of(forward, reverse, edge)?;
    Ok(canonical_from_counterpart(edge, counterpart))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VertexId;
    use crate::labeled::{
        BucketLabelKey,
        graph::test_support::{TestEdge, test_graph_with_default},
        record::LabeledVertex,
    };
    use crate::traits::CsrEdge;

    fn directed_label(id: u16) -> BucketLabelKey {
        BucketLabelKey::directed_from_index(id)
    }

    fn undirected_label(id: u16) -> BucketLabelKey {
        BucketLabelKey::undirected_from_index(id)
    }

    fn two_sided_graphs(
        label: BucketLabelKey,
    ) -> (
        LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        LabeledLaraGraph<TestEdge, crate::VectorMemory>,
    ) {
        (
            test_graph_with_default(label),
            test_graph_with_default(label),
        )
    }

    fn push_vertices(graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>, count: u32) {
        for _ in 0..count {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
    }

    fn push_both_vertices(
        forward: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        reverse: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        count: u32,
    ) {
        push_vertices(forward, count);
        push_vertices(reverse, count);
    }

    fn find_slot_for_target(
        graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        owner: VertexId,
        label: BucketLabelKey,
        target: VertexId,
    ) -> Option<u32> {
        let mut found = None;
        graph
            .for_each_live_edge_slot_for_label(owner, label, |slot, row| {
                if row.neighbor_vid() == target {
                    found = Some(slot);
                }
            })
            .unwrap();
        found
    }

    fn insert_directed(
        forward: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        reverse: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        src: u32,
        tgt: u32,
        label: BucketLabelKey,
    ) -> (CanonicalEdgeOccurrence, CanonicalEdgeOccurrence) {
        forward
            .insert_edge(VertexId::from(src), label, TestEdge { target: tgt })
            .unwrap();
        reverse
            .insert_edge(VertexId::from(tgt), label, TestEdge { target: src })
            .unwrap();
        let fwd_slot =
            find_slot_for_target(forward, VertexId::from(src), label, VertexId::from(tgt))
                .expect("forward edge inserted");
        let rev_slot =
            find_slot_for_target(reverse, VertexId::from(tgt), label, VertexId::from(src))
                .expect("reverse edge inserted");
        (
            CanonicalEdgeOccurrence {
                orientation: Orientation::Forward,
                owner_vertex_id: VertexId::from(src),
                label_id: label,
                slot_index: BucketEntryPosition::new(fwd_slot),
            },
            CanonicalEdgeOccurrence {
                orientation: Orientation::Reverse,
                owner_vertex_id: VertexId::from(tgt),
                label_id: label,
                slot_index: BucketEntryPosition::new(rev_slot),
            },
        )
    }

    fn insert_undirected(
        forward: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
        a: u32,
        b: u32,
        label: BucketLabelKey,
    ) -> CanonicalEdgeOccurrence {
        let (high, low) = if a > b { (a, b) } else { (b, a) };
        forward
            .insert_edge(VertexId::from(high), label, TestEdge { target: low })
            .unwrap();
        forward
            .insert_edge(VertexId::from(low), label, TestEdge { target: high })
            .unwrap();
        let slot = find_slot_for_target(forward, VertexId::from(high), label, VertexId::from(low))
            .expect("high-owner edge inserted");
        CanonicalEdgeOccurrence {
            orientation: Orientation::Forward,
            owner_vertex_id: VertexId::from(high),
            label_id: label,
            slot_index: BucketEntryPosition::new(slot),
        }
    }

    #[test]
    fn directed_edge_counterpart() {
        let label = directed_label(10);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let (fwd, rev) = insert_directed(&forward, &reverse, 1, 2, label);

        let resolved_rev = counterpart_of(&forward, &reverse, fwd).unwrap();
        assert_eq!(resolved_rev, rev);

        let resolved_fwd = counterpart_of(&forward, &reverse, rev).unwrap();
        assert_eq!(resolved_fwd, fwd);

        assert_eq!(
            canonical_handle(&forward, &reverse, fwd).unwrap(),
            fwd.handle()
        );
        assert_eq!(
            canonical_handle(&forward, &reverse, rev).unwrap(),
            fwd.handle()
        );
    }

    #[test]
    fn reverse_occurrence_reads_from_incoming_bucket_with_exact_slot() {
        let label = directed_label(12);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let (fwd, rev) = insert_directed(&forward, &reverse, 1, 2, label);

        let mut seen = Vec::new();
        let _ = reverse
            .visit_edges::<()>(
                rev.owner_vertex_id,
                rev.label_id,
                crate::labeled::OutEdgeOrder::Ascending,
                |slot, edge| {
                    seen.push((slot, edge.target));
                    std::ops::ControlFlow::Continue(())
                },
            )
            .unwrap();

        assert_eq!(seen, vec![(rev.slot_index, 1)]);
        assert_eq!(counterpart_of(&forward, &reverse, rev).unwrap(), fwd);
    }

    #[test]
    fn directed_self_loop_uses_the_reverse_projection() {
        let label = directed_label(11);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 2);
        let (fwd, rev) = insert_directed(&forward, &reverse, 1, 1, label);

        assert_eq!(counterpart_of(&forward, &reverse, fwd).unwrap(), rev);
        assert_eq!(counterpart_of(&forward, &reverse, rev).unwrap(), fwd);
    }

    #[test]
    fn undirected_edge_counterpart() {
        let label = undirected_label(20);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let high = insert_undirected(&forward, 1, 2, label);

        let low = counterpart_of(&forward, &reverse, high).unwrap();
        assert_eq!(low.owner_vertex_id, VertexId::from(1));

        let back = counterpart_of(&forward, &reverse, low).unwrap();
        assert_eq!(back, high);

        assert_eq!(
            canonical_handle(&forward, &reverse, high).unwrap(),
            high.handle()
        );
        assert_eq!(
            canonical_handle(&forward, &reverse, low).unwrap(),
            high.handle()
        );
    }

    #[test]
    fn undirected_self_loop_counterpart() {
        let label = undirected_label(30);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 6);
        let slot = {
            forward
                .insert_edge(VertexId::from(5), label, TestEdge { target: 5 })
                .unwrap();
            find_slot_for_target(&forward, VertexId::from(5), label, VertexId::from(5))
                .expect("self-loop inserted")
        };
        let edge = CanonicalEdgeOccurrence {
            orientation: Orientation::Forward,
            owner_vertex_id: VertexId::from(5),
            label_id: label,
            slot_index: BucketEntryPosition::new(slot),
        };

        assert_eq!(counterpart_of(&forward, &reverse, edge).unwrap(), edge);
        assert_eq!(
            canonical_handle(&forward, &reverse, edge).unwrap(),
            edge.handle()
        );
    }

    #[test]
    fn parallel_directed_edges_keep_pair_order() {
        let label = directed_label(40);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let mut forward_refs = Vec::new();
        let mut reverse_refs = Vec::new();
        for _value in 1..=3u32 {
            let (fwd, rev) = insert_directed(&forward, &reverse, 1, 2, label);
            forward_refs.push(fwd);
            reverse_refs.push(rev);
        }

        for (fwd, rev) in forward_refs.iter().zip(&reverse_refs) {
            let resolved = counterpart_of(&forward, &reverse, *fwd).unwrap();
            assert_eq!(&resolved, rev);
            let back = counterpart_of(&forward, &reverse, resolved).unwrap();
            assert_eq!(back, *fwd);
        }
    }

    #[test]
    fn interleaved_neighbors_do_not_change_pair_order() {
        let label = directed_label(41);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 4);
        let first = insert_directed(&forward, &reverse, 1, 2, label);
        let _interleaved = insert_directed(&forward, &reverse, 1, 3, label);
        let second = insert_directed(&forward, &reverse, 1, 2, label);

        assert_eq!(
            counterpart_of(&forward, &reverse, first.0).unwrap(),
            first.1
        );
        assert_eq!(
            counterpart_of(&forward, &reverse, second.0).unwrap(),
            second.1
        );
    }

    #[test]
    fn tombstoned_source_is_not_reported_as_absent() {
        let label = directed_label(42);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let _first = insert_directed(&forward, &reverse, 1, 2, label);
        let (fwd, _) = insert_directed(&forward, &reverse, 1, 2, label);
        let _third = insert_directed(&forward, &reverse, 1, 2, label);
        forward
            .compact_vertex_edge_span(VertexId::from(1), 0)
            .unwrap();
        forward
            .remove_edge_at_slot(fwd.owner_vertex_id, fwd.label_id, fwd.slot_index.raw())
            .unwrap();

        let err = counterpart_of(&forward, &reverse, fwd).unwrap_err();
        assert_eq!(err, CounterpartLookupError::SourceNotLive(fwd));
    }

    #[test]
    fn relation_count_mismatch_is_not_silently_accepted() {
        let label = directed_label(43);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        let first = insert_directed(&forward, &reverse, 1, 2, label);
        let _second = insert_directed(&forward, &reverse, 1, 2, label);
        reverse
            .remove_edge_at_slot(
                first.1.owner_vertex_id,
                first.1.label_id,
                first.1.slot_index.raw(),
            )
            .unwrap();

        let err = counterpart_of(&forward, &reverse, first.0).unwrap_err();
        assert!(matches!(
            err,
            CounterpartLookupError::InconsistentRelation {
                source,
                source_count: 2,
                counterpart_count: 1,
                ..
            } if source == first.0
        ));
    }

    #[test]
    fn source_not_found_returns_error() {
        let label = directed_label(1);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 2);
        let edge = CanonicalEdgeOccurrence {
            orientation: Orientation::Forward,
            owner_vertex_id: VertexId::from(1),
            label_id: label,
            slot_index: BucketEntryPosition::new(0),
        };
        let err = counterpart_of(&forward, &reverse, edge).unwrap_err();
        assert!(matches!(err, CounterpartLookupError::SourceNotFound(_)));
    }

    #[test]
    fn impossible_logical_slot_mapping_fails_closed() {
        let label = directed_label(2);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 2);
        let edge = CanonicalEdgeOccurrence {
            orientation: Orientation::Forward,
            owner_vertex_id: VertexId::from(1),
            label_id: label,
            slot_index: BucketEntryPosition::new(u32::MAX),
        };

        let err = counterpart_of(&forward, &reverse, edge).unwrap_err();
        assert_eq!(err, CounterpartLookupError::SourceNotFound(edge));
    }

    #[test]
    fn reverse_orientation_on_undirected_is_invalid() {
        let label = undirected_label(1);
        let (forward, reverse) = two_sided_graphs(label);
        push_both_vertices(&forward, &reverse, 3);
        forward
            .insert_edge(VertexId::from(1), label, TestEdge { target: 2 })
            .unwrap();
        forward
            .insert_edge(VertexId::from(2), label, TestEdge { target: 1 })
            .unwrap();
        let slot = find_slot_for_target(&forward, VertexId::from(1), label, VertexId::from(2))
            .expect("edge inserted");
        let edge = CanonicalEdgeOccurrence {
            orientation: Orientation::Reverse,
            owner_vertex_id: VertexId::from(1),
            label_id: label,
            slot_index: BucketEntryPosition::new(slot),
        };
        let err = counterpart_of(&forward, &reverse, edge).unwrap_err();
        assert!(matches!(err, CounterpartLookupError::InvalidOrientation(_)));
    }
}
