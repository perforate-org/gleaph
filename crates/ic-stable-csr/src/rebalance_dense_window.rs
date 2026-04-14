//! `rebalance_weighted` on a PMA **RebalanceWindow** (density path) without going through `resize_double`.
//!
//! Lives under `#[cfg(test)]` in the library because [`crate::dgap::DgapEdgeStore::insert_edge_skip_maintain_for_test`]
//! is crate-local test-only; integration tests build this crate without `cfg(test)`.

#[path = "../tests/common/mod.rs"]
mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{
    TestEdge as TE, TestVertex as TV, assert_dense_vertex_bases_non_decreasing, dual_edge_memories,
};
use crate::{
    CsrEdgeSlotTombstoneScan, DgapEdgeStore, DgapStores, SegmentEdgeCounts, StableVec,
    VectorMemory, VertexCount, VertexId,
    dgap::{RebalanceDecision, rebalance_decision, recount_segment_edge_counts_column},
    traits::CsrEdge,
};

type TeStore = DgapEdgeStore<TE, VectorMemory, VectorMemory>;

fn sec_actual_total_for_decision(
    stores: &DgapStores<TV, TE, VectorMemory, VectorMemory, VectorMemory>,
) -> (Vec<i64>, Vec<i64>) {
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
    let actual: Vec<i64> = buf.iter().map(|c| c.actual).collect();
    let total: Vec<i64> = buf.iter().map(|c| c.total).collect();
    (actual, total)
}

fn assert_sec_matches_recount(
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

/// [`crate::DgapStores::insert_edge`] runs [`crate::dgap::DgapEdgeStore::maintain_rebalance_loop`] and clears a
/// [`RebalanceDecision::RebalanceWindow`] before returning, so a post-insert `rebalance_decision`
/// snapshot is usually `Noop`. Here we use [`crate::dgap::DgapEdgeStore::insert_edge_skip_maintain_for_test`] to
/// pack segment 0 until full recount + `rebalance_decision` yields `RebalanceWindow`, then call
/// [`crate::dgap::DgapEdgeStore::rebalance_weighted`] explicitly.
#[test]
fn rebalance_weighted_direct_preserves_dense_bases_after_rebalance_window() {
    let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
    let vertices = StableVec::new(mv);
    let edges = TeStore::new(dual_edge_memories());
    edges.format_new(512, 2, 4, 0).expect("format_new");
    let stores = DgapStores::new(vertices, edges);
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    for _ in 0..8 {
        stores
            .insert_vertex(TV {
                slot_base: 0,
                deg: 0,
                log_head: -1,
            })
            .unwrap();
    }
    stores.refresh_slab_occupied_tail_meta().unwrap();
    stores.sync_pma_meta().unwrap();

    for vid in 4..8usize {
        stores
            .insert_edge(vid, TE([0, 0, 0, 0]).with_neighbor_vid(0))
            .unwrap();
    }

    const MAX_EXTRA: usize = 20_000;
    let mut extra = 0usize;
    let (left, right, pma_idx) = loop {
        stores.refresh_slab_occupied_tail_meta().unwrap();
        stores.sync_pma_meta().unwrap();
        let h = stores.edges.header().unwrap();
        let n = stores.vertices.len() as usize;
        let (actual, total) = sec_actual_total_for_decision(&stores);
        let dec = rebalance_decision(
            0,
            h.segment_size,
            h.segment_count,
            n,
            h.tree_height,
            &actual,
            &total,
        );
        match dec {
            RebalanceDecision::RebalanceWindow {
                left_vertex,
                right_vertex,
                pma_idx,
            } => break (left_vertex, right_vertex, pma_idx),
            RebalanceDecision::ResizeNeeded => {
                panic!(
                    "ResizeNeeded before RebalanceWindow; increase elem_capacity in this test (extra={extra})"
                );
            }
            RebalanceDecision::Noop => {}
        }
        assert!(
            extra < MAX_EXTRA,
            "failed to reach RebalanceWindow in {MAX_EXTRA} inserts; see tests/common/mod.rs rebalance note / plan A2"
        );
        let src = extra % 4;
        let dst = 4 + (extra % 4);
        stores
            .edges
            .insert_edge_skip_maintain_for_test(
                &stores.vertices,
                src,
                TE([0, 0, 0, 0]).with_neighbor_vid(dst),
            )
            .unwrap();
        extra += 1;
    };

    assert_sec_matches_recount(&stores);
    assert_dense_vertex_bases_non_decreasing(&stores);

    stores
        .edges
        .rebalance_weighted(
            &stores.vertices,
            left as VertexId,
            right as VertexCount,
            pma_idx,
        )
        .expect("rebalance_weighted");

    stores.sync_pma_meta().unwrap();
    assert_sec_matches_recount(&stores);
    assert_dense_vertex_bases_non_decreasing(&stores);
}
