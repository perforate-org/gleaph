use super::*;
use crate::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use std::ops::ControlFlow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row<const N: usize>(u32, u16, u32);
impl<const N: usize> CsrEdge for Row<N> {
    const BYTES: usize = N;
    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 0, 0)
    }
    fn write_to(&self, bytes: &mut [u8]) {
        bytes.fill(0);
        bytes[..4].copy_from_slice(&self.0.to_le_bytes());
    }
    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid), self.1, self.2)
    }
    fn with_label_id(self, label_id: u16) -> Self {
        Self(self.0, label_id, self.2)
    }
    fn with_slot_index(self, slot: u32) -> Self {
        Self(self.0, self.1, slot)
    }
    fn edge_slot_index_raw(&self) -> u32 {
        self.2
    }
}
impl<const N: usize> CsrEdgeTombstone for Row<N> {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL), 0, 0)
    }
}
fn assert_all_label_rows<const N: usize>(
    graph: &LabeledLaraGraph<Row<N>, crate::VectorMemory>,
    vid: VertexId,
    live: &[u32],
) {
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let mut expected: Vec<_> = live
            .iter()
            .map(|target| (*target, graph.default_label().raw()))
            .collect();
        if order == OutEdgeOrder::Descending {
            expected.reverse();
        }
        let mut seen = Vec::new();
        let flow = graph
            .visit_all_labels_with_inline_property(vid, order, |edge| {
                seen.push((edge.0, edge.1));
                ControlFlow::<u32>::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(seen, expected, "all-label width={N} order={order:?}");
        if expected.is_empty() {
            continue;
        }
        seen.clear();
        let flow = graph
            .visit_all_labels_with_inline_property(vid, order, |edge| {
                seen.push((edge.0, edge.1));
                if seen.len() == 3 {
                    ControlFlow::Break(0xfeed_u32)
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Break(0xfeed));
        assert_eq!(seen, expected[..3], "no suffix callbacks");
    }
}

fn exercise<const N: usize>(bypass: bool) {
    for match_case in 0..5 {
        let m = crate::test_support::vector_memory;
        let graph = LabeledLaraGraph::<Row<N>, _>::new(
            crate::labeled::graph::GraphMemories {
                vertices: m(),
                buckets: m(),
                bucket_free_spans: m(),
                bucket_free_span_by_start: m(),
                edge_counts: m(),
                edges: m(),
                edge_log: m(),
                edge_span_meta: m(),
                edge_free_spans: m(),
                edge_free_span_by_start: m(),
                inline_property_bytes_slab: m(),
                value_free_spans: m(),
                value_free_span_by_start: m(),
                inline_property_bytes_log: m(),
                value_blobs: m(),
                ltb: m(),
            },
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::from_raw(2);
        for i in 0..67 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    Row(i, 0, 0),
                    EdgePlacementPolicy::Insertion,
                )
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
            assert!(prefix > 0);
            assert_all_label_rows(&graph, vid, &(0..67).collect::<Vec<_>>());
        }
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
        if bypass {
            assert_all_label_rows(&graph, vid, &live);
        }
        for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
            let mut seen = Vec::new();
            let flow = graph
                .visit_edges(vid, label, order, |slot, edge| {
                    seen.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            assert_eq!(flow, ControlFlow::Continue(()));
            let mut expected: Vec<_> = live.iter().map(|i| (*i, *i)).collect();
            if order == OutEdgeOrder::Descending {
                expected.reverse();
            }
            assert_eq!(
                seen, expected,
                "width={N} bypass={bypass} target={target} order={order:?}"
            );
            let mut materialized = Vec::new();
            graph
                .visit_edges_for_label(vid, label, order, |edge| {
                    materialized.push((edge.edge_slot_index_raw(), edge.0, edge.1));
                })
                .unwrap();
            assert_eq!(
                materialized,
                expected
                    .iter()
                    .map(|(slot, target)| (*slot, *target, label.raw()))
                    .collect::<Vec<_>>()
            );
            for window in [
                crate::traverse::TraversalWindow::new(0, Some(0)),
                crate::traverse::TraversalWindow::new(prefix.saturating_sub(1), Some(4)),
                crate::traverse::TraversalWindow::new(31, Some(4)),
                crate::traverse::TraversalWindow::new(65, Some(4)),
                crate::traverse::TraversalWindow::new(67, None),
                crate::traverse::TraversalWindow::new(u32::MAX, Some(u32::MAX)),
            ] {
                let mut slots: Vec<_> = (0..67).collect();
                if order == OutEdgeOrder::Descending {
                    slots.reverse();
                }
                let want: Vec<_> = slots
                    .into_iter()
                    .skip(window.offset as usize)
                    .take(window.limit.unwrap_or(u32::MAX) as usize)
                    .filter(|slot| ![31, 32, 66].contains(slot))
                    .map(|slot| (slot, Row::<N>(slot, label.raw(), slot)))
                    .collect();
                let mut actual = Vec::new();
                let flow = graph
                    .visit_edges_window(vid, label, order, window, |slot, edge| {
                        actual.push((slot.raw(), edge));
                        ControlFlow::<()>::Continue(())
                    })
                    .unwrap();
                assert_eq!(flow, ControlFlow::Continue(()));
                assert_eq!(
                    actual, want,
                    "width={N} bypass={bypass} order={order:?} window={window:?}"
                );
            }
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
        let flow = graph
            .visit_edges(vid, label, OutEdgeOrder::Ascending, |slot, edge| {
                survivors.push((slot.raw(), edge.0));
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        assert_eq!(
            survivors,
            live.into_iter()
                .filter(|i| *i != target)
                .map(|i| (i, i))
                .collect::<Vec<_>>()
        );
        if bypass {
            if match_case == 4 {
                for (slot, target) in &survivors {
                    assert_eq!(
                        graph
                            .remove_edge_at_slot(vid, label, *slot)
                            .unwrap()
                            .unwrap()
                            .0,
                        *target
                    );
                }
                survivors.clear();
                assert_eq!(graph.vertices().get(vid).degree(), 0);
                assert_eq!(graph.edges().spill_used_len(run), used);
            }
            let edge_count = graph.edges().header().num_edges;
            graph.promote_bypass_to_bucket_mode(vid).unwrap();
            assert_eq!(graph.edges().header().num_edges, edge_count);
            assert_eq!(graph.edges().spill_used_len(run), used);
            let vertex = graph.vertices().get(vid);
            assert!(!vertex.is_default_edge_labeled());
            assert_eq!(vertex.spill_run(), crate::traits::NO_SPILL_RUN);
            let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
            let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
            assert_eq!(bucket.edge_spill_run(), raw_run);
            assert_eq!(bucket.stored_slots_raw(), prefix);
            let mut after = Vec::new();
            let flow = graph
                .visit_edges(vid, label, OutEdgeOrder::Ascending, |slot, edge| {
                    after.push((slot.raw(), edge.0));
                    ControlFlow::<()>::Continue(())
                })
                .unwrap();
            assert_eq!(flow, ControlFlow::Continue(()));
            assert_eq!(after, survivors);
            assert_all_label_rows(
                &graph,
                vid,
                &survivors
                    .iter()
                    .map(|(_, target)| *target)
                    .collect::<Vec<_>>(),
            );
        }
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
    let flow = graph
        .visit_edges_with_inline_property(vid, label, OutEdgeOrder::Ascending, |slot, item| {
            assert_eq!(item.inline_property.bytes(), item.edge.target.to_le_bytes());
            before.push((slot.raw(), item.edge.target));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    assert_eq!(flow, ControlFlow::Continue(()));
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
    let flow = graph
        .visit_edges_with_inline_property(vid, label, OutEdgeOrder::Descending, |slot, item| {
            assert_eq!(item.inline_property.bytes(), item.edge.target.to_le_bytes());
            after.push((slot.raw(), item.edge.target));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    assert_eq!(flow, ControlFlow::Continue(()));
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
        crate::labeled::graph::GraphMemories {
            vertices: m(),
            buckets: m(),
            bucket_free_spans: m(),
            bucket_free_span_by_start: m(),
            edge_counts: m(),
            edges: m(),
            edge_log: m(),
            edge_span_meta: m(),
            edge_free_spans: m(),
            edge_free_span_by_start: m(),
            inline_property_bytes_slab: m(),
            value_free_spans: m(),
            value_free_span_by_start: m(),
            inline_property_bytes_log: m(),
            value_blobs: m(),
            ltb: m(),
        },
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..67 {
        graph
            .insert_edge_skip_leaf_cascade(vid, label, Row(i, 0, 0), EdgePlacementPolicy::Insertion)
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
            .insert_edge(
                vid,
                label,
                Row(target, 0, 0),
                EdgePlacementPolicy::Insertion,
            )
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
            .insert_edge(
                vid,
                label,
                Row(target, 0, 0),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let before = graph.vertices().get(vid);
    let before_count = graph.edges().header().num_edges;
    // The store still rejects before mutation; the public four-byte caller now
    // consumes this signal through a separately tested prepared tree transition.
    let error = graph
        .edges()
        .insert_edge(graph.vertices(), vid, Row(end, 0, 0))
        .unwrap_err();
    assert!(matches!(
        error,
        crate::lara::operation_error::LaraOperationError::SpillNeedsPromotion
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
}
#[test]
fn retained_run_all_label_four_bytes() {
    exercise::<4>(true);
}
#[test]
fn retained_run_all_label_ten_bytes() {
    exercise::<10>(true);
}

fn point_state_graph() -> (
    LabeledLaraGraph<Row<4>, crate::VectorMemory>,
    [crate::VectorMemory; 16],
) {
    let m = test_support::labeled_memories();
    let graph = LabeledLaraGraph::new(
        crate::labeled::graph::GraphMemories {
            vertices: m[0].clone(),
            buckets: m[1].clone(),
            bucket_free_spans: m[2].clone(),
            bucket_free_span_by_start: m[3].clone(),
            edge_counts: m[4].clone(),
            edges: m[5].clone(),
            edge_log: m[6].clone(),
            edge_span_meta: m[7].clone(),
            edge_free_spans: m[8].clone(),
            edge_free_span_by_start: m[9].clone(),
            inline_property_bytes_slab: m[10].clone(),
            value_free_spans: m[11].clone(),
            value_free_span_by_start: m[12].clone(),
            inline_property_bytes_log: m[13].clone(),
            value_blobs: m[14].clone(),
            ltb: m[15].clone(),
        },
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    (graph, m)
}

fn assert_point_states(
    graph: &LabeledLaraGraph<Row<4>, crate::VectorMemory>,
    owner: VertexId,
    label: BucketLabelKey,
    expected: &[Option<u32>],
) {
    for (slot, target) in expected.iter().copied().enumerate() {
        let slot = slot as u32;
        let state = graph
            .row_plane()
            .read_row_state(owner, label, BucketEntryPosition::new(slot))
            .unwrap();
        match (state, target) {
            (EdgeSlotState::Live(edge), Some(target)) => {
                assert_eq!(edge, Row(target, label.raw(), slot))
            }
            (EdgeSlotState::Tombstone, None) => (),
            (actual, expected) => panic!("slot={slot}: {actual:?} != {expected:?}"),
        }
        assert_eq!(
            graph.edge_exists_at_slot(owner, label, slot).unwrap(),
            target.is_some()
        );
    }
    for slot in [expected.len() as u32, u32::MAX] {
        assert!(matches!(
            graph
                .row_plane()
                .read_row_state(owner, label, BucketEntryPosition::new(slot))
                .unwrap(),
            EdgeSlotState::Missing
        ));
        assert!(!graph.edge_exists_at_slot(owner, label, slot).unwrap());
    }
    assert!(matches!(
        graph
            .row_plane()
            .read_row_state(
                owner,
                BucketLabelKey::from_raw(99),
                BucketEntryPosition::new(0)
            )
            .unwrap(),
        EdgeSlotState::Missing
    ));
    assert!(
        !graph
            .edge_exists_at_slot(owner, BucketLabelKey::from_raw(99), 0)
            .unwrap()
    );
    // Selected reads call the private bucket reader directly; both orders must retain
    // physical positions, not dense live ordinals. Missing slots emit nothing.
    let mut selected: Vec<_> = (0..expected.len() as u32)
        .map(BucketEntryPosition::new)
        .collect();
    selected.push(BucketEntryPosition::new(u32::MAX));
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let mut actual = Vec::new();
        let flow = graph
            .visit_edges_at(owner, label, &selected, order, |slot, edge| {
                actual.push((slot.raw(), edge));
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        let mut live: Vec<_> = expected
            .iter()
            .enumerate()
            .filter_map(|(slot, target)| {
                target.map(|target| (slot as u32, Row(target, label.raw(), slot as u32)))
            })
            .collect();
        if order == OutEdgeOrder::Descending {
            live.reverse();
        }
        assert_eq!(actual, live);
    }
}

#[test]
fn point_state_tiny_preserves_all_memories() {
    let (graph, memories) = point_state_graph();
    let snapshot = || {
        memories
            .iter()
            .map(|m| m.borrow().clone())
            .collect::<Vec<_>>()
    };
    let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    let mut expected = Vec::new();
    for slot in 0..3 {
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                label,
                Row(1000 + slot, 0, 0),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        expected.push(Some(1000 + slot));
    }
    graph.remove_edge_at_slot(owner, label, 1).unwrap().unwrap();
    expected[1] = None;
    let bucket_slot = graph
        .find_bucket_slot(&graph.vertices().get(owner), label)
        .unwrap()
        .unwrap();
    assert!(
        graph
            .buckets()
            .read_label_bucket_slot(bucket_slot)
            .unwrap()
            .is_tiny_mode()
    );
    let before = snapshot();
    assert_point_states(&graph, owner, label, &expected);
    assert_eq!(snapshot(), before);
}

#[test]
fn point_state_slab_and_retained_bypass_preserve_all_memories() {
    // Seed without tiny holes: promotion out of tiny may pack its inline slots.
    let (graph, memories) = point_state_graph();
    let snapshot = || {
        memories
            .iter()
            .map(|m| m.borrow().clone())
            .collect::<Vec<_>>()
    };
    let label = BucketLabelKey::from_raw(2);
    let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
    let mut expected = Vec::new();
    for slot in 0..67 {
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                label,
                Row(2000 + slot, 0, 0),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        expected.push(Some(2000 + slot));
    }
    let bucket_slot = graph
        .find_bucket_slot(&graph.vertices().get(owner), label)
        .unwrap()
        .unwrap();
    let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
    assert!(!bucket.is_tree_mode() && !bucket.is_tiny_mode());
    let run = graph.edges().spill_mint(bucket.edge_spill_run()).unwrap();
    let prefix = graph.edges().initialized_slab_slots(&bucket).unwrap();
    let used = graph.edges().spill_used_len(run);
    assert!(prefix > 0 && used > 32);
    assert_eq!(prefix + used, 67);
    for slot in [0, prefix, 66] {
        assert_eq!(
            graph
                .remove_edge_at_slot(owner, label, slot)
                .unwrap()
                .unwrap()
                .0,
            2000 + slot
        );
        expected[slot as usize] = None;
    }
    let before = snapshot();
    assert_point_states(&graph, owner, label, &expected);
    assert_eq!(snapshot(), before);

    graph.enable_default_edge_bypass(owner).unwrap();
    let label = graph.default_label();
    let vertex = graph.vertices().get(owner);
    assert!(vertex.is_default_edge_labeled());
    assert_eq!(vertex.bypass_spill_run(), run.get());
    assert_eq!(
        graph.edges().initialized_slab_slots(&vertex).unwrap(),
        prefix
    );
    assert_eq!(graph.edges().spill_used_len(run), used);
    let before = snapshot();
    let spill_slot = prefix + 1;
    let state = graph
        .row_plane()
        .read_row_state(owner, label, BucketEntryPosition::new(spill_slot))
        .unwrap();
    assert!(
        matches!(state, EdgeSlotState::Live(edge) if edge == Row(2000 + spill_slot, label.raw(), spill_slot)),
        "retained bypass run slot={spill_slot}: {state:?}"
    );
    assert_point_states(&graph, owner, label, &expected);
    assert_eq!(
        graph
            .count_preceding_live_edges_with_neighbor(
                owner,
                label,
                BucketEntryPosition::new(66),
                VertexId::from(2065)
            )
            .unwrap(),
        1
    );
    assert_eq!(snapshot(), before);

    // Malformed total extent is not an absent row. Keep the run real and intact.
    graph
        .vertices()
        .set(owner, &vertex.with_stored_slots(used - 1));
    let corrupt = snapshot();
    assert!(matches!(
        graph
            .row_plane()
            .read_row_state(owner, label, BucketEntryPosition::new(0)),
        Err(LabeledOperationError::Store(
            crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow
        ))
    ));
    assert!(matches!(
        graph.edge_exists_at_slot(owner, label, 0),
        Err(LabeledOperationError::Store(
            crate::lara::operation_error::LaraOperationError::CollectAllocationOverflow
        ))
    ));
    assert_eq!(snapshot(), corrupt);
    graph.vertices().set(owner, &vertex);
    assert_eq!(snapshot(), before);
}

#[test]
fn point_state_tree_native_boundaries_and_errors_preserve_all_memories() {
    let (graph, memories) = point_state_graph();
    let snapshot = || {
        memories
            .iter()
            .map(|m| m.borrow().clone())
            .collect::<Vec<_>>()
    };
    let owner = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    let mut expected = Vec::new();
    for slot in 0..T_PROMOTE {
        graph
            .insert_edge_skip_leaf_cascade(
                owner,
                label,
                Row(1000 + slot, 0, 0),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        expected.push(Some(1000 + slot));
    }
    for slot in [0, 3, 511, T_PROMOTE - 1] {
        graph
            .remove_edge_at_slot(owner, label, slot)
            .unwrap()
            .unwrap();
        expected[slot as usize] = None;
    }
    promote::promote_bucket_to_tree_mode(&graph, owner, label).unwrap();
    graph
        .insert_edge(
            owner,
            label,
            Row(1000 + T_PROMOTE, 0, 0),
            EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    expected.push(Some(1000 + T_PROMOTE));
    let bucket_slot = graph
        .find_bucket_slot(&graph.vertices().get(owner), label)
        .unwrap()
        .unwrap();
    let bucket = graph.buckets().read_label_bucket_slot(bucket_slot).unwrap();
    assert!(bucket.is_tree_mode());
    assert_eq!(bucket.stored_slots(), T_PROMOTE + 1);
    assert_eq!(bucket.degree(), T_PROMOTE - 3);
    let before = snapshot();
    assert_point_states(&graph, owner, label, &expected);
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let mut native = Vec::new();
        let flow = graph
            .visit_slot_rows(owner, label, order, |row| {
                let slot = row.slot;
                let edge = row.into_edge();
                native.push((slot.raw(), edge.0));
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        assert_eq!(flow, ControlFlow::Continue(()));
        let mut want: Vec<_> = expected
            .iter()
            .enumerate()
            .map(|(slot, target)| (slot as u32, target.unwrap_or(u32::MAX)))
            .collect();
        if order == OutEdgeOrder::Descending {
            want.reverse();
        }
        assert_eq!(native, want, "native visitors remain tombstone-inclusive");
    }
    assert_eq!(snapshot(), before);
    // A minted-envelope error must not become Missing or a root id masquerading as a target.
    let mut root = [0; 4];
    graph.edges().read_slot_bytes(bucket.edge_slot(), &mut root);
    let unminted = graph.ltb().tail_next();
    graph
        .edges()
        .write_slot_bytes(bucket.edge_start(), &unminted.to_le_bytes())
        .unwrap();
    let corrupt = snapshot();
    assert!(
        matches!(graph.row_plane().read_row_state(owner, label, BucketEntryPosition::new(0)), Err(LabeledOperationError::LtbBlock(crate::labeled::ltb_raw_block_store::BlockError::NotMinted { id })) if id == unminted)
    );
    assert!(
        matches!(graph.edge_exists_at_slot(owner, label, 0), Err(LabeledOperationError::LtbBlock(crate::labeled::ltb_raw_block_store::BlockError::NotMinted { id })) if id == unminted)
    );
    assert_eq!(snapshot(), corrupt);
    graph
        .edges()
        .write_slot_bytes(bucket.edge_start(), &root)
        .unwrap();
    assert_eq!(snapshot(), before);
}

#[test]
fn point_state_counterpart_parallel_occurrences_use_live_native_ranks() {
    use crate::labeled::bidirectional::{
        Orientation,
        counterpart::{
            CanonicalEdgeOccurrence, CounterpartLookupError, counterpart_of,
            counterpart_of_verified,
        },
    };
    let (forward, fm) = point_state_graph();
    let (reverse, rm) = point_state_graph();
    let label = BucketLabelKey::directed_from_index(2);
    for (graph, owner, target, hole) in [(&forward, 0, 1, 3), (&reverse, 1, 0, 7)] {
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        for _ in 0..T_PROMOTE {
            graph
                .insert_edge_skip_leaf_cascade(
                    VertexId::from(owner),
                    label,
                    Row(target, 0, 0),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        graph
            .remove_edge_at_slot(VertexId::from(owner), label, hole)
            .unwrap()
            .unwrap();
        promote::promote_bucket_to_tree_mode(graph, VertexId::from(owner), label).unwrap();
    }
    let snapshot = || {
        fm.iter()
            .chain(rm.iter())
            .map(|m| m.borrow().clone())
            .collect::<Vec<_>>()
    };
    let before = snapshot();
    for lookup in [counterpart_of, counterpart_of_verified] {
        for (forward_slot, reverse_slot) in [(2, 2), (4, 3), (7, 6), (8, 8), (1023, 1023)] {
            let source = CanonicalEdgeOccurrence {
                orientation: Orientation::Forward,
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                slot_index: BucketEntryPosition::new(forward_slot),
            };
            let mate = CanonicalEdgeOccurrence {
                orientation: Orientation::Reverse,
                owner_vertex_id: VertexId::from(1),
                label_id: label,
                slot_index: BucketEntryPosition::new(reverse_slot),
            };
            assert_eq!(lookup(&forward, &reverse, source).unwrap(), mate);
            assert_eq!(lookup(&forward, &reverse, mate).unwrap(), source);
        }
        for (orientation, owner, dead) in
            [(Orientation::Forward, 0, 3), (Orientation::Reverse, 1, 7)]
        {
            let source = CanonicalEdgeOccurrence {
                orientation,
                owner_vertex_id: VertexId::from(owner),
                label_id: label,
                slot_index: BucketEntryPosition::new(dead),
            };
            assert_eq!(
                lookup(&forward, &reverse, source),
                Err(CounterpartLookupError::SourceNotLive(source))
            );
            let missing = CanonicalEdgeOccurrence {
                slot_index: BucketEntryPosition::new(T_PROMOTE),
                ..source
            };
            assert_eq!(
                lookup(&forward, &reverse, missing),
                Err(CounterpartLookupError::SourceNotFound(missing))
            );
        }
    }
    assert_eq!(snapshot(), before);
}
