//! Plan 0337 tree-bucket tombstone header-count measurement benches.
//!
//! Measures the first tree-regime OFFSET baseline (depth-2 tree bucket at
//! 1M+1 stored slots, seeded through the production deepen path) and the
//! bench-scoped Level-1 counting walk (root-entry walk + per-block header
//! tombstone count, skipping fully-dead blocks without payload reads)
//! against it, plus the slab-regime re-anchor at the post-ADR-0088 extent
//! cap 4,096.
//!
//! Declared fixture decisions (before any run): tombstones are laid down as
//! a contiguous dead prefix (oldest slots removed first) — the "aged runs"
//! regime the Level-1 per-block count targets; the grid is reduced to
//! densities {50%, 87.5%} × OFFSET {first live slot, first live + 65,536}
//! (the 0% point exercises the tree dense fast path, not the sparse
//! overshoot regime; the intermediate ~10K point is dropped for canbench
//! budget). Production read paths are untouched: the counting walk lives
//! only here.

use super::bench::OFFSET_WINDOW_LIMIT;
use crate::labeled::{
    BucketLabelKey, LabeledVertex, OutEdgeOrder,
    graph::LabeledLaraGraph,
    graph::traverse::{BucketEntryPosition, LabeledTraversalRequest},
};
use crate::traits::{CsrEdge, CsrEdgeTombstone};
use crate::traverse::{Traversal, TraversalWindow};
use crate::{VertexId, test_support::labeled_lara_memories};
use canbench_rs::{bench, bench_fn};
use std::hint::black_box;

/// Tree fixture edge (4 bytes — tree mode's required width).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TreeBenchEdge {
    target: u32,
}

impl CsrEdge for TreeBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.target)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            target: u32::from(vid),
        }
    }
}

impl CsrEdgeTombstone for TreeBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
        }
    }
}

const TREE_SEED_SLOTS: u32 = 1_048_577; // 1M+1: the last insert deepens to depth 2
const TREE_DENSITY_875: u32 = 917_504; // 87.5% of 1,048,576
const TREE_DENSITY_50: u32 = 524_288; // 50%
const OFFSET_STEP: u32 = 65_536;
const SLAB_REANCHOR_EXTENT: u32 = 4_096;
const SLAB_REANCHOR_STRIDE: u32 = 8;
const SLAB_REANCHOR_OFFSET: u32 = 960;

struct TreeFixture {
    graph: LabeledLaraGraph<TreeBenchEdge, crate::VectorMemory>,
    vid: VertexId,
    label: BucketLabelKey,
    bucket_slot: u64,
}

fn tree_bench_graph(elem_capacity: u64) -> LabeledLaraGraph<TreeBenchEdge, crate::VectorMemory> {
    let (
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
    ) = labeled_lara_memories();
    LabeledLaraGraph::new(
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        BucketLabelKey::from_raw(1),
    )
    .expect("tree bench graph")
}

/// Seeds 1M+1 tree slots through the production insert path; the final
/// insert crosses `R_max` and deepens the bucket to depth 2 (Plan 0325
/// precedent — 2^30 seeding is out of canbench budget).
fn build_tree_fixture(tombstones: u32) -> TreeFixture {
    let graph = tree_bench_graph(1 << 21);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    for i in 0..TREE_SEED_SLOTS {
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                TreeBenchEdge {
                    target: 10_000_000 + i,
                },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap_or_else(|e| panic!("tree seed failed at i={i}: {e:?}"));
    }
    // Contiguous dead prefix: remove the oldest `tombstones` slots in place
    // (tree-mode remove funnels through the production dispatch; each remove
    // increments the owning block's header tombstone count).
    for slot in 0..tombstones {
        graph
            .remove_edge_at_slot(vid, label, slot)
            .expect("tree tombstone remove");
    }
    let vertex = graph.vertices().get(vid);
    let bucket_slot = match graph.find_bucket(vid, &vertex, label).expect("find") {
        crate::labeled::graph::BucketSearch::Found { slot, .. } => slot,
        _ => panic!("tree bucket missing"),
    };
    let bucket = graph
        .buckets()
        .read_label_bucket_slot(bucket_slot)
        .expect("read bucket");
    assert!(bucket.is_tree_mode());
    assert_eq!(
        bucket.tree_mode_physical_depth(),
        2,
        "fixture must be depth 2"
    );
    assert_eq!(bucket.stored_slots, TREE_SEED_SLOTS);
    assert_eq!(bucket.degree, TREE_SEED_SLOTS - tombstones);
    TreeFixture {
        graph,
        vid,
        label,
        bucket_slot,
    }
}

/// The exact measured visitor (0336 shape: fixed count/checksum/black_box).
fn visit_exact(fixture: &TreeFixture, offset: u32) -> (u32, u64) {
    let request = LabeledTraversalRequest {
        owner: fixture.vid,
        label: fixture.label,
        order: OutEdgeOrder::Ascending,
    };
    let mut count = 0u32;
    let mut checksum = 0u64;
    let result = Traversal::visit_edges_window(
        &fixture.graph,
        &request,
        TraversalWindow::new(offset, Some(OFFSET_WINDOW_LIMIT)),
        |slot, edge| {
            count += 1;
            checksum = checksum
                .wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
                .wrapping_add(u64::from(edge.target));
            black_box((slot.raw(), edge.target));
            std::ops::ControlFlow::<()>::Continue(())
        },
    )
    .expect("tree window traversal");
    assert!(result.is_continue());
    (count, checksum)
}

/// Expected live rows in the window `[offset, offset + limit)` in slot
/// order: the truth is the live slots of the bucket (contiguous live
/// suffix `[tombstones, stored)`).
fn expected_window_rows(
    tombstones: u32,
    stored: u32,
    offset: u32,
) -> Vec<(BucketEntryPosition, u32)> {
    let mut rows = Vec::new();
    for slot in offset..offset + OFFSET_WINDOW_LIMIT {
        if slot >= tombstones && slot < stored {
            rows.push((BucketEntryPosition::new(slot), 10_000_000 + slot));
        }
    }
    rows
}

fn assert_window_parity(fixture: &TreeFixture, tombstones: u32, offset: u32) {
    let stored = TREE_SEED_SLOTS;
    let expected = expected_window_rows(tombstones, stored, offset);
    let mut rows = Vec::new();
    let request = LabeledTraversalRequest {
        owner: fixture.vid,
        label: fixture.label,
        order: OutEdgeOrder::Ascending,
    };
    let result = Traversal::visit_edges_window(
        &fixture.graph,
        &request,
        TraversalWindow::new(offset, Some(OFFSET_WINDOW_LIMIT)),
        |slot, edge| {
            rows.push((slot, edge.target));
            std::ops::ControlFlow::<()>::Continue(())
        },
    )
    .expect("parity traversal");
    assert!(result.is_continue());
    assert_eq!(rows, expected);
    assert_eq!(rows.len(), OFFSET_WINDOW_LIMIT as usize);
}

/// Bench-scoped Level-1 counting walk: root-entry walk (leaf block ids) +
/// per-block header tombstone count. Blocks entirely below the window are
/// resolved by header alone (fully-dead blocks skip their payloads
/// entirely); only blocks overlapping the window scan payloads. Returns
/// (count, checksum, blocks_passed, blocks_total).
fn counting_walk_offset(fixture: &TreeFixture, offset: u32) -> (u32, u64, u32, u32) {
    use crate::labeled::ltb_raw_block_store::BLOCK_PAYLOAD_BYTES;
    use crate::labeled::tree_csr::B;
    let bucket = fixture
        .graph
        .buckets()
        .read_label_bucket_slot(fixture.bucket_slot)
        .expect("bucket");
    let stored = bucket.stored_slots;
    let leaf_count =
        u32::try_from((u64::from(stored)).div_ceil(crate::labeled::tree_csr::B as u64))
            .expect("leaf count");
    // Root-entry walk: resolve every leaf block id (the production
    // resolver, bench-invoked).
    let mut leaf_ids = Vec::with_capacity(leaf_count as usize);
    for block_index in 0..leaf_count {
        leaf_ids.push(
            crate::labeled::graph::tree_write::resolve_leaf_block_id::<TreeBenchEdge, _>(
                &fixture.graph,
                &bucket,
                block_index,
            )
            .expect("resolve leaf"),
        );
    }
    let window_end = offset.saturating_add(OFFSET_WINDOW_LIMIT);
    let mut count = 0u32;
    let mut checksum = 0u64;
    let mut blocks_passed = 0u32;
    for (block_index, block_id) in leaf_ids.iter().enumerate() {
        let start_slot = block_index as u32 * B as u32;
        let end_slot = (start_slot + B as u32).min(stored);
        blocks_passed += 1;
        let header = fixture.graph.ltb().read_block_header(*block_id);
        let used = end_slot - start_slot;
        // Bucket-level accounting per block: live == used - count.
        let block_live = used.saturating_sub(u32::from(header.tombstone_count));
        let fully_dead = header.tombstone_count as u32 == used;
        if end_slot <= offset || window_end <= start_slot {
            // Block entirely outside the window: header accounting only.
            if fully_dead {
                continue; // no payload read
            }
            continue; // partially-live out-of-window block: header only
        }
        // Block overlaps the window: scan in-window slots and verify the
        // scanned live count equals the header count.
        let mut scanned_live = 0u32;
        for slot in start_slot.max(offset)..end_slot.min(window_end) {
            let in_block = (slot - start_slot) as usize * 4;
            let mut buf = [0u8; 4];
            fixture
                .graph
                .ltb()
                .read_payload_partial(*block_id, in_block, &mut buf)
                .expect("read payload");
            let edge = TreeBenchEdge::read_from(&buf);
            if !edge.is_tombstone_edge() {
                scanned_live += 1;
                count += 1;
                checksum = checksum
                    .wrapping_add(u64::from(slot).wrapping_mul(31))
                    .wrapping_add(u64::from(edge.target));
                black_box((slot, edge.target));
            }
        }
        // Cross-check the scanned block's true live count against its
        // header count (full-block scan when the window covers the block).
        if offset <= start_slot && end_slot <= window_end {
            assert_eq!(
                scanned_live, block_live,
                "block {block_id} live count != header-derived live count"
            );
        }
        let _ = BLOCK_PAYLOAD_BYTES;
    }
    (count, checksum, blocks_passed, leaf_count)
}

fn assert_l1_parity(fixture: &TreeFixture, tombstones: u32, offset: u32, exact: (u32, u64)) {
    let (l1_count, l1_checksum, _, _) = counting_walk_offset(fixture, offset);
    assert_eq!(
        (l1_count, l1_checksum),
        exact,
        "counting walk must match the exact walk"
    );
    assert_window_parity(fixture, tombstones, offset);
    // Bucket-level Σ check (preflight/postflight discipline): every block's
    // header count == scanned markers, Σ == stored - degree.
    use crate::labeled::tree_csr::B;
    let bucket = fixture
        .graph
        .buckets()
        .read_label_bucket_slot(fixture.bucket_slot)
        .expect("bucket");
    let leaf_count =
        u32::try_from((u64::from(bucket.stored_slots)).div_ceil(B as u64)).expect("leaf count");
    let mut total = 0u64;
    for block_index in 0..leaf_count {
        let block_id =
            crate::labeled::graph::tree_write::resolve_leaf_block_id::<TreeBenchEdge, _>(
                &fixture.graph,
                &bucket,
                block_index,
            )
            .expect("resolve leaf");
        let header = fixture.graph.ltb().read_block_header(block_id);
        let start_slot = block_index * B as u32;
        let end_slot = (start_slot + B as u32).min(bucket.stored_slots);
        let mut scanned = 0u32;
        for slot in start_slot..end_slot {
            let mut buf = [0u8; 4];
            fixture
                .graph
                .ltb()
                .read_payload_partial(block_id, ((slot - start_slot) * 4) as usize, &mut buf)
                .expect("read payload");
            if TreeBenchEdge::read_from(&buf).is_tombstone_edge() {
                scanned += 1;
            }
        }
        assert_eq!(u32::from(header.tombstone_count), scanned);
        total += u64::from(header.tombstone_count);
    }
    assert_eq!(total, u64::from(bucket.stored_slots - bucket.degree));
}

/// Baseline exact-scan OFFSET at 87.5% tombstones, first live slot.
#[bench(raw)]
fn offset_tree_1m_d875_off_first() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875;
    let exact = visit_exact(&fixture, offset);
    assert_eq!(exact.0, OFFSET_WINDOW_LIMIT);
    assert_l1_parity(&fixture, TREE_DENSITY_875, offset, exact);
    bench_fn(|| {
        let (count, checksum) = visit_exact(&fixture, offset);
        black_box((count, checksum));
    })
}

/// Level-1 counting walk at the same point.
#[bench(raw)]
fn offset_tree_1m_d875_off_first_l1() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875;
    let exact = visit_exact(&fixture, offset);
    assert_l1_parity(&fixture, TREE_DENSITY_875, offset, exact);
    bench_fn(|| {
        let (count, checksum, blocks_passed, blocks_total) = counting_walk_offset(&fixture, offset);
        black_box((count, checksum, blocks_passed, blocks_total));
    })
}

/// Baseline exact-scan OFFSET at 87.5%, deep into the live region.
#[bench(raw)]
fn offset_tree_1m_d875_off_deep() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875 + OFFSET_STEP;
    let exact = visit_exact(&fixture, offset);
    assert_eq!(exact.0, OFFSET_WINDOW_LIMIT);
    assert_l1_parity(&fixture, TREE_DENSITY_875, offset, exact);
    bench_fn(|| {
        let (count, checksum) = visit_exact(&fixture, offset);
        black_box((count, checksum));
    })
}

/// Level-1 counting walk at the same point.
#[bench(raw)]
fn offset_tree_1m_d875_off_deep_l1() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875 + OFFSET_STEP;
    let exact = visit_exact(&fixture, offset);
    assert_l1_parity(&fixture, TREE_DENSITY_875, offset, exact);
    bench_fn(|| {
        let (count, checksum, blocks_passed, blocks_total) = counting_walk_offset(&fixture, offset);
        black_box((count, checksum, blocks_passed, blocks_total));
    })
}

/// Baseline exact-scan OFFSET at 50% tombstones, first live slot.
#[bench(raw)]
fn offset_tree_1m_d50_off_first() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_50);
    let offset = TREE_DENSITY_50;
    let exact = visit_exact(&fixture, offset);
    assert_eq!(exact.0, OFFSET_WINDOW_LIMIT);
    assert_l1_parity(&fixture, TREE_DENSITY_50, offset, exact);
    bench_fn(|| {
        let (count, checksum) = visit_exact(&fixture, offset);
        black_box((count, checksum));
    })
}

/// Level-1 counting walk at the same point.
#[bench(raw)]
fn offset_tree_1m_d50_off_first_l1() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_50);
    let offset = TREE_DENSITY_50;
    let exact = visit_exact(&fixture, offset);
    assert_l1_parity(&fixture, TREE_DENSITY_50, offset, exact);
    bench_fn(|| {
        let (count, checksum, blocks_passed, blocks_total) = counting_walk_offset(&fixture, offset);
        black_box((count, checksum, blocks_passed, blocks_total));
    })
}

/// Slab-regime re-anchor at the post-ADR-0088 extent cap 4,096: the 0283
/// worst-case shape (87.5% tombstones, OFFSET 960, LIMIT 32) at the legal
/// slab ceiling. The fixed slab rule: slab stays status-quo iff worst
/// overshoot < 100,000 instructions/query.
#[bench(raw)]
fn offset_slab4096_d875_off960() -> canbench_rs::BenchResult {
    let (graph, vid, label) = super::bench::workload_dense_graph(SLAB_REANCHOR_EXTENT);
    for slot in 0..SLAB_REANCHOR_EXTENT {
        if slot % SLAB_REANCHOR_STRIDE != 0 {
            assert!(
                graph
                    .remove_edge_at_slot(vid, label, slot)
                    .expect("slab reanchor remove")
                    .is_some()
            );
        }
    }
    let request = LabeledTraversalRequest {
        owner: vid,
        label,
        order: OutEdgeOrder::Ascending,
    };
    // Preflight parity: the window rows are the multiples of 8 in
    // [960, 992).
    let mut rows = Vec::new();
    let result = Traversal::visit_edges_window(
        &graph,
        &request,
        TraversalWindow::new(SLAB_REANCHOR_OFFSET, Some(OFFSET_WINDOW_LIMIT)),
        |slot, edge| {
            rows.push((slot, edge.0));
            std::ops::ControlFlow::<()>::Continue(())
        },
    )
    .expect("slab reanchor parity traversal");
    assert!(result.is_continue());
    let expected: Vec<(BucketEntryPosition, u32)> = (SLAB_REANCHOR_OFFSET
        ..SLAB_REANCHOR_OFFSET + OFFSET_WINDOW_LIMIT)
        .filter(|slot| slot % SLAB_REANCHOR_STRIDE == 0)
        .map(|slot| {
            (
                BucketEntryPosition::new(slot),
                super::bench::workload_target(slot),
            )
        })
        .collect();
    assert_eq!(rows, expected);
    bench_fn(|| {
        let mut count = 0u32;
        let mut checksum = 0u64;
        let result = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(SLAB_REANCHOR_OFFSET, Some(OFFSET_WINDOW_LIMIT)),
            |slot, edge| {
                count += 1;
                checksum = checksum
                    .wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
                    .wrapping_add(u64::from(edge.0));
                black_box((slot.raw(), edge.0));
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .expect("slab reanchor measured traversal");
        assert!(result.is_continue());
        black_box((count, checksum));
    })
}

// ===========================================================================
// Plan 0338 — S0/S1/S1+S2 attribution (ADR 0094 §4)
// ===========================================================================
//
// S0: the pre-0338 exact walk — every leaf block resolved and payload-
//      scanned, position cut applied in the closure (bench-scope twin of
//      the old production arm via `visit_tree_mode_label_bucket_edges`).
// S1: window-restricted block resolution (ADR 0088 §2 slot-range
//      arithmetic), all overlapping payloads scanned, no skip.
// S1+S2: production `visit_edges_window` — S1 + the header-count dead
//      block skip with fail-closed scan verification.
//
// All three arms run the SAME window and the SAME visitor work; the
// three-arm parity gate asserts identical (count, checksum) before every
// measured closure. K per arm = blocks entered; payload-scanned blocks
// recorded for the S2 skip accounting.

use crate::labeled::graph::tree_read::visit_tree_mode_label_bucket_edges;

/// S0 arm: full leaf-set walk with the pre-0338 counting cut.
/// Returns (count, checksum, blocks_entered).
fn s0_exact_walk(fixture: &TreeFixture, offset: u32, limit: Option<u32>) -> (u32, u64, u32) {
    let bucket = fixture
        .graph
        .buckets()
        .read_label_bucket_slot(fixture.bucket_slot)
        .expect("bucket");
    let stored = bucket.stored_slots;
    let end = match limit {
        Some(l) => offset.saturating_add(l).min(stored),
        None => stored,
    };
    let mut count = 0u32;
    let mut checksum = 0u64;
    let mut current: u32 = 0;
    visit_tree_mode_label_bucket_edges(
        &fixture.graph,
        fixture.label.raw(),
        &bucket,
        bucket.degree,
        OutEdgeOrder::Ascending,
        |slot, edge| {
            if current < offset {
                current += 1;
                return;
            }
            if current >= end {
                return;
            }
            current += 1;
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                return;
            }
            count += 1;
            checksum = checksum
                .wrapping_add(u64::from(slot).wrapping_mul(31))
                .wrapping_add(u64::from(edge.target));
            black_box((slot, edge.target));
        },
    )
    .expect("s0 walk");
    let blocks_entered =
        u32::try_from(u64::from(stored).div_ceil(crate::labeled::tree_csr::B as u64))
            .expect("leaf count");
    (count, checksum, blocks_entered)
}

/// S1 arm: window-restricted block resolution, NO dead-block skip. Every
/// overlapping block's payload is scanned. Returns (count, checksum,
/// blocks_entered, blocks_payload_scanned).
fn s1_window_walk(fixture: &TreeFixture, offset: u32, limit: Option<u32>) -> (u32, u64, u32, u32) {
    use crate::labeled::tree_csr::B;
    let bucket = fixture
        .graph
        .buckets()
        .read_label_bucket_slot(fixture.bucket_slot)
        .expect("bucket");
    let stored = bucket.stored_slots;
    let end = match limit {
        Some(l) => offset.saturating_add(l).min(stored),
        None => stored,
    };
    let leaf_count = u32::try_from(u64::from(stored).div_ceil(B as u64)).expect("leaf count");
    let start_block = offset / B as u32;
    let end_block_exclusive = u64::from(end).div_ceil(B as u64) as u32;
    let end_block = end_block_exclusive.min(leaf_count);
    let mut count = 0u32;
    let mut checksum = 0u64;
    for block_index in start_block..end_block {
        let block_first = block_index * B as u32;
        let block_end = (block_first + B as u32).min(stored);
        let block_id =
            crate::labeled::graph::tree_write::resolve_leaf_block_id::<TreeBenchEdge, _>(
                &fixture.graph,
                &bucket,
                block_index,
            )
            .expect("resolve leaf");
        let mut payload = [0u8; crate::labeled::ltb_raw_block_store::BLOCK_PAYLOAD_BYTES];
        fixture
            .graph
            .ltb()
            .read_payload(block_id, &mut payload)
            .expect("read payload");
        for slot in block_first.max(offset)..block_end.min(end) {
            let byte = (slot - block_first) as usize * 4;
            let edge = TreeBenchEdge::read_from(&payload[byte..byte + 4]);
            if edge.is_deleted_slot() || edge.is_tombstone_edge() {
                continue;
            }
            count += 1;
            checksum = checksum
                .wrapping_add(u64::from(slot).wrapping_mul(31))
                .wrapping_add(u64::from(edge.target));
            black_box((slot, edge.target));
        }
    }
    (
        count,
        checksum,
        end_block - start_block,
        end_block - start_block,
    )
}

/// The S1+S2 arm is the production `visit_edges_window` (exact window form,
/// limit-parameterized).
fn visit_exact_window(fixture: &TreeFixture, offset: u32, limit: Option<u32>) -> (u32, u64) {
    let request = LabeledTraversalRequest {
        owner: fixture.vid,
        label: fixture.label,
        order: OutEdgeOrder::Ascending,
    };
    let mut count = 0u32;
    let mut checksum = 0u64;
    let result = Traversal::visit_edges_window(
        &fixture.graph,
        &request,
        TraversalWindow::new(offset, limit),
        |slot, edge| {
            count += 1;
            checksum = checksum
                .wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
                .wrapping_add(u64::from(edge.target));
            black_box((slot.raw(), edge.target));
            std::ops::ControlFlow::<()>::Continue(())
        },
    )
    .expect("tree window traversal");
    assert!(result.is_continue());
    (count, checksum)
}

/// The three-arm parity gate: identical (count, checksum) across S0, S1,
/// and the production path for the same window.
fn assert_three_arm_parity(fixture: &TreeFixture, offset: u32, limit: Option<u32>) {
    let (s0_count, s0_checksum, _) = s0_exact_walk(fixture, offset, limit);
    let (s1_count, s1_checksum, _, _) = s1_window_walk(fixture, offset, limit);
    let (p_count, p_checksum) = visit_exact_window(fixture, offset, limit);
    assert_eq!(
        (s0_count, s0_checksum),
        (s1_count, s1_checksum),
        "S1 parity vs S0"
    );
    assert_eq!(
        (s0_count, s0_checksum),
        (p_count, p_checksum),
        "production parity vs S0"
    );
}

// --- Fixture 1: 0337 dead-prefix grid (S1 expected to dominate: the dead
// prefix lies before the window, so no fully-dead block is inside it). ---

/// S0 on the 87.5% dead-prefix fixture (OFFSET = first live slot).
#[bench(raw)]
fn offset_tree_d875_s0() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875;
    assert_three_arm_parity(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
    bench_fn(|| {
        let (count, checksum, blocks) = s0_exact_walk(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
        black_box((count, checksum, blocks));
    })
}

/// S1 on the 87.5% dead-prefix fixture (no dead block inside the window).
#[bench(raw)]
fn offset_tree_d875_s1() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875;
    assert_three_arm_parity(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
    bench_fn(|| {
        let (count, checksum, entered, scanned) =
            s1_window_walk(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
        black_box((count, checksum, entered, scanned));
    })
}

/// S1+S2 (production) on the 87.5% dead-prefix fixture.
#[bench(raw)]
fn offset_tree_d875_s1s2() -> canbench_rs::BenchResult {
    let fixture = build_tree_fixture(TREE_DENSITY_875);
    let offset = TREE_DENSITY_875;
    assert_three_arm_parity(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
    bench_fn(|| {
        let (count, checksum) = visit_exact_window(&fixture, offset, Some(OFFSET_WINDOW_LIMIT));
        black_box((count, checksum));
    })
}

// --- Fixture 2: in-window dead blocks (S2 expected to dominate the
// residual: every OTHER leaf block fully tombstoned; the window starts at
// the first live slot and extends to the bucket end — no limit — so it
// spans 1,024 blocks of which 512 are fully dead, plus the partial tail). ---

/// In-window dead fixture: 1M+1 slots; every even-indexed leaf block fully
/// tombstoned (513 dead blocks including the 1-slot tail). Returns the
/// fixture and the first live slot (block 1's first slot).
fn build_in_window_dead_fixture() -> (TreeFixture, u32) {
    use crate::labeled::tree_csr::B;
    let fixture = build_tree_fixture(0);
    let stored = TREE_SEED_SLOTS;
    let leaf_count = u32::try_from(u64::from(stored).div_ceil(B as u64)).expect("leaf count");
    for block_index in (0..leaf_count).step_by(2) {
        let first = block_index * B as u32;
        let last = (first + B as u32).min(stored);
        for slot in first..last {
            fixture
                .graph
                .remove_edge_at_slot(fixture.vid, fixture.label, slot)
                .expect("in-window tombstone remove");
        }
    }
    (fixture, B as u32)
}

/// S0 on the in-window dead fixture (full-extent window).
#[bench(raw)]
fn offset_tree_inwindow_s0() -> canbench_rs::BenchResult {
    let (fixture, first_live) = build_in_window_dead_fixture();
    assert_three_arm_parity(&fixture, first_live, None);
    bench_fn(|| {
        let (count, checksum, blocks) = s0_exact_walk(&fixture, first_live, None);
        black_box((count, checksum, blocks));
    })
}

/// S1 on the in-window dead fixture (window-restricted, no skip).
#[bench(raw)]
fn offset_tree_inwindow_s1() -> canbench_rs::BenchResult {
    let (fixture, first_live) = build_in_window_dead_fixture();
    assert_three_arm_parity(&fixture, first_live, None);
    bench_fn(|| {
        let (count, checksum, entered, scanned) = s1_window_walk(&fixture, first_live, None);
        black_box((count, checksum, entered, scanned));
    })
}

/// S1+S2 (production) on the in-window dead fixture.
#[bench(raw)]
fn offset_tree_inwindow_s1s2() -> canbench_rs::BenchResult {
    let (fixture, first_live) = build_in_window_dead_fixture();
    assert_three_arm_parity(&fixture, first_live, None);
    bench_fn(|| {
        let (count, checksum) = visit_exact_window(&fixture, first_live, None);
        black_box((count, checksum));
    })
}
