mod common;

use common::{TestEdge as TE, TestVertex as TV, empty_vertex, vm};
use ic_stable_csr::{
    CsrGraphWithGcQueueDenseDeleted, CsrGraphWithGcQueueRowTombstone,
    CsrGraphWithGcQueueSparseDeleted, SegmentMaintainThresholds,
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

type DenseQueueGraph = CsrGraphWithGcQueueDenseDeleted<
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

#[test]
fn row_queue_open_existing_preserves_edges_and_queue_state() {
    let m0 = vm();
    let m1 = vm();
    let m2 = vm();
    let m3 = vm();
    let m4 = vm();
    let m5 = vm();
    let mq = vm();

    {
        let g = RowQueueGraph::format_new_with_gc_queue(
            m0.clone(),
            m1.clone(),
            m2.clone(),
            m3.clone(),
            m4.clone(),
            m5.clone(),
            mq.clone(),
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
    }

    let reopened = RowQueueGraph::open_existing_with_gc_queue(
        m0,
        m1,
        m2,
        m3,
        m4,
        m5,
        mq,
        Some(SegmentMaintainThresholds {
            soft_tombstone_score_threshold: 0.0,
            strict_tombstone_score_threshold: 0.0,
            ..Default::default()
        }),
    )
    .expect("reopen");
    assert_eq!(reopened.graph().vertex_count(), 3);
    let raw0: Vec<_> = reopened
        .graph()
        .out_edges(0)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(raw0, vec![TE([1, 0, 0, 0])]);
}

#[test]
fn dense_queue_open_existing_preserves_deleted_index_and_logical_view() {
    let m0 = vm();
    let m1 = vm();
    let m2 = vm();
    let m3 = vm();
    let m4 = vm();
    let m5 = vm();
    let md = vm();
    let mq = vm();

    let queue_before_reopen = {
        let g = DenseQueueGraph::format_new_with_gc_queue(
            m0.clone(),
            m1.clone(),
            m2.clone(),
            m3.clone(),
            m4.clone(),
            m5.clone(),
            md.clone(),
            mq.clone(),
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
        g.insert_directed(1, 0, TE([0, 0, 0, 0])).unwrap();
        g.delete_vertex(0).unwrap();
        g.work_queue_len()
    };

    let reopened = DenseQueueGraph::open_existing_with_gc_queue(
        m0,
        m1,
        m2,
        m3,
        m4,
        m5,
        md,
        mq,
        Some(SegmentMaintainThresholds {
            soft_tombstone_score_threshold: 0.0,
            strict_tombstone_score_threshold: 0.0,
            ..Default::default()
        }),
    )
    .expect("reopen");
    assert_eq!(reopened.graph().vertex_count(), 3);
    assert_eq!(reopened.work_queue_len(), queue_before_reopen);
    let logical: Vec<_> = reopened
        .out_edges_logical(1)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert!(logical.is_empty());
}
