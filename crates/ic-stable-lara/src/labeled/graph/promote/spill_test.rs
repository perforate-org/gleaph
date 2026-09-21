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
    let outer = retained_bypass::<N, _>(used, crate::test_support::vector_memory);
    outer
        .forward()
        .promote_bypass_to_bucket_mode(VertexId::from(0))
        .unwrap();
    outer
}
fn retained_bypass<const N: usize, M: Memory>(
    used: u32,
    mut m: impl FnMut() -> M,
) -> crate::labeled::bidirectional::DeferredBidirectionalLabeledLaraGraph<Edge<N>, M> {
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
    rows_for_owner(g, VertexId::from(0), label, order)
}
fn rows_for_owner<E: CsrEdgeTombstone, M: Memory>(
    g: &LabeledLaraGraph<E, M>,
    owner: VertexId,
    label: BucketLabelKey,
    order: super::super::OutEdgeOrder,
) -> Vec<(u32, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let flow = g
        .visit_edges_with_inline_property(owner, label, order, |slot, item| {
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
fn assert_tree<E: CsrEdgeTombstone, M: Memory>(
    g: &LabeledLaraGraph<E, M>,
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

fn check_public_capacity(holes: u32) {
    let outer = retained_bypass::<4, _>(
        crate::labeled::spill_run_store::MAX_ROWS,
        crate::test_support::vector_memory,
    );
    let g = outer.forward();
    let v = VertexId::from(0);
    let label = g.default_label();
    for i in 0..holes {
        assert_eq!(
            g.remove_edge_at_slot(v, label, i).unwrap().unwrap(),
            Edge(i)
        );
    }
    let old = g.vertices().get(v);
    assert!(old.is_default_edge_labeled());
    let mut expected = rows(g, label, super::super::OutEdgeOrder::Ascending);
    let count = g.edges().header().num_edges;
    outer
        .insert_forward_out_edge(
            v,
            label,
            Edge(99999),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    expected.push((old.stored_slots, 99999, Vec::new()));
    assert_tree(g, label, old.stored_slots + 1, &expected);
    assert_eq!(bucket(g, label).degree(), old.degree + 1);
    assert_eq!(g.edges().header().num_edges, count + 1);
    assert!(!g.vertices().get(v).is_default_edge_labeled());
}
#[test]
fn public_bypass_capacity_no_holes() {
    check_public_capacity(0);
}
#[test]
fn public_bypass_capacity_eight_holes() {
    check_public_capacity(8);
}
#[test]
fn public_bypass_capacity_sixteen_holes() {
    check_public_capacity(16);
}

#[test]
fn public_bypass_capacity_target_failure_preserves_source() {
    use crate::test_support::FailpointMemory;
    let memories: [FailpointMemory; 33] = std::array::from_fn(|_| FailpointMemory::new());
    let mut index = 0;
    let outer = retained_bypass::<4, _>(crate::labeled::spill_run_store::MAX_ROWS, || {
        let memory = memories[index].clone();
        index += 1;
        memory
    });
    let g = outer.forward();
    let v = VertexId::from(0);
    let label = g.default_label();
    for i in 0..16 {
        g.remove_edge_at_slot(v, label, i).unwrap().unwrap();
    }
    for _ in 0..14 {
        g.ltb().mint().unwrap();
    }
    let old = g.vertices().get(v);
    let run = g.edges().spill_mint(old.bypass_spill_run()).unwrap();
    let used = g.edges().spill_used_len(run);
    let before = rows(g, label, super::super::OutEdgeOrder::Ascending);
    let count = g.edges().header().num_edges;
    let free_before: u64 = g
        .edges()
        .free_byte_spans()
        .iter()
        .map(|span| span.len)
        .sum();
    let capacity = g.edges().header().elem_capacity;
    let counts = g.leaf_segment_counts_for_vid(v);
    let snapshot = |memory: &FailpointMemory| {
        let mut bytes = vec![0; memory.byte_len()];
        memory.read(0, &mut bytes);
        bytes
    };
    let source_roles = [0, 1, 2, 3, 4, 6, 7, 10, 13, 14];
    let source_bytes: Vec<_> = source_roles
        .iter()
        .map(|&role| snapshot(&memories[role]))
        .collect();
    let prefix = g.edges().initialized_slab_slots(&old).unwrap();
    let mut prefix_bytes = vec![0; prefix as usize * 4];
    g.edges()
        .read_slots_contiguous(old.base_slot_start(), &mut prefix_bytes);
    memories[15].fail_at_grow(memories[15].grow_count() + 1);
    let result = outer.insert_forward_out_edge(
        v,
        label,
        Edge(99999),
        super::super::EdgePlacementPolicy::Insertion,
    );
    assert!(
        matches!(
            result,
            Err(
                crate::labeled::bidirectional::DeferredBidirectionalLabeledError::Forward(
                    LabeledOperationError::Store(LaraOperationError::RebalanceFailed(
                        crate::GrowFailed {
                            current_size: 1,
                            delta: 1
                        }
                    ))
                )
            )
        ),
        "{result:?}"
    );
    assert_eq!(g.vertices().get(v), old);
    assert_eq!(g.edges().spill_used_len(run), used);
    assert_eq!(g.edges().header().num_edges, count);
    assert_eq!(
        rows(g, label, super::super::OutEdgeOrder::Ascending),
        before
    );
    assert_eq!(
        rows(g, label, super::super::OutEdgeOrder::Descending),
        before.iter().cloned().rev().collect::<Vec<_>>()
    );
    assert_eq!(g.leaf_segment_counts_for_vid(v), counts);
    for (&role, bytes) in source_roles.iter().zip(&source_bytes) {
        assert_eq!(
            &snapshot(&memories[role]),
            bytes,
            "source memory role {role}"
        );
    }
    let mut after_prefix = vec![0; prefix_bytes.len()];
    g.edges()
        .read_slots_contiguous(old.base_slot_start(), &mut after_prefix);
    assert_eq!(after_prefix, prefix_bytes);
    assert_eq!(g.ltb().tail_next(), 15);
    assert_eq!(g.ltb().free_count(), 1);
    let free_after: u64 = g
        .edges()
        .free_byte_spans()
        .iter()
        .map(|span| span.len)
        .sum();
    assert_eq!(
        free_after - free_before,
        g.edges().header().elem_capacity - capacity
    );
    memories[15].fail_never();
    outer
        .insert_forward_out_edge(
            v,
            label,
            Edge(99999),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    assert_eq!(bucket(g, label).degree(), old.degree + 1);
    assert_eq!(g.edges().header().num_edges, count + 1);
    let mut expected = before;
    expected.push((old.stored_slots, 99999, Vec::new()));
    assert_tree(g, label, old.stored_slots + 1, &expected);
    assert_eq!(g.leaf_segment_counts_for_vid(v).actual, 0);
}

#[test]
fn normal_and_bypass_relocation_retains_runs_and_values() {
    use super::super::test_support::{InlinePropertyTestEdge, inline_property_test_graph};
    for width in [8, 33] {
        let g = inline_property_test_graph();
        let owner = g
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        let label = BucketLabelKey::from_raw(2);
        g.ensure_label_bucket_inline_property_byte_width(owner, label, width)
            .unwrap();
        for i in 0..96 {
            if i == 8 {
                // Setup only: explicitly materialize a nonempty prefix before
                // creating the retained run and holes exercised by relocation.
                compact_run_into_prefix_with_growth_retry(&g, owner, 0, bucket(&g, label)).unwrap();
            }
            if i == 32 {
                let other = BucketLabelKey::from_raw(3);
                g.ensure_label_bucket_inline_property_byte_width(owner, other, width)
                    .unwrap();
                g.insert_edge_skip_leaf_cascade(
                    owner,
                    other,
                    InlinePropertyTestEdge::with_bytes(9000, &distinct_sidecar(9000, width)),
                    super::super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            }
            g.insert_edge_skip_leaf_cascade(
                owner,
                label,
                InlinePropertyTestEdge::with_bytes(i, &distinct_sidecar(i, width)),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        }
        for slot in [0, 3, 31, 63] {
            g.remove_edge_at_slot(owner, label, slot).unwrap().unwrap();
        }
        assert_eq!(
            bucket(&g, label).values_spill_run(),
            crate::traits::NO_SPILL_RUN
        );
        let other = BucketLabelKey::from_raw(4);
        g.ensure_label_bucket_inline_property_byte_width(owner, other, width)
            .unwrap();
        g.insert_edge_skip_leaf_cascade(
            owner,
            other,
            InlinePropertyTestEdge::with_bytes(9001, &distinct_sidecar(9001, width)),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
        g.insert_edge_skip_leaf_cascade(
            owner,
            label,
            InlinePropertyTestEdge::with_bytes(96, &distinct_sidecar(96, width)),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
        let peer = g
            .push_vertex(crate::labeled::record::LabeledVertex::default())
            .unwrap();
        for i in 0..67 {
            g.insert_edge_skip_leaf_cascade(
                peer,
                label,
                InlinePropertyTestEdge::with_bytes(10000 + i, &[]),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        }
        g.enable_default_edge_bypass(peer).unwrap();
        let source = bucket(&g, label);
        assert!(
            source.stored_slots() > 3,
            "holes must cover both prefix and run"
        );
        assert!(
            source.stored_slots() <= 31,
            "slot 31 must belong to the retained run"
        );
        assert_ne!(source.edge_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_ne!(source.values_spill_run(), crate::traits::NO_SPILL_RUN);
        assert_ne!(
            source.stored_slots(),
            source.inline_property_bytes_slab_slots()
        );
        // Occupy the current tail so relocation must allocate a disjoint destination.
        let _blocker = g.edges().allocate_span(1).unwrap();
        for streaming in [true, false] {
            if !streaming {
                g.insert_edge_skip_leaf_cascade(
                    owner,
                    label,
                    InlinePropertyTestEdge::with_bytes(97, &distinct_sidecar(97, width)),
                    super::super::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            }
            let source = bucket(&g, label);
            let edge_run = g.edges().spill_mint(source.edge_spill_run()).unwrap();
            let edge_used = g.edges().spill_used_len(edge_run);
            let value_run = g.values().spill_mint(source.values_spill_run()).unwrap();
            let value_used = g.values().spill_used_len(value_run);
            let peer_before = g.vertices().get(peer);
            let peer_prefix = g.edges().initialized_slab_slots(&peer_before).unwrap();
            let before = rows(&g, label, super::super::OutEdgeOrder::Ascending);
            let last = if streaming { 96 } else { 97 };
            let expected: Vec<_> = (0..=last)
                .filter(|slot| ![0, 3, 31, 63].contains(slot))
                .map(|slot| (slot, slot, distinct_sidecar(slot, width)))
                .collect();
            assert_eq!(before, expected, "exact source ordinals and sidecars");
            let count = g.edges().header().num_edges;
            let neighbors: Vec<_> = [3, 4]
                .into_iter()
                .map(|n| {
                    (
                        BucketLabelKey::from_raw(n),
                        rows(
                            &g,
                            BucketLabelKey::from_raw(n),
                            super::super::OutEdgeOrder::Ascending,
                        ),
                    )
                })
                .collect();
            for (neighbor, expected) in &neighbors {
                let target = if neighbor.raw() == 3 { 9000 } else { 9001 };
                assert_eq!(
                    *expected,
                    vec![(0, target, distinct_sidecar(target, width))]
                );
            }
            let old = g.labeled_leaf_physical_range(owner).unwrap();
            if streaming {
                g.relocate_labeled_leaf_physical_block(owner).unwrap();
            } else {
                g.rebalance_labeled_leaf_weighted_slide(owner).unwrap();
            }
            let (start, len) = g.labeled_leaf_physical_range(owner).unwrap();
            if streaming {
                assert!(
                    start >= old.0 + old.1 || start + len <= old.0,
                    "disjoint streaming source/destination"
                );
            }
            let after = bucket(&g, label);
            assert_eq!(
                after.with_edge_range(source.edge_start(), after.stored_slots()),
                source,
                "width={width}, streaming={streaming}: relocation must not fold either plane"
            );
            assert_eq!(g.edges().spill_used_len(edge_run), edge_used);
            assert_eq!(g.values().spill_used_len(value_run), value_used);
            assert_eq!(
                rows(&g, label, super::super::OutEdgeOrder::Ascending),
                before
            );
            assert_eq!(
                rows(&g, label, super::super::OutEdgeOrder::Descending),
                before.iter().cloned().rev().collect::<Vec<_>>()
            );
            assert!(
                after.edge_start() >= start
                    && after.edge_start() + u64::from(after.stored_slots()) <= start + len
            );
            let peer_after = g.vertices().get(peer);
            assert_eq!(
                peer_after.with_base_slot_start(peer_before.base_slot_start()),
                peer_before
            );
            assert!(
                peer_after.base_slot_start() >= start
                    && peer_after.base_slot_start() + u64::from(peer_prefix) <= start + len
            );
            if !streaming {
                assert_eq!(
                    (start, len),
                    old,
                    "buffered slide stays inside the same leaf"
                );
                assert_ne!(
                    peer_before.base_slot_start(),
                    peer_after.base_slot_start(),
                    "exercise a real move"
                );
                assert!(
                    peer_after.base_slot_start()
                        < peer_before.base_slot_start() + u64::from(peer_prefix)
                        && peer_before.base_slot_start()
                            < peer_after.base_slot_start() + u64::from(peer_prefix),
                    "exercise overlapping prefix copies"
                );
            }
            for (neighbor, expected) in &neighbors {
                assert_eq!(
                    rows(&g, *neighbor, super::super::OutEdgeOrder::Ascending),
                    *expected
                );
                assert_eq!(
                    rows(&g, *neighbor, super::super::OutEdgeOrder::Descending),
                    expected.iter().cloned().rev().collect::<Vec<_>>()
                );
            }
            let mut peer_rows = Vec::new();
            assert_eq!(
                g.visit_edges(
                    peer,
                    g.bypass_storage_label_for(&peer_before),
                    super::super::OutEdgeOrder::Ascending,
                    |slot, edge| {
                        peer_rows.push((slot.raw(), u32::from(edge.neighbor_vid())));
                        ControlFlow::<()>::Continue(())
                    }
                )
                .unwrap(),
                ControlFlow::Continue(())
            );
            assert_eq!(
                peer_rows,
                (0..67).map(|i| (i, 10000 + i)).collect::<Vec<_>>()
            );
            for free in g.edges().free_byte_spans() {
                assert!(
                    free.start_slot + free.len <= start || start + len <= free.start_slot,
                    "active leaf overlaps {free:?}"
                );
            }
            assert_eq!(g.edges().header().num_edges, count);
        }
    }
}

#[test]
fn public_capacity_bucket_reservation_failure_preserves_cached_peer() {
    use super::super::OutEdgeOrder::{Ascending, Descending};
    use crate::test_support::FailpointMemory;
    let memories: [FailpointMemory; 33] = std::array::from_fn(|_| FailpointMemory::new());
    let mut index = 0;
    let outer = retained_bypass::<4, _>(64, || {
        let memory = memories[index].clone();
        index += 1;
        memory
    });
    let g = outer.forward();
    let peer = VertexId::from(0);
    g.promote_bypass_to_bucket_mode(peer).unwrap();
    let owner = outer.push_vertex().unwrap();
    for i in 0..67 {
        outer
            .insert_forward_out_edge(
                owner,
                BucketLabelKey::from_raw(2),
                Edge(i),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    g.enable_default_edge_bypass(owner).unwrap();
    let label = g.bypass_storage_label_for(&g.vertices().get(owner));
    let prefix = g
        .edges()
        .initialized_slab_slots(&g.vertices().get(owner))
        .unwrap();
    for i in 67..prefix + crate::labeled::spill_run_store::MAX_ROWS {
        outer
            .insert_forward_out_edge(
                owner,
                label,
                Edge(i),
                super::super::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    for i in 0..16 {
        g.remove_edge_at_slot(owner, label, i).unwrap().unwrap();
    }
    // Consume allocatable two-descriptor spans until the real bucket allocator
    // reaches a page grow. The fixture owns these allocations, not the transition.
    assert_eq!(memories[1].size(), 5);
    memories[1].fail_at_grow(memories[1].grow_count() + 1);
    let mut hit = false;
    for _ in 0..=memories[1].byte_len() / LabelBucket::BYTES {
        match g.buckets().allocate_span(2) {
            Ok(_) => {}
            Err(error) => {
                assert!(
                    matches!(
                        error,
                        LaraOperationError::ResizeFailed(crate::GrowFailed {
                            current_size: 5,
                            delta: 5
                        })
                    ),
                    "{error:?}"
                );
                hit = true;
                break;
            }
        }
    }
    assert!(hit, "setup must reach a real bucket page boundary");
    let peer_lookup = || match g
        .find_bucket(peer, &g.vertices().get(peer), g.default_label())
        .unwrap()
    {
        BucketSearch::Found { slot, bucket } => (slot, bucket),
        _ => panic!("peer missing"),
    };
    let cached = peer_lookup();
    assert_eq!(peer_lookup(), cached);
    let peer_before = g.vertices().get(peer);
    let peer_rows = rows(g, g.default_label(), Ascending);
    let source = g.vertices().get(owner);
    let raw_prefix = |start: u64, slots: u32| {
        let mut bytes = vec![0; slots as usize * Edge::<4>::BYTES];
        g.edges().read_slots_contiguous_bytes(start, &mut bytes);
        bytes
    };
    let source_prefix = g.edges().initialized_slab_slots(&source).unwrap();
    let source_bytes = raw_prefix(source.base_slot_start(), source_prefix);
    let peer_bytes = raw_prefix(cached.1.edge_start(), cached.1.stored_slots());
    let before = rows_for_owner(g, owner, label, Ascending);
    let count = g.edges().header().num_edges;
    let leaf = g.leaf_segment_counts_for_vid(owner);
    let snapshot = |role: usize| {
        let mut bytes = vec![0; memories[role].byte_len()];
        memories[role].read(0, &mut bytes);
        bytes
    };
    let roles = [0, 1, 4, 6, 7, 10, 13, 14];
    let saved: Vec<_> = roles.iter().map(|&role| snapshot(role)).collect();
    let free_before: u64 = g
        .edges()
        .free_byte_spans()
        .iter()
        .map(|span| span.len)
        .sum();
    let cap = g.edges().header().elem_capacity;
    assert_eq!(g.ltb().tail_next(), 0);
    memories[1].fail_at_grow(memories[1].grow_count() + 1);
    let error = outer
        .insert_forward_out_edge(
            owner,
            label,
            Edge(99999),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            crate::labeled::bidirectional::DeferredBidirectionalLabeledError::Forward(
                LabeledOperationError::Store(LaraOperationError::ResizeFailed(crate::GrowFailed {
                    current_size: 5,
                    delta: 5
                }))
            )
        ),
        "{error:?}"
    );
    assert_eq!(
        g.ltb().tail_next(),
        2,
        "both native edge leaves were prepared before bucket reservation"
    );
    assert_eq!(g.ltb().free_count(), 2);
    let free_after: u64 = g
        .edges()
        .free_byte_spans()
        .iter()
        .map(|span| span.len)
        .sum();
    assert_eq!(
        free_after - free_before,
        g.edges().header().elem_capacity - cap
    );
    for (&role, bytes) in roles.iter().zip(saved.iter()) {
        assert_eq!(&snapshot(role), bytes, "canonical memory role {role}");
    }
    assert_eq!(
        raw_prefix(source.base_slot_start(), source_prefix),
        source_bytes
    );
    assert_eq!(
        raw_prefix(cached.1.edge_start(), cached.1.stored_slots()),
        peer_bytes
    );
    assert_eq!(g.vertices().get(owner), source);
    assert_eq!(g.vertices().get(peer), peer_before);
    assert_eq!(peer_lookup(), cached);
    assert_eq!(g.leaf_segment_counts_for_vid(owner), leaf);
    assert_eq!(g.edges().header().num_edges, count);
    assert_eq!(rows_for_owner(g, owner, label, Ascending), before);
    assert_eq!(
        rows_for_owner(g, owner, label, Descending),
        before.iter().cloned().rev().collect::<Vec<_>>()
    );
    assert_eq!(rows(g, g.default_label(), Ascending), peer_rows);
    memories[1].fail_never();
    outer
        .insert_forward_out_edge(
            owner,
            label,
            Edge(99999),
            super::super::EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    let now = peer_lookup();
    assert_eq!(now.1, cached.1);
    assert_eq!(now.0, g.vertices().get(peer).base_slot_start());
    assert_ne!(now.0, cached.0, "peer descriptor actually moved");
    assert_eq!(rows(g, g.default_label(), Ascending), peer_rows);
    assert_eq!(
        rows(g, g.default_label(), Descending),
        peer_rows.iter().cloned().rev().collect::<Vec<_>>()
    );
    let expected: Vec<_> = (0..=source.stored_slots)
        .map(|slot| {
            if slot < 16 {
                (slot, u32::MAX, Vec::new())
            } else if slot == source.stored_slots {
                (slot, 99999, Vec::new())
            } else {
                (slot, slot, Vec::new())
            }
        })
        .collect();
    assert_eq!(rows_for_owner(g, owner, label, Ascending), expected);
    assert_eq!(
        rows_for_owner(g, owner, label, Descending),
        expected.iter().cloned().rev().collect::<Vec<_>>()
    );
    assert_eq!(g.edges().header().num_edges, count + 1);
    assert_eq!(
        g.leaf_segment_counts_for_vid(owner).actual,
        leaf.actual - i64::from(source.degree)
    );
}
