mod common;

use common::{TestEdge as TE, TestVertex as TV, empty_vertex, vm};
use ic_stable_csr::{
    CsrGraphWithGcQueueRowTombstone, CsrGraphWithGcQueueSparseDeleted, SegmentMaintainThresholds,
    VectorMemory,
    traits::CsrVertexTombstone,
};

type RowQueueGraph = CsrGraphWithGcQueueRowTombstone<
    TV,
    TE,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
    VectorMemory,
>;

type SparseQueueGraph = CsrGraphWithGcQueueSparseDeleted<
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

#[test]
fn row_tombstone_queue_supports_delete_vertex_and_gc_step() {
    let g = RowQueueGraph::format_new_with_gc_queue(
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
            soft_tombstone_score_threshold: 0.0,
            strict_tombstone_score_threshold: 0.0,
            ..Default::default()
        }),
    )
    .expect("format");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    g.insert_directed(0, 1, TE([1, 0, 0, 0])).unwrap();
    g.insert_directed(1, 0, TE([0, 0, 0, 0])).unwrap();

    g.delete_vertex(0).unwrap();
    assert!(g
        .graph()
        .forward_dgap()
        .vertices
        .get_dense(0)
        .unwrap()
        .is_tombstone());

    let raw_before_gc: Vec<_> = g.graph().out_edges(1).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(raw_before_gc, vec![TE([0, 0, 0, 0])]);

    let _ = g.gc_step(16).unwrap();
}

#[test]
fn sparse_deleted_logical_iter_hides_deleted_neighbors_immediately() {
    let g = SparseQueueGraph::format_new_with_gc_queue(
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

    g.delete_vertex(0).unwrap();

    let out1: Vec<_> = g
        .out_edges_logical(1)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(out1.is_empty());
}
