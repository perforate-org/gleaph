//! Debug invariant helpers for labeled CSR layouts.

use crate::{
    VertexId,
    labeled::{
        edge_slab::EdgeSlabStore,
        record::{LabelBucket, LabeledVertex},
        row_store::RowStore,
        traits::LabeledCsrVertex,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

/// Asserts that every vertex row and bucket row exposes valid clean-scan ranges.
pub fn assert_labeled_layout_invariants<E, M>(
    vertices: &RowStore<LabeledVertex, M>,
    buckets: &RowStore<LabelBucket, M>,
    edges: &EdgeSlabStore<E, M>,
) where
    E: CsrEdge,
    M: Memory,
{
    let edge_cap = edges.header().elem_capacity;
    for vidx in 0..vertices.len() {
        let vertex = vertices.get(VertexId::from(vidx));
        if vertex.is_default_edge_labeled() {
            let end = vertex
                .base_slot_start()
                .saturating_add(u64::from(vertex.degree()));
            assert!(
                end <= edge_cap,
                "vertex {vidx}: default edge range [{}, {end}) exceeds edge capacity {edge_cap}",
                vertex.base_slot_start()
            );
            continue;
        }
        let bucket_end = vertex
            .base_slot_start()
            .saturating_add(u64::from(vertex.degree()));
        assert!(
            bucket_end <= u64::from(buckets.len()),
            "vertex {vidx}: bucket range [{}, {bucket_end}) exceeds bucket len {}",
            vertex.base_slot_start(),
            buckets.len()
        );
        for bidx in vertex.base_slot_start()..bucket_end {
            let bucket = buckets.get(VertexId::from(bidx as u32));
            let edge_end = bucket.edge_start.saturating_add(u64::from(bucket.edge_len));
            assert!(
                edge_end <= edge_cap,
                "vertex {vidx} bucket {bidx}: edge range [{}, {edge_end}) exceeds edge capacity {edge_cap}",
                bucket.edge_start
            );
        }
    }
}
