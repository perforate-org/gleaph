use std::hint::black_box;

use canbench_rs::bench;

use super::{EdgeStore, counts::SegmentEdgeCounts, segment_tree_leaf_count};
use crate::{
    VertexId, bench as helper,
    lara::vertex::{Vertex, VertexStore},
    test_support::TestEdge,
    traits::{CsrEdge, CsrEdgeTombstone},
};

/// Matches [`EdgeStore::new`] / [`EdgeStore::grow_segment_tree_to`] in this module.
const BENCH_EDGE_SEGMENT_SIZE: u32 = 16;

/// 24-byte edge mirroring the production graph `Edge` row width, for measuring
/// the byte-length component of the slab write path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WideEdge([u8; 24]);

impl CsrEdge for WideEdge {
    const BYTES: usize = 24;

    fn read_from(bytes: &[u8]) -> Self {
        debug_assert_eq!(bytes.len(), Self::BYTES);
        let mut row = [0u8; 24];
        row.copy_from_slice(&bytes[..24]);
        Self(row)
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[..24].copy_from_slice(&self.0);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(u32::from_le_bytes(self.0[..4].try_into().unwrap()))
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        let mut row = self.0;
        row[..4].copy_from_slice(&u32::from(vid).to_le_bytes());
        Self(row)
    }
}

fn edge_store_with_vertices<E: CsrEdge>(
    vertex_count: u32,
    slot_stride: u32,
) -> (
    VertexStore<Vertex, helper::BenchMemory>,
    EdgeStore<E, helper::BenchMemory>,
) {
    // Deliberately below the production graph boundary: these benches measure
    // EdgeStore slab/log primitives with controlled row geometry. Production
    // graph fixtures use the shared helpers in `crate::bench`, which materialize
    // rows through `LaraGraph::push_vertex` instead of assigning base slots.
    let mut memories = helper::BenchMemoryFactory::new();
    let vertices = VertexStore::new(memories.memory()).expect("vertices");
    for vid in 0..vertex_count {
        vertices
            .push(Vertex::from_parts(
                u64::from(vid) * u64::from(slot_stride),
                0,
                0,
                -1,
                false,
            ))
            .expect("push vertex");
    }
    let edges = EdgeStore::new(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        u64::from(vertex_count) * u64::from(slot_stride),
        BENCH_EDGE_SEGMENT_SIZE,
        0,
    )
    .expect("edge store");
    let seg_count = segment_tree_leaf_count(vertex_count.into(), BENCH_EDGE_SEGMENT_SIZE);
    edges
        .grow_segment_tree_to(seg_count)
        .expect("grow edge segments");
    // With a single vertex row but a wide slab (`slot_stride > 1`), PMA leaf totals stay
    // zero unless a graph initializer runs — `slab_window_exclusive_end` then reports a
    // zero-width CSR window and every insert overflows into the segment log (`SegmentLogFull`).
    // Real graphs set leaf totals via `LaraGraph::update_leaf_count_and_ancestors`; mirror
    // that here for multi-slot single-vertex workloads (log-spill benches keep stride `1`).
    if vertex_count == 1 && slot_stride > 1 {
        let elem_cap = u64::from(vertex_count).saturating_mul(u64::from(slot_stride));
        let total_i64 = i64::try_from(elem_cap).unwrap_or(i64::MAX);
        let idx = u64::from(seg_count);
        edges.counts_store().set(
            idx,
            &SegmentEdgeCounts {
                actual: 0,
                total: total_i64,
            },
        );
    }
    (vertices, edges)
}

/// Measures the no-location `EdgeStore` insert path when each insert fits directly in the
/// vertex-owned slab span. This isolates the update-side fast path before log
/// spill or graph-level rebalance is involved.
#[bench(raw)]
fn bench_r_ed_st_si_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1024, 4);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_slab_insert");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            edges
                .insert_edge_without_logical_slot(
                    &vertices,
                    VertexId::from(i as u32),
                    helper::test_edge(i),
                )
                .expect("insert slab edge");
        }
        black_box(vertices.len());
    })
}

/// Measures no-location overflow-log admission after a tiny owned slab span fills. The
/// workload stays below the per-segment log cap and watches for regressions in
/// log-chain writes and vertex `log_head` updates.
#[bench(raw)]
fn bench_r_ed_st_ls_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_log_spill");
        for i in 0..128 {
            let i = black_box(i);
            edges
                .insert_edge_without_logical_slot(
                    &vertices,
                    VertexId::from(black_box(0u32)),
                    helper::test_edge(i),
                )
                .expect("insert log edge");
        }
        black_box(vertices.get(VertexId::from(0)).log_head());
    })
}

/// Measures collecting one large neighborhood from slab storage after setup.
/// This protects the clean scan contract at the `EdgeStore` layer, including
/// decoding fixed-width edge records into a caller-owned vector.
#[bench(raw)]
fn bench_r_ed_st_oi_col_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_out_edges_collect");
        black_box(
            edges
                .collect_out_edges_slot_order(&vertices, VertexId::from(black_box(0u32)))
                .expect("collect edges"),
        );
    })
}

/// Measures iteration over one large slab-backed neighborhood without
/// materializing the whole row into a vector.
#[bench(raw)]
fn bench_r_ed_st_oi_1024() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_out_edges_iter");
        let mut count = 0usize;
        for edge in edges
            .out_edges_iter(&vertices, VertexId::from(black_box(0u32)))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}

/// Measures iteration over a log-backed row. The default ascending iterator walks the
/// slab prefix first, then replays the overflow chain in materialization order without
/// allocating the collected edge vector.
#[bench(raw)]
fn bench_r_ed_st_oi_lb_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    for i in 0..128 {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_out_edges_iter_log_backed");
        let mut count = 0usize;
        for edge in edges
            .out_edges_iter(&vertices, VertexId::from(black_box(0u32)))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}

/// Measures the explicit descending iterator over the same log-backed row. Kept as the
/// explicit-DESC counterpart to [`bench_lara_edge_store_out_edges_iter_log_backed_128`]
/// so the hot-path iteration cost can be compared against the ascending default directly.
#[bench(raw)]
fn bench_r_ed_st_d_oi_lb_128() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, 1);
    for i in 0..128 {
        edges
            .insert_edge(&vertices, VertexId::from(0), helper::test_edge(i))
            .expect("insert edge");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_edge_store_desc_out_edges_iter_log_backed");
        let mut count = 0usize;
        for edge in edges
            .desc_out_edges_iter(&vertices, VertexId::from(black_box(0u32)))
            .expect("iterate edges")
        {
            black_box(edge);
            count += 1;
        }
        black_box(count);
    })
}

/// Slab-write micro benches for the Plan 0199 batch unordered placement threshold.
///
/// `write_slot` pays the grow-check + address-computation fixed cost once per slot while
/// `write_slots_contiguous` pays it once for the whole window; `bench_lara_slab_patch_sparse_1`
/// measures the whole-window read+patch+write competitor that rewrites an entire slot window
/// to fill tombstones. Each fixture pre-grows the slab in setup so the measured closure
/// contains pure write/read cost without memory-growth noise. The IC charging layer adds
/// ~1 instruction per byte on top, which the 24-byte `WideEdge` variant isolates.
#[bench(raw)]
fn bench_r_sl_ws_1() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        edges
            .write_slot(i, helper::test_edge(i))
            .expect("pre-grow slot");
    }
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_slab_write_single");
        for i in 0..helper::MEDIUM_N {
            let i = black_box(i);
            edges
                .write_slot(i, helper::test_edge(i))
                .expect("write slot");
        }
        black_box(vertices.len());
    })
}

/// One `write_slots_contiguous` call writing the same 1024 four-byte slots as
/// [`bench_lara_slab_write_single_1`], isolating the per-call fixed cost of the batch path.
#[bench(raw)]
fn bench_r_sl_wc_1() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices::<TestEdge>(1, helper::MEDIUM_N as u32);
    let edge_bytes: Vec<u8> = {
        let mut buf = vec![0u8; helper::MEDIUM_N as usize * TestEdge::BYTES];
        for i in 0..helper::MEDIUM_N {
            let off = i as usize * TestEdge::BYTES;
            helper::test_edge(i).write_to(&mut buf[off..off + TestEdge::BYTES]);
        }
        buf
    };
    edges
        .write_slots_contiguous(0, &edge_bytes)
        .expect("pre-grow window");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_slab_write_contig");
        edges
            .write_slots_contiguous(0, black_box(&edge_bytes))
            .expect("write contiguous");
        black_box(vertices.len());
    })
}

/// Same contiguous one-call path as [`bench_lara_slab_write_contig_1`] but with the
/// 24-byte `WideEdge` row width, isolating the byte-length component of the slab write.
#[bench(raw)]
fn bench_r_sl_wc_24() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices::<WideEdge>(1, helper::MEDIUM_N as u32);
    let base = WideEdge([0u8; 24]);
    let edge_bytes: Vec<u8> = {
        let mut buf = vec![0u8; helper::MEDIUM_N as usize * WideEdge::BYTES];
        for i in 0..helper::MEDIUM_N {
            let off = i as usize * WideEdge::BYTES;
            base.with_neighbor_vid(VertexId::from(i as u32))
                .write_to(&mut buf[off..off + WideEdge::BYTES]);
        }
        buf
    };
    edges
        .write_slots_contiguous(0, &edge_bytes)
        .expect("pre-grow window");
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_slab_write_contig_24b");
        edges
            .write_slots_contiguous(0, black_box(&edge_bytes))
            .expect("write contiguous");
        black_box(vertices.len());
    })
}

/// Whole-window rewrite competitor: a half-deleted 1024-slot row (even slots tombstoned,
/// odd slots live) is read into a buffer, the live slots are patched with fresh encodings,
/// and the full window is written back in one contiguous call. This is the cost model for
/// filling in-slab tombstones by rewriting a slab window instead of per-hole writes.
#[bench(raw)]
fn bench_r_sl_ps_1() -> canbench_rs::BenchResult {
    let (vertices, edges) = edge_store_with_vertices(1, helper::MEDIUM_N as u32);
    for i in 0..helper::MEDIUM_N {
        let edge = if i % 2 == 0 {
            TestEdge::tombstone_edge()
        } else {
            helper::test_edge(i)
        };
        edges.write_slot(i, edge).expect("pre-fill slot");
    }
    let mut window = vec![0u8; helper::MEDIUM_N as usize * TestEdge::BYTES];
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("lara_slab_patch_sparse");
        edges.read_slots_contiguous(0, &mut window);
        for i in (1..helper::MEDIUM_N).step_by(2) {
            let off = i as usize * TestEdge::BYTES;
            helper::test_edge(i).write_to(&mut window[off..off + TestEdge::BYTES]);
        }
        edges
            .write_slots_contiguous(0, black_box(&window))
            .expect("rewrite window");
        black_box(vertices.len());
    })
}
