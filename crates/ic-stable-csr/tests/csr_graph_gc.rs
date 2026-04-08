//! Logical delete, degree updates, GC queue, and filtered iterators.

mod common;

use common::{
    TestEdge as TE, TestVertex as TV, assert_dense_vertex_bases_non_decreasing, empty_vertex, vm,
};
use ic_stable_csr::{
    CsrEdgeSlotTombstoneScan as _, CsrGraphError, CsrGraphWithGcQueue, DgapStores,
    SegmentEdgeCounts, SegmentMaintainThresholds, VectorMemory,
    dgap::recount_segment_edge_counts_column,
    traits::{CsrVertex, CsrVertexTombstone},
};

type GcTestGraph = CsrGraphWithGcQueue<
    TV,
    TE,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
>;

fn assert_fwd_rev_bases_non_decreasing(g: &GcTestGraph) {
    assert_dense_vertex_bases_non_decreasing(g.graph().forward_dgap());
    assert_dense_vertex_bases_non_decreasing(g.graph().reverse_dgap());
}

fn assert_sec_matches_full_recount_te(
    stores: &DgapStores<TV, TE, VectorMemory, VectorMemory, VectorMemory>,
) {
    let h = stores.edges.header().unwrap();
    let sc = h.segment_count as usize;
    let len = sc * 2;
    let mut buf = vec![
        SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 0,
        };
        len
    ];
    let es = h.edge_stride;
    recount_segment_edge_counts_column(
        &stores.vertices,
        stores.vertices.len(),
        h.segment_count,
        h.segment_size,
        h.elem_capacity,
        |slot| {
            let e = stores.edges.read_slot(es, slot);
            TE::record_is_physical_tombstone(&e)
        },
        &mut buf,
    );
    for j in 0..len {
        assert_eq!(
            stores.edges.read_segment_edge_counts(j),
            buf[j],
            "SEC node {j} diverges from full recount"
        );
    }
}

#[test]
fn delete_edge_tombstone_gc_and_degrees() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            1,
            8,
            0,
        Some(SegmentMaintainThresholds {
            soft_tombstone_score_threshold: 0.05,
            strict_tombstone_score_threshold: 0.20,
            ..Default::default()
        }),
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);

    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 2, TE([2, 0, 0, 0])).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);

    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .get_dense(0)
            .unwrap()
            .degree(),
        1
    );
    assert_eq!(
        g.graph()
            .reverse_dgap()
            .vertices
            .get_dense(1)
            .unwrap()
            .degree(),
        1
    );

    g.delete_edge_directed(0, 1).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);
    assert_sec_matches_full_recount_te(g.graph().forward_dgap());
    assert_sec_matches_full_recount_te(g.graph().reverse_dgap());
    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .get_dense(0)
            .unwrap()
            .degree(),
        0
    );
    assert_eq!(
        g.graph()
            .reverse_dgap()
            .vertices
            .get_dense(1)
            .unwrap()
            .degree(),
        0
    );

    assert!(g.work_queue_len() >= 1);
    let n = g.gc_step(8).expect("gc");
    assert!(n >= 1);

    assert_sec_matches_full_recount_te(g.graph().forward_dgap());
    assert_sec_matches_full_recount_te(g.graph().reverse_dgap());
    assert_fwd_rev_bases_non_decreasing(&g);

    let out0: Vec<_> = g
        .out_edges_logical(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out0.is_empty());
}

#[test]
fn delete_vertex_hides_edges_until_gc() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            1,
            8,
            0,
        None,
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 0, TE([0, 0, 0, 0])).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);

    assert_eq!(
        g.graph()
            .forward_dgap()
            .vertices
            .get_dense(1)
            .unwrap()
            .degree(),
        1
    );
    g.delete_vertex(0).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);
    assert_sec_matches_full_recount_te(g.graph().forward_dgap());
    assert_sec_matches_full_recount_te(g.graph().reverse_dgap());
    assert!(
        g.graph()
            .forward_dgap()
            .vertices
            .get_dense(0)
            .unwrap()
            .is_tombstone()
    );

    let out1: Vec<_> = g
        .out_edges_logical(1)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out1.is_empty());

    let _ = g.gc_step(16).expect("gc");
    assert_fwd_rev_bases_non_decreasing(&g);
    assert_sec_matches_full_recount_te(g.graph().forward_dgap());
    assert_sec_matches_full_recount_te(g.graph().reverse_dgap());
}

#[test]
fn insert_rejects_tombstone_endpoint() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            1,
            8,
            0,
        None,
    )
    .expect("format");

    for _ in 0..2 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.delete_vertex(0).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);

    let e = g.insert_directed(0, 1, TE([1, 0, 0, 0]));
    assert!(matches!(
        e,
        Err(CsrGraphError::EndpointTombstone { vid: 0 })
    ));
}

#[test]
fn insert_rejects_duplicate_neighbor_slot() {
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            1,
            8,
            0,
        None,
    )
    .expect("format");

    for _ in 0..2 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    assert_fwd_rev_bases_non_decreasing(&g);

    let e = g.insert_directed(0, 1, TE([1, 0, 0, 0]));
    assert!(matches!(
        e,
        Err(CsrGraphError::AdjacencySlotOccupied { src: 0, dst: 1 })
    ));
}

#[test]
fn delete_edge_inline_when_queue_pressure_threshold_zero() {
    let thr = SegmentMaintainThresholds {
        queue_depth_inline_pressure: 0,
        ..Default::default()
    };
    let g = CsrGraphWithGcQueue::format_new_with_gc_queue(
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            vm(),
            64,
            1,
            8,
            0,
        Some(thr),
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.delete_edge_directed(0, 1).unwrap();
    assert_eq!(g.work_queue_len(), 0);
    assert_eq!(g.gc_step(8).unwrap(), 0);
    assert_fwd_rev_bases_non_decreasing(&g);
    let out0: Vec<_> = g
        .out_edges_logical(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out0.is_empty());
}
