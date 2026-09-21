use super::*;
use crate::{
    VectorMemory,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use std::ops::ControlFlow;

#[derive(Clone, Debug, PartialEq)]
struct Edge<const N: usize>(u32);
impl<const N: usize> CsrEdge for Edge<N> {
    const BYTES: usize = N;
    fn read_from(b: &[u8]) -> Self {
        Self(u32::from_le_bytes(b[..4].try_into().unwrap()))
    }
    fn write_to(&self, b: &mut [u8]) {
        b.fill(0);
        b[..4].copy_from_slice(&self.0.to_le_bytes());
    }
    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }
    fn with_neighbor_vid(&self, v: VertexId) -> Self {
        Self(u32::from(v))
    }
}
impl<const N: usize> CsrEdgeTombstone for Edge<N> {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}
fn retained<const N: usize>(
    used: u32,
) -> crate::labeled::bidirectional::DeferredBidirectionalLabeledLaraGraph<Edge<N>, VectorMemory> {
    let m = crate::test_support::vector_memory;
    let outer = crate::labeled::bidirectional::DeferredBidirectionalLabeledLaraGraph::new(
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
        m(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    let v = outer.push_vertex().unwrap();
    for i in 0..67 {
        outer
            .insert_forward_out_edge(
                v,
                BucketLabelKey::from_raw(2),
                Edge(i),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let g = outer.forward();
    g.enable_default_edge_bypass(v).unwrap();
    let prefix = g
        .edges()
        .initialized_slab_slots(&g.vertices().get(v))
        .unwrap();
    assert!(prefix > 0);
    for i in 67..prefix + used {
        outer
            .insert_forward_out_edge(
                v,
                g.default_label(),
                Edge(i),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    g.promote_bypass_to_bucket_mode(v).unwrap();
    outer
}
fn bucket<E: CsrEdgeTombstone, M: Memory>(
    g: &LabeledLaraGraph<E, M>,
    label: BucketLabelKey,
) -> LabelBucket {
    match g
        .find_bucket(
            VertexId::from(0),
            &g.vertices().get(VertexId::from(0)),
            label,
        )
        .unwrap()
    {
        BucketSearch::Found { bucket, .. } => bucket,
        _ => panic!("missing bucket"),
    }
}
fn rows<E: CsrEdgeTombstone, M: Memory>(
    g: &LabeledLaraGraph<E, M>,
    label: BucketLabelKey,
    order: super::super::OutEdgeOrder,
) -> Vec<(u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let flow = g
        .visit_edges_with_inline_property(VertexId::from(0), label, order, |slot, item| {
            out.push((
                slot.raw(),
                u32::from(item.edge.neighbor_vid()),
                item.inline_property.bytes().to_vec(),
            ));
            ControlFlow::<()>::Continue(())
        })
        .unwrap();
    assert_eq!(flow, ControlFlow::Continue(()));
    out
}
// Native tree visitors retain tombstone rows; verify the whole slot stream and headers.
fn assert_tree<E: CsrEdgeTombstone>(
    g: &LabeledLaraGraph<E, VectorMemory>,
    label: BucketLabelKey,
    extent: u32,
    live: &[(u32, u32, Vec<u8>)],
) {
    let b = bucket(g, label);
    let width = usize::from(b.inline_property_byte_width());
    let expected: Vec<_> = (0..extent)
        .map(|slot| {
            live.iter().find(|row| row.0 == slot).cloned().unwrap_or((
                slot,
                u32::MAX,
                vec![0; width],
            ))
        })
        .collect();
    assert_eq!(
        rows(g, label, super::super::OutEdgeOrder::Ascending),
        expected
    );
    assert_eq!(
        rows(g, label, super::super::OutEdgeOrder::Descending),
        expected.iter().cloned().rev().collect::<Vec<_>>()
    );
    for block in 0..extent.div_ceil(BLOCK_B as u32) {
        let mut bytes = [0; 4];
        g.edges()
            .read_slot_bytes(b.edge_start() + u64::from(block), &mut bytes);
        let header = g.ltb().read_block_header(u32::from_le_bytes(bytes));
        let start = block * BLOCK_B as u32;
        let end = (start + BLOCK_B as u32).min(extent);
        let holes = expected[start as usize..end as usize]
            .iter()
            .filter(|row| row.1 == u32::MAX)
            .count();
        assert_eq!(usize::from(header.tombstone_count), holes);
        assert_eq!(
            header.kind,
            crate::labeled::ltb_raw_block_store::BlockKind::Edge
        );
    }
}
fn check_promotion(deletes: u32) {
    let outer = retained::<4>(crate::labeled::spill_run_store::MAX_ROWS);
    let g = outer.forward();
    let v = VertexId::from(0);
    let label = g.default_label();
    for i in 0..deletes {
        assert_eq!(g.remove_edge_at_slot(v, label, i).unwrap().unwrap().0, i);
    }
    let old = bucket(&g, label);
    let run = g.edges().spill_mint(old.edge_spill_run()).unwrap();
    let extent = old.stored_slots() + g.edges().spill_used_len(run);
    let count = g.edges().header().num_edges;
    let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
    assert_eq!(
        before.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
        (deletes..extent).map(|i| (i, i)).collect::<Vec<_>>()
    );
    promote_bucket_to_tree_mode(&g, v, label).unwrap();
    let after = bucket(&g, label);
    assert!(after.is_tree_mode());
    assert_eq!(after.stored_slots(), extent);
    assert_eq!(after.degree(), old.degree());
    assert_eq!(g.edges().header().num_edges, count);
    assert_tree(g, label, extent, &before);
}
#[test]
fn promotion_retained_no_holes() {
    check_promotion(0);
}
#[test]
fn promotion_retained_eight_holes() {
    check_promotion(8);
}
#[test]
fn promotion_retained_sixteen_holes() {
    check_promotion(16);
}
#[test]
fn promotion_retained_below_cap_rejects_without_mutation() {
    let outer = retained::<4>(128);
    let g = outer.forward();
    let label = g.default_label();
    let old = bucket(&g, label);
    let run = g.edges().spill_mint(old.edge_spill_run()).unwrap();
    let used = g.edges().spill_used_len(run);
    let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
    let count = g.edges().header().num_edges;
    let result = promote_bucket_to_tree_mode(&g, VertexId::from(0), label);
    assert!(
        matches!(result,Err(LabeledOperationError::AllocSpaceCapReached{current_alloc_space,cap:T_PROMOTE,mode:BucketMode::Slab}) if current_alloc_space==old.stored_slots()+used),
        "{result:?}"
    );
    assert_eq!(bucket(&g, label), old);
    assert_eq!(g.edges().spill_used_len(run), used);
    assert_eq!(g.edges().header().num_edges, count);
    assert_eq!(
        rows(&g, label, super::super::OutEdgeOrder::Ascending),
        before
    );
}
#[test]
fn promotion_retained_wide_rejects_without_mutation() {
    let outer = retained::<10>(crate::labeled::spill_run_store::MAX_ROWS);
    let g = outer.forward();
    let label = g.default_label();
    let old = bucket(&g, label);
    let run = g.edges().spill_mint(old.edge_spill_run()).unwrap();
    let used = g.edges().spill_used_len(run);
    let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
    assert!(matches!(
        promote_bucket_to_tree_mode(&g, VertexId::from(0), label),
        Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
            actual: 10,
            expected: 4
        })
    ));
    assert_eq!(bucket(&g, label), old);
    assert_eq!(g.edges().spill_used_len(run), used);
    assert_eq!(
        rows(&g, label, super::super::OutEdgeOrder::Ascending),
        before
    );
}
fn distinct_sidecar(target: u32, width: u16) -> Vec<u8> {
    let identity = (u64::from(target) + 1).to_le_bytes();
    (0..usize::from(width))
        .map(|i| identity[i % 8] ^ ((i / 8) as u8).wrapping_mul(53))
        .collect()
}

#[test]
fn promotion_retained_sidecars_keep_slot_identity() {
    use super::super::test_support::{InlinePropertyTestEdge, inline_property_test_graph};
    for (width, holes) in [(8, false), (8, true), (33, false), (33, true)] {
        let g = inline_property_test_graph();
        let v = g
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        g.ensure_label_bucket_inline_property_byte_width(v, label, width)
            .unwrap();
        for i in 0..T_PROMOTE {
            if i == 32 {
                let other = BucketLabelKey::from_raw(3);
                g.ensure_label_bucket_inline_property_byte_width(v, other, width)
                    .unwrap();
                g.insert_edge_skip_leaf_cascade(
                    v,
                    other,
                    InlinePropertyTestEdge::with_bytes(99999, &distinct_sidecar(99999, width)),
                    super::super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            }
            let value = distinct_sidecar(i, width);
            g.insert_edge_skip_leaf_cascade(
                v,
                label,
                InlinePropertyTestEdge::with_bytes(i, &value),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        }
        assert_ne!(
            bucket(&g, label).values_spill_run(),
            crate::traits::NO_SPILL_RUN
        );
        if holes {
            for i in [0, 3, 31, 32, 511, 1023] {
                g.remove_edge_at_slot(v, label, i).unwrap().unwrap();
            }
        }
        let old = bucket(&g, label);
        assert!(!old.is_tree_mode());
        assert_ne!(old.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_eq!(
            old.values_spill_run() == crate::traits::NO_SPILL_RUN,
            holes,
            "deletion folds values; retained values-run cases here have no holes"
        );
        assert_ne!(
            old.stored_slots(),
            old.inline_property_bytes_slab_slots(),
            "independent source prefixes"
        );
        let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
        assert_eq!(
            rows(&g, label, super::super::OutEdgeOrder::Descending),
            before.iter().cloned().rev().collect::<Vec<_>>()
        );
        let other = BucketLabelKey::from_raw(3);
        let neighbor = vec![(0, 99999, distinct_sidecar(99999, width))];
        for order in [
            super::super::OutEdgeOrder::Ascending,
            super::super::OutEdgeOrder::Descending,
        ] {
            assert_eq!(rows(&g, other, order), neighbor);
        }
        for (slot, target, value) in &before {
            assert_eq!(slot, target);
            assert_eq!(*value, distinct_sidecar(*target, width));
        }
        promote_bucket_to_tree_mode(&g, v, label).unwrap();
        let after = bucket(&g, label);
        assert!(after.is_tree_mode());
        assert_eq!(after.degree(), old.degree());
        assert_eq!(after.stored_slots(), T_PROMOTE);
        assert_tree(&g, label, T_PROMOTE, &before);
        for order in [
            super::super::OutEdgeOrder::Ascending,
            super::super::OutEdgeOrder::Descending,
        ] {
            assert_eq!(rows(&g, other, order), neighbor);
        }
    }
}

#[test]
fn promotion_retained_allocation_failure_releases_target_and_preserves_source() {
    use super::super::test_support::InlinePropertyTestEdge;
    use crate::test_support::failpoint_labeled_memories;
    let mems = failpoint_labeled_memories();
    let g = LabeledLaraGraph::<InlinePropertyTestEdge, _>::new(
        mems[0].clone(),
        mems[1].clone(),
        mems[2].clone(),
        mems[3].clone(),
        mems[4].clone(),
        mems[5].clone(),
        mems[6].clone(),
        mems[7].clone(),
        mems[8].clone(),
        mems[9].clone(),
        mems[10].clone(),
        mems[11].clone(),
        mems[12].clone(),
        mems[13].clone(),
        mems[14].clone(),
        mems[15].clone(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    let v = g
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .unwrap();
    let label = BucketLabelKey::from_raw(2);
    g.ensure_label_bucket_inline_property_byte_width(v, label, 8)
        .unwrap();
    for i in 0..T_PROMOTE {
        if i == 32 {
            let other = BucketLabelKey::from_raw(3);
            g.ensure_label_bucket_inline_property_byte_width(v, other, 8)
                .unwrap();
            g.insert_edge_skip_leaf_cascade(
                v,
                other,
                InlinePropertyTestEdge::with_bytes(99999, &[77; 8]),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        }
        g.insert_edge_skip_leaf_cascade(
            v,
            label,
            InlinePropertyTestEdge::with_bytes(i, &u64::from(i).to_le_bytes()),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    }
    // Leave exactly two block slots on the first 64-KiB page: property then edge
    // transcription succeeds, and the second property leaf requires a failing grow.
    for _ in 0..13 {
        g.ltb().mint().unwrap();
    }
    let old = bucket(&g, label);
    assert_ne!(old.edge_spill_run(), crate::traits::NO_SPILL_RUN);
    assert_ne!(old.values_spill_run(), crate::traits::NO_SPILL_RUN);
    let vertex = g.vertices().get(v);
    let count = g.edges().header().num_edges;
    let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
    let free_before: u64 = g.edges().free_byte_spans().iter().map(|s| s.len).sum();
    let cap_before = g.edges().header().elem_capacity;
    mems[15].fail_at_grow(mems[15].grow_count() + 1);
    let result = promote_bucket_to_tree_mode(&g, v, label);
    assert!(
        matches!(
            result,
            Err(LabeledOperationError::Store(
                LaraOperationError::RebalanceFailed(crate::GrowFailed {
                    current_size: 1,
                    delta: 1
                })
            ))
        ),
        "{result:?}"
    );
    assert_eq!(
        g.ltb().tail_next(),
        15,
        "two target blocks were actually minted before failure"
    );
    assert_eq!(g.ltb().free_count(), 2, "every target block was released");
    assert_eq!(bucket(&g, label), old);
    assert_eq!(g.vertices().get(v), vertex);
    assert_eq!(g.edges().header().num_edges, count);
    assert_eq!(
        rows(&g, label, super::super::OutEdgeOrder::Ascending),
        before
    );
    assert_eq!(
        rows(&g, label, super::super::OutEdgeOrder::Descending),
        before.iter().cloned().rev().collect::<Vec<_>>()
    );
    let free_after: u64 = g.edges().free_byte_spans().iter().map(|s| s.len).sum();
    assert_eq!(
        free_after - free_before,
        g.edges().header().elem_capacity - cap_before,
        "target root span was returned in full"
    );
    mems[15].fail_never();
    promote_bucket_to_tree_mode(&g, v, label).unwrap();
}

fn check_cascade_ownership<const N: usize>() {
    let m = crate::test_support::vector_memory;
    let g = LabeledLaraGraph::new(
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
    let v = g
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .unwrap();
    for i in 0..67 {
        g.insert_edge_skip_leaf_cascade(
            v,
            BucketLabelKey::from_raw(2),
            Edge::<N>(i),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    }
    g.enable_default_edge_bypass(v).unwrap();
    let prefix = g
        .edges()
        .initialized_slab_slots(&g.vertices().get(v))
        .unwrap();
    for i in 67..prefix + crate::labeled::spill_run_store::MAX_ROWS {
        let before = g.vertices().get(v);
        g.insert_edge(
            v,
            g.default_label(),
            Edge::<N>(i),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
        let after = g.vertices().get(v);
        let start = after.base_slot_start();
        if i == 67 {
            assert_eq!(
                after.bypass_spill_run(),
                before.bypass_spill_run(),
                "relocation must retain the run"
            );
        }
        let run = g.edges().spill_mint(after.bypass_spill_run()).unwrap();
        assert_eq!(g.edges().spill_used_len(run), i + 1 - prefix);
        assert_eq!(
            g.edges().initialized_slab_slots(&after).unwrap(),
            prefix,
            "append {i}"
        );
        for span in g.edges().free_byte_spans() {
            assert!(
                start + u64::from(prefix) <= span.start_slot || span.start_slot + span.len <= start,
                "append {i}, width {N}: active [{start}, {}) free={span:?}; before={before:?} after={after:?}",
                start + u64::from(prefix)
            );
        }
        let actual = rows(&g, g.default_label(), super::super::OutEdgeOrder::Ascending);
        assert_eq!(
            actual.iter().map(|r| (r.0, r.1)).collect::<Vec<_>>(),
            (0..=i).map(|j| (j, j)).collect::<Vec<_>>(),
            "append {i}"
        );
    }
    let retained = g.vertices().get(v);
    let run = retained.bypass_spill_run();
    let extent = retained.stored_slots;
    let count = g.edges().header().num_edges;
    let allocation = g.edges().allocate_span(2).unwrap();
    assert!(
        allocation + 2 <= retained.base_slot_start()
            || retained.base_slot_start() + u64::from(prefix) <= allocation
    );
    g.edges()
        .write_slots_contiguous(allocation, &vec![0xab; 2 * N])
        .unwrap();
    g.promote_bypass_to_bucket_mode(v).unwrap();
    let b = bucket(&g, g.default_label());
    assert_eq!(b.stored_slots(), prefix);
    assert_eq!(b.edge_spill_run(), run);
    let expected: Vec<_> = (0..extent).map(|i| (i, i, Vec::new())).collect();
    assert_eq!(
        rows(&g, g.default_label(), super::super::OutEdgeOrder::Ascending),
        expected
    );
    assert_eq!(
        rows(
            &g,
            g.default_label(),
            super::super::OutEdgeOrder::Descending
        ),
        expected.iter().cloned().rev().collect::<Vec<_>>()
    );
    if N == 4 {
        promote_bucket_to_tree_mode(&g, v, g.default_label()).unwrap();
        assert_tree(&g, g.default_label(), extent, &expected);
    } else {
        assert!(matches!(
            promote_bucket_to_tree_mode(&g, v, g.default_label()),
            Err(LabeledOperationError::TreeModeEdgeWidthUnsupported {
                actual: 10,
                expected: 4
            })
        ));
        assert_eq!(bucket(&g, g.default_label()), b);
    }
    let b = bucket(&g, g.default_label());
    let physical = if N == 4 {
        extent.div_ceil(BLOCK_B as u32)
    } else {
        prefix
    };
    for free in g.edges().free_byte_spans() {
        assert!(
            free.start_slot + free.len <= b.edge_start()
                || b.edge_start() + u64::from(physical) <= free.start_slot
        );
    }
    let next = g.edges().allocate_span(2).unwrap();
    assert!(next + 2 <= b.edge_start() || b.edge_start() + u64::from(physical) <= next);
    g.edges()
        .write_slots_contiguous(next, &vec![0xcd; 2 * N])
        .unwrap();
    assert_eq!(
        rows(&g, g.default_label(), super::super::OutEdgeOrder::Ascending),
        expected
    );
    assert_eq!(g.edges().header().num_edges, count);
}
#[test]
fn cascade_retained_prefix_ownership_four_bytes() {
    check_cascade_ownership::<4>();
}
#[test]
fn cascade_retained_prefix_ownership_ten_bytes() {
    check_cascade_ownership::<10>();
}
