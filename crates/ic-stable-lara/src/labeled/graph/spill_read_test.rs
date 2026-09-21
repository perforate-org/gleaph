use super::*;
use crate::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
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
    for match_case in 0..5 {
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
        let label = BucketLabelKey::from_raw(2);
        for i in 0..67 {
            graph
                .insert_edge_skip_leaf_cascade(vid, label, Row(i), EdgePlacementPolicy::Insertion)
                .unwrap();
        }
        // Produce a run as a labeled bucket, then use the canonical conversion that
        // transfers its run and prefix without moving rows.
        let label = if bypass {
            graph.enable_default_edge_bypass(vid).unwrap();
            graph.default_label()
        } else {
            label
        };
        let vertex = graph.vertices().get(vid);
        assert_eq!(vertex.is_default_edge_labeled(), bypass);
        let (raw_run, prefix) = if bypass {
            (vertex.bypass_spill_run(), 0)
        } else {
            let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
            let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
            (bucket.edge_spill_run(), bucket.stored_slots_raw())
        };
        assert_ne!(
            raw_run,
            crate::traits::NO_SPILL_RUN,
            "fixture must own a spill run"
        );
        let run = graph.edges().spill_mint(raw_run).unwrap();
        let used = graph.edges().spill_used_len(run);
        assert!(used > 32, "fixture must cross a spill chunk");
        let prefix = if bypass {
            vertex.stored_degree() - used
        } else {
            prefix
        };
        assert!(prefix < 31);
        let target = [0, prefix, 33, 65, 999][match_case];
        if bypass {
            let invalid = vertex.with_stored_slots(used - 1);
            assert!(matches!(
                graph.edges().initialized_slab_slots(&invalid),
                Err(crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow)
            ));
        }
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut seen = Vec::new();
            let _ = graph
                .visit_edges(vid, label, order, |slot, edge| {
                    seen.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            let mut expected: Vec<_> = (0..67).map(|i| (i, i)).collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(seen, expected);
        }
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
            assert_eq!(
                seen, expected,
                "width={N} bypass={bypass} target={target} order={order:?}"
            );
            let mut with_empty_values = Vec::new();
            let flow = graph
                .visit_edges_with_inline_property(vid, label, order, |slot, item| {
                    assert_eq!(item.inline_property.width(), 0);
                    assert!(item.inline_property.bytes().is_empty());
                    with_empty_values.push((slot.raw(), item.edge.0));
                    if with_empty_values.len() == 3 {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .unwrap();
            assert_eq!(flow, ControlFlow::Break(()));
            assert_eq!(with_empty_values, expected[..3]);
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
    let vertex = graph.vertices().get(vid);
    let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
    let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
    assert_ne!(bucket.edge_spill_run(), crate::traits::NO_SPILL_RUN);
    let run = graph.edges().spill_mint(bucket.edge_spill_run()).unwrap();
    assert!(graph.edges().spill_used_len(run) > 32);
    assert!(bucket.stored_slots_raw() <= 33, "match must reach spill");
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

fn append_lifecycle<const N: usize>() {
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
    let label = BucketLabelKey::from_raw(2);
    for i in 0..67 {
        graph
            .insert_edge_skip_leaf_cascade(vid, label, Row(i), EdgePlacementPolicy::Insertion)
            .unwrap();
    }
    graph.enable_default_edge_bypass(vid).unwrap();
    let label = graph.default_label();
    let vertex = graph.vertices().get(vid);
    let run = graph.edges().spill_mint(vertex.bypass_spill_run()).unwrap();
    let initial_used = graph.edges().spill_used_len(run);
    assert!(initial_used > 32);
    let prefix = graph.edges().initialized_slab_slots(&vertex).unwrap();
    let initial_class = initial_used.next_power_of_two();
    let mut dead = Vec::new();
    for target in 67..prefix + initial_class + 3 {
        if target == 71 {
            for slot in [0, prefix, 31, 32, 66] {
                assert_eq!(
                    graph
                        .remove_edge_at_slot(vid, label, slot)
                        .unwrap()
                        .unwrap()
                        .0,
                    slot
                );
                dead.push(slot);
            }
        }
        graph
            .insert_edge(vid, label, Row(target), EdgePlacementPolicy::Insertion)
            .unwrap();
        let vertex = graph.vertices().get(vid);
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(
            vertex.bypass_spill_run(),
            run.get(),
            "tail run grows in place"
        );
        assert_eq!(
            graph.edges().spill_used_len(run),
            initial_used + target - 66
        );
        assert_eq!(
            graph.edges().initialized_slab_slots(&vertex).unwrap(),
            prefix
        );
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut seen = Vec::new();
            let _ = graph
                .visit_edges(vid, label, order, |slot, edge| {
                    seen.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            let mut expected: Vec<_> = (0..=target)
                .filter(|i| !dead.contains(i))
                .map(|i| (i, i))
                .collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(
                seen, expected,
                "width={N}, appended={target}, order={order:?}"
            );
        }
    }
    assert!(graph.edges().spill_used_len(run) > initial_class);
    let end = prefix + crate::labeled::spill_run_store::MAX_ROWS;
    for target in prefix + initial_class + 3..end {
        graph
            .insert_edge(vid, label, Row(target), EdgePlacementPolicy::Insertion)
            .unwrap();
    }
    let before = graph.vertices().get(vid);
    let before_count = graph.edges().header().num_edges;
    let error = graph
        .insert_edge(vid, label, Row(end), EdgePlacementPolicy::Insertion)
        .unwrap_err();
    assert!(matches!(
        error,
        LabeledOperationError::Store(
            crate::lara::operation_error::LaraOperationError::SpillNeedsPromotion
        )
    ));
    let after = graph.vertices().get(vid);
    assert_eq!(after.degree(), before.degree());
    assert_eq!(after.stored_degree(), before.stored_degree());
    assert_eq!(after.bypass_spill_run(), run.get());
    assert_eq!(
        graph.edges().spill_used_len(run),
        crate::labeled::spill_run_store::MAX_ROWS
    );
    assert_eq!(graph.edges().header().num_edges, before_count);
    assert_eq!(
        graph.edges().initialized_slab_slots(&after).unwrap(),
        prefix
    );
    let mut seen = Vec::new();
    let _ = graph
        .visit_edges(vid, label, OutEdgeOrder::Ascending, |slot, edge| {
            seen.push((slot.raw(), edge.0));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    assert_eq!(
        seen,
        (0..end)
            .filter(|i| !dead.contains(i))
            .map(|i| (i, i))
            .collect::<Vec<_>>()
    );
}
#[test]
fn retained_run_append_preserves_prefix_and_ordinals_four_bytes() {
    append_lifecycle::<4>();
}
#[test]
fn retained_run_append_preserves_prefix_and_ordinals_ten_bytes() {
    append_lifecycle::<10>();
}

#[test]
fn spill_matching_preserves_ordinals_and_stops_predicates_at_match() {
    exercise::<4>(false);
    exercise::<10>(false);
    exercise::<4>(true);
    exercise::<10>(true);
}
