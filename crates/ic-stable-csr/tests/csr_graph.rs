//! [`CsrGraphRowTombstone::format_new`] and bidirectional insert / neighborhood iterators.

mod common;

use common::{
    TestEdge as TE, TestVertex as TV, assert_dense_vertex_bases_non_decreasing, empty_vertex, vm,
};
use ic_stable_csr::{
    CsrGraphError, CsrGraphRowTombstone, VectorMemory, VertexId,
    traits::{CsrEdge, CsrEdgeUndirected},
};

type CsrTestGraph =
    CsrGraphRowTombstone<TV, TE, VectorMemory, VectorMemory, VectorMemory, VectorMemory, VectorMemory>;

fn assert_csr_fwd_rev_bases_non_decreasing(g: &CsrTestGraph) {
    assert_dense_vertex_bases_non_decreasing(g.forward_dgap());
    assert_dense_vertex_bases_non_decreasing(g.reverse_dgap());
}

#[test]
fn format_new_directed_transpose_neighbors() {
    let g =
        CsrGraphRowTombstone::format_new(
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
        )
        .expect("format_new");

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();
    assert_csr_fwd_rev_bases_non_decreasing(&g);

    g.insert_directed(VertexId(0), VertexId(1), TE([1, 0, 0, 0]))
        .unwrap();
    g.insert_directed(VertexId(1), VertexId(2), TE([2, 0, 0, 0]))
        .unwrap();
    assert_csr_fwd_rev_bases_non_decreasing(&g);

    let out0: Vec<_> = g
        .out_edges(VertexId(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(out0, vec![TE([1, 0, 0, 0])]);

    let out1: Vec<_> = g
        .out_edges(VertexId(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(out1, vec![TE([2, 0, 0, 0])]);

    let in1: Vec<_> = g
        .in_edges(VertexId(1))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(in1, vec![TE([0, 0, 0, 0])]);

    let in2: Vec<_> = g
        .in_edges(VertexId(2))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(in2, vec![TE([1, 0, 0, 0])]);
}

#[test]
fn insert_directed_rejects_undirected_flag_via_specialization() {
    let g = CsrGraphRowTombstone::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 32, 1, 8, 0)
        .unwrap();

    g.insert_vertex(empty_vertex()).unwrap();
    g.insert_vertex(empty_vertex()).unwrap();
    g.sync_pma_meta().unwrap();

    let e = TE([1, 1, 0, 0]).with_undirected(true);
    let err = g
        .insert_directed(VertexId(0), VertexId(1), e)
        .unwrap_err();
    assert_eq!(err, CsrGraphError::UndirectedEdgeInDirectedInsert);
    assert_csr_fwd_rev_bases_non_decreasing(&g);
}

#[test]
fn insert_undirected_sets_flag_and_symmetric_degrees() {
    let g = CsrGraphRowTombstone::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 128, 1, 8, 0)
        .unwrap();

    for _ in 0..3 {
        g.insert_vertex(empty_vertex()).unwrap();
    }
    g.sync_pma_meta().unwrap();

    g.insert_undirected(
        VertexId(0),
        VertexId(2),
        TE([0, 0, 0, 0]).with_neighbor_vid(VertexId(2)),
    )
    .unwrap();
    assert_csr_fwd_rev_bases_non_decreasing(&g);

    let out0: Vec<_> = g
        .out_edges(VertexId(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(out0.len(), 1);
    assert_eq!(out0[0].neighbor_vid(), VertexId(2));
    assert!(out0[0].is_undirected());

    let out2: Vec<_> = g
        .out_edges(VertexId(2))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(out2.len(), 1);
    assert_eq!(out2[0].neighbor_vid(), VertexId(0));
    assert!(out2[0].is_undirected());

    let in2: Vec<_> = g
        .in_edges(VertexId(2))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(in2.len(), 1);
    assert_eq!(in2[0].neighbor_vid(), VertexId(0));

    let in0: Vec<_> = g
        .in_edges(VertexId(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(in0.len(), 1);
    assert_eq!(in0[0].neighbor_vid(), VertexId(2));
    assert_csr_fwd_rev_bases_non_decreasing(&g);
}

#[test]
fn neighbor_mismatch_on_directed_insert() {
    let g = CsrGraphRowTombstone::format_new(vm(), vm(), vm(), vm(), vm(), vm(), 32, 1, 8, 0)
        .unwrap();

    g.insert_vertex(empty_vertex()).unwrap();
    g.insert_vertex(empty_vertex()).unwrap();
    g.sync_pma_meta().unwrap();
    assert_csr_fwd_rev_bases_non_decreasing(&g);

    let err = g
        .insert_directed(VertexId(0), VertexId(1), TE([9, 0, 0, 0]))
        .unwrap_err();
    assert_eq!(
        err,
        CsrGraphError::NeighborMismatch {
            expected: VertexId(1),
            actual: VertexId(9)
        }
    );
    assert_csr_fwd_rev_bases_non_decreasing(&g);
}
