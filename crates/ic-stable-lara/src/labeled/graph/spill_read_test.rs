use super::*;
use crate::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone},
};
use std::ops::ControlFlow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row<const N: usize>(u32);
impl<const N: usize> CsrEdge for Row<N> {
    const BYTES: usize = N;
    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[..4].try_into().unwrap()))
    }
    fn write_to(&self, bytes: &mut [u8]) {
        bytes.fill(0);
        bytes[..4].copy_from_slice(&self.0.to_le_bytes());
    }
    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}
impl<const N: usize> CsrEdgeTombstone for Row<N> {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}
fn exercise<const N: usize>(bypass: bool) {
    for target in [0, 33, 65, 999] {
        let m = crate::test_support::vector_memory;
        let graph = LabeledLaraGraph::<Row<N>, _>::new(
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            m(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(if bypass { 1 } else { 2 });
        for i in 0..67 {
            graph
                .insert_edge_skip_leaf_cascade(vid, label, Row(i), EdgePlacementPolicy::Insertion)
                .unwrap();
        }
        assert_eq!(graph.vertices().get(vid).is_default_edge_labeled(), bypass);
        for dead in [31, 32, 66] {
            assert_eq!(
                graph
                    .remove_edge_at_slot(vid, label, dead)
                    .unwrap()
                    .unwrap()
                    .0,
                dead
            );
        }
        let live: Vec<u32> = (0..67).filter(|i| ![31, 32, 66].contains(i)).collect();
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut seen = Vec::new();
            graph
                .visit_edges(vid, label, order, |slot, edge| {
                    seen.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            let mut expected: Vec<_> = live.iter().map(|i| (*i, *i)).collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(seen, expected);
        }
        let mut visited = Vec::new();
        let result = graph
            .remove_edge_matching_with_move(vid, label, |edge| {
                visited.push(edge.0);
                edge.0 == target
            })
            .unwrap();
        let expected: Vec<_> = live
            .iter()
            .copied()
            .take_while(|i| *i != target)
            .chain(live.contains(&target).then_some(target))
            .collect();
        assert_eq!(visited, expected);
        assert_eq!(
            result.as_ref().map(|r| r.removed.0),
            live.contains(&target).then_some(target)
        );
        if let Some(result) = result {
            assert!(result.moves.is_empty());
        }
        let mut survivors = Vec::new();
        graph
            .visit_edges(vid, label, OutEdgeOrder::Ascending, |slot, edge| {
                survivors.push((slot.raw(), edge.0));
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        assert_eq!(
            survivors,
            live.into_iter()
                .filter(|i| *i != target)
                .map(|i| (i, i))
                .collect::<Vec<_>>()
        );
    }
}
#[test]
fn spill_matching_attaches_sidecars_before_predicates_and_preserves_survivors() {
    use super::test_support::{InlinePropertyTestEdge, inline_property_test_graph};
    let graph = inline_property_test_graph();
    let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(vid, label, 4)
        .unwrap();
    for i in 0..67u32 {
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyTestEdge::with_bytes(i, &i.to_le_bytes()),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let mut before = Vec::new();
    graph
        .visit_edges_with_inline_property(vid, label, OutEdgeOrder::Ascending, |slot, item| {
            assert_eq!(item.inline_property.bytes(), item.edge.target.to_le_bytes());
            before.push((slot.raw(), item.edge.target));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    let mut predicates = Vec::new();
    let removed = graph
        .remove_edge_matching_with_move(vid, label, |edge| {
            assert_eq!(edge.value, edge.target.to_le_bytes());
            predicates.push(edge.target);
            edge.target == 33
        })
        .unwrap()
        .unwrap();
    assert_eq!(predicates, (0..=33).collect::<Vec<_>>());
    assert_eq!(removed.removed.target, 33);
    assert_eq!(removed.removed.value, 33u32.to_le_bytes());
    assert!(removed.moves.is_empty());
    let mut after = Vec::new();
    graph
        .visit_edges_with_inline_property(vid, label, OutEdgeOrder::Descending, |slot, item| {
            assert_eq!(item.inline_property.bytes(), item.edge.target.to_le_bytes());
            after.push((slot.raw(), item.edge.target));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    assert_eq!(
        after,
        before
            .into_iter()
            .rev()
            .filter(|(_, target)| *target != 33)
            .collect::<Vec<_>>()
    );
}

#[test]
fn spill_matching_preserves_ordinals_and_stops_predicates_at_match() {
    exercise::<4>(false);
    exercise::<10>(false);
    exercise::<4>(true);
    exercise::<10>(true);
}
