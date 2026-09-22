//! Diagnostic prerequisite witnesses; no fabricated tree descriptors or tombstones.
use super::*;
use crate::labeled::graph::test_support::{
    InlinePropertyTestEdge, inline_property_test_graph, retained_compaction_rows,
    retained_compaction_value, tree_compaction_fixture,
};
use crate::labeled::graph::{BucketSearch, T_DEMOTE, T_PROMOTE, bucket_physical_resident_slots};
use crate::labeled::ltb_raw_block_store::{BlockHeader, BlockKind};
use crate::labeled::record::LabelBucket;
use crate::test_support::vector_memory;
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

type Graph = LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>;
type Rows = Vec<(u32, u32, Vec<u8>)>;
const HOLES: [u32; 4] = [0, 3, 511, T_PROMOTE - 1];
fn label() -> BucketLabelKey {
    BucketLabelKey::from_raw(3)
}
fn vid() -> VertexId {
    VertexId::from(0)
}
fn bucket(graph: &Graph) -> LabelBucket {
    match graph
        .find_bucket(vid(), &graph.vertices().get(vid()), label())
        .unwrap()
    {
        BucketSearch::Found { bucket, .. } => bucket,
        _ => panic!("missing tree fixture"),
    }
}
fn expected_rows(width: u16) -> Rows {
    (0..T_PROMOTE)
        .map(|slot| {
            let target = if HOLES.contains(&slot) {
                40_000 + slot
            } else {
                1000 + slot
            };
            (slot, target, retained_compaction_value(target, width))
        })
        .collect()
}
fn fixture(width: u16) -> Graph {
    let graph = tree_compaction_fixture(width);
    // Fill the promotion-transcribed holes through the existing real writer,
    // so the later reuse can select only the newly deleted slot.
    for slot in HOLES {
        let target = 40_000 + slot;
        let location = graph
            .insert_edge_skip_leaf_cascade_with_location(
                vid(),
                label(),
                InlinePropertyTestEdge::with_bytes(
                    target,
                    &retained_compaction_value(target, width),
                ),
                EdgePlacementPolicy::Unordered,
            )
            .unwrap()
            .unwrap();
        assert_eq!(location.logical_slot, slot);
    }
    assert_eq!(
        (bucket(&graph).degree(), bucket(&graph).stored_slots()),
        (T_PROMOTE, T_PROMOTE)
    );
    assert_rows(&graph, &expected_rows(width));
    graph
}
fn roots(graph: &Graph) -> Vec<u8> {
    let b = bucket(graph);
    let mut bytes = vec![0; bucket_physical_resident_slots(&b) as usize * 4];
    graph
        .edges()
        .read_slots_contiguous_bytes(b.edge_start(), &mut bytes);
    bytes
}
fn blocks(graph: &Graph) -> Vec<(BlockHeader, [u8; 4096])> {
    (0..graph.ltb().tail_next())
        .map(|id| {
            let mut bytes = [0; 4096];
            graph.ltb().read_payload(id, &mut bytes).unwrap();
            (graph.ltb().read_block_header(id), bytes)
        })
        .collect()
}
fn neighbors(graph: &Graph) -> String {
    let snapshots: Vec<_> = [(0, 2), (1, 2)]
        .into_iter()
        .map(|(owner, raw)| {
            let owner = VertexId::from(owner);
            let vertex = graph.vertices().get(owner);
            let b = match graph
                .find_bucket(owner, &vertex, BucketLabelKey::from_raw(raw))
                .unwrap()
            {
                BucketSearch::Found { bucket, .. } => bucket,
                _ => panic!("neighbor missing"),
            };
            let mut bytes = vec![0; bucket_physical_resident_slots(&b) as usize * 4];
            graph
                .edges()
                .read_slots_contiguous_bytes(b.edge_start(), &mut bytes);
            (
                vertex,
                b,
                bytes,
                retained_compaction_rows(
                    graph,
                    owner,
                    BucketLabelKey::from_raw(raw),
                    OutEdgeOrder::Ascending,
                ),
            )
        })
        .collect();
    format!("{snapshots:?}")
}
fn snapshot(graph: &Graph) -> String {
    let ltb = graph.ltb();
    let counts = graph
        .edges()
        .counts_store()
        .get(graph.edges().vertex_segment_counts_index(vid()).unwrap());
    format!(
        "{:?}",
        (
            bucket(graph),
            roots(graph),
            blocks(graph),
            neighbors(graph),
            graph.edges().header(),
            counts,
            (
                ltb.tail_next(),
                ltb.free_head(),
                ltb.free_count(),
                ltb.block_capacity()
            ),
            graph.edges().free_span_store().spans()
        )
    )
}
fn assert_rows(graph: &Graph, expected: &Rows) {
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let actual = retained_compaction_rows(graph, vid(), label(), order);
        let want = match order {
            OutEdgeOrder::Ascending => expected.clone(),
            OutEdgeOrder::Descending => expected.iter().rev().cloned().collect(),
        };
        assert_eq!(actual, want, "raw native rows include dead properties");
    }
    let b = bucket(graph);
    assert!(b.is_tree_mode());
    assert_eq!(b.stored_slots() as usize, expected.len());
    assert_eq!(
        b.degree() as usize,
        expected.iter().filter(|r| r.1 != u32::MAX).count()
    );
    assert_eq!(graph.edges().header().num_edges, u64::from(b.degree()) + 17);
    let counts = graph
        .edges()
        .counts_store()
        .get(graph.edges().vertex_segment_counts_index(vid()).unwrap());
    assert_eq!(counts.actual, 17, "tree rows never enter slab-live counts");
    let root = roots(graph);
    let ids: std::collections::BTreeSet<_> = root
        .chunks_exact(4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    assert_eq!(ids.len(), root.len() / 4);
    for (index, bytes) in root.chunks_exact(4).enumerate() {
        let header = graph
            .ltb()
            .read_block_header(u32::from_le_bytes(bytes.try_into().unwrap()));
        assert_eq!(header.level, 0);
        let edge_leaves = expected.len().div_ceil(1024);
        if index < edge_leaves {
            assert_eq!(header.kind, BlockKind::Edge);
            let dead = expected
                .iter()
                .skip(index * 1024)
                .take(1024)
                .filter(|r| r.1 == u32::MAX)
                .count();
            assert_eq!(usize::from(header.tombstone_count), dead);
        } else {
            assert_eq!(header.kind, BlockKind::InlineProperty);
            assert_eq!(header.tombstone_count, 0);
        }
    }
    for free in graph.edges().free_span_store().spans() {
        let start = b.edge_start();
        let end = start + u64::from(bucket_physical_resident_slots(&b));
        assert!(
            end <= free.start_slot || free.start_slot + free.len <= start,
            "root must stay owned"
        );
    }
}

#[test]
fn property_tree_delete_public_preserves_native_rows() {
    let graph = DeferredBidirectionalLabeledLaraGraph {
        forward: fixture(33),
        reverse: inline_property_test_graph(),
        maintenance: BidirectionalMaintenanceQueue::new(vector_memory()),
        config: DeferredConfig::default(),
    };
    let f = graph.forward();
    let before = bucket(f);
    let root = roots(f);
    let mut backing = blocks(f);
    let peer = neighbors(f);
    let mut expected = expected_rows(33);
    let mut sidecars: BTreeMap<_, _> = expected
        .iter()
        .map(|(slot, target, _)| (*slot, (*target, u64::from(*target) + 0x4567_0000_0000)))
        .collect();
    let removed = graph
        .remove_forward_edge_at_slot_with_move(vid(), label(), 125)
        .unwrap()
        .unwrap();
    assert_eq!(
        (removed.removed.target, removed.removed.slot_index),
        (1125, 125)
    );
    assert!(removed.moves.is_empty());
    sidecars.remove(&125).unwrap();
    // Current owner changes only edge bytes. Dead properties are retained, not zeroed.
    expected[125].1 = u32::MAX;
    assert_rows(f, &expected);
    assert_eq!(bucket(f), before.with_degree_field(T_PROMOTE - 1));
    assert_eq!(roots(f), root);
    let leaf = u32::from_le_bytes(root[0..4].try_into().unwrap()) as usize;
    backing[leaf].0.tombstone_count += 1;
    backing[leaf].1[125 * 4..126 * 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        blocks(f),
        backing,
        "only marker and edge-leaf tombstone count change"
    );
    assert_eq!(neighbors(f), peer);
    for (slot, target, _) in expected.iter().filter(|r| r.1 != u32::MAX) {
        assert_eq!(
            sidecars.get(slot),
            Some(&(*target, u64::from(*target) + 0x4567_0000_0000))
        );
    }
    let unchanged = snapshot(f);
    let mut outcomes = Vec::new();
    for slot in [125, T_PROMOTE, u32::MAX] {
        let removal = graph
            .remove_forward_edge_at_slot_with_move(vid(), label(), slot)
            .unwrap();
        outcomes.push((slot, removal.map(|r| (r.removed.target, r.moves.len()))));
        assert_eq!(snapshot(f), unchanged);
    }
    assert_eq!(graph.maintenance_queue_len(), 0);
    eprintln!("TREE_DELETE_PUBLIC_NOOPS {outcomes:?}");
    assert_eq!(
        outcomes,
        vec![(125, None), (T_PROMOTE, None), (u32::MAX, None)]
    );
}

#[test]
fn property_tree_delete_owner_policy_matrix() {
    for width in [8, 33] {
        for policy in [
            EdgePlacementPolicy::Insertion,
            EdgePlacementPolicy::Unordered,
        ] {
            let graph = fixture(width);
            let peer = neighbors(&graph);
            let mut expected = expected_rows(width);
            let deleted = graph
                .remove_edge_at_slot_with_move(vid(), label(), 125)
                .unwrap()
                .unwrap();
            assert_eq!(deleted.removed.target, 1125);
            assert!(deleted.moves.is_empty());
            expected[125].1 = u32::MAX;
            assert_rows(&graph, &expected);
            let unchanged = snapshot(&graph);
            let invalid = InlinePropertyTestEdge::with_bytes(90_000, &[0x7b]);
            let err = graph
                .insert_edge_skip_leaf_cascade_with_location(vid(), label(), invalid, policy)
                .unwrap_err();
            assert!(
                matches!(err, LabeledOperationError::InlinePropertyBytesWidthMismatch { bucket_width, edge_inline_property_width: 1 } if bucket_width == width)
            );
            assert_eq!(
                snapshot(&graph),
                unchanged,
                "rejected insert preserves tree and neighbors"
            );
            assert!(
                graph
                    .remove_edge_at_slot(vid(), label(), T_PROMOTE)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(snapshot(&graph), unchanged);
            // Test state idempotency independently of the public no-op result contract.
            // The owner currently returns Some(tombstone); the public test requires None.
            let repeat = graph.remove_edge_at_slot(vid(), label(), 125).unwrap();
            assert!(repeat.is_none() || repeat.unwrap().is_tombstone_edge());
            assert_eq!(snapshot(&graph), unchanged);
            let target = 90_000 + u32::from(width);
            let value = retained_compaction_value(target, width);
            let location = graph
                .insert_edge_skip_leaf_cascade_with_location(
                    vid(),
                    label(),
                    InlinePropertyTestEdge::with_bytes(target, &value),
                    policy,
                )
                .unwrap()
                .unwrap();
            let slot = if policy == EdgePlacementPolicy::Insertion {
                T_PROMOTE
            } else {
                125
            };
            assert_eq!(location.logical_slot, slot);
            if policy == EdgePlacementPolicy::Insertion {
                expected.push((slot, target, value));
            } else {
                expected[slot as usize] = (slot, target, value);
            }
            assert_rows(&graph, &expected);
            assert_eq!(neighbors(&graph), peer);
            eprintln!("TREE_DELETE_MATRIX width={width} policy={policy:?} slot={slot} complete");
        }
    }
}

#[test]
fn property_tree_delete_reachable_demotion_preserves_identities() {
    let graph = DeferredBidirectionalLabeledLaraGraph {
        forward: fixture(33),
        reverse: inline_property_test_graph(),
        maintenance: BidirectionalMaintenanceQueue::new(vector_memory()),
        config: DeferredConfig::default(),
    };
    let mut sidecars: BTreeMap<_, _> = expected_rows(33)
        .into_iter()
        .map(|(slot, target, value)| (slot, (target, value)))
        .collect();
    for slot in 0..T_PROMOTE - T_DEMOTE {
        let before = bucket(graph.forward());
        let root_before = roots(graph.forward());
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            graph.remove_forward_edge_at_slot_with_move(vid(), label(), slot)
        }));
        if outcome.is_err() {
            let after = bucket(graph.forward());
            eprintln!(
                "TREE_DELETE_DEMOTION slot={slot} degree={}->{} extent={}->{} live={} root_changed={} tail={} free={}",
                before.degree(),
                after.degree(),
                before.stored_slots(),
                after.stored_slots(),
                graph.forward().edges().header().num_edges,
                roots(graph.forward()) != root_before,
                graph.forward().ltb().tail_next(),
                graph.forward().ltb().free_count()
            );
        }
        let removed = outcome
            .expect("public deletion must not panic")
            .unwrap()
            .unwrap();
        assert_eq!(removed.removed.target, sidecars.remove(&slot).unwrap().0);
        for moved in removed.moves {
            assert_eq!(moved.label_id, label());
            let identity = sidecars
                .remove(&moved.old_slot_index)
                .expect("reported source identity");
            assert!(sidecars.insert(moved.new_slot_index, identity).is_none());
        }
        let live: BTreeMap<_, _> =
            retained_compaction_rows(graph.forward(), vid(), label(), OutEdgeOrder::Ascending)
                .into_iter()
                .filter(|r| r.1 != u32::MAX)
                .map(|(slot, target, value)| (slot, (target, value)))
                .collect();
        assert_eq!(
            live, sidecars,
            "surviving identities change only through returned events"
        );
    }
}
