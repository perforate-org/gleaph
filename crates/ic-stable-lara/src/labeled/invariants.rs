//! Debug invariant helpers for labeled CSR layouts.

use crate::{
    VertexId,
    labeled::{
        bucket_store::LabelBucketStore,
        record::{LabelId, LabeledVertex},
    },
    lara::{edge::EdgeStore, vertex::VertexStore},
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

/// Asserts that every vertex row and LabelBucket exposes valid clean-scan ranges.
pub fn assert_labeled_layout_invariants<E, M>(
    vertices: &VertexStore<LabeledVertex, M>,
    buckets: &LabelBucketStore<M>,
    edges: &EdgeStore<E, M>,
) where
    E: CsrEdge,
    M: Memory,
{
    let edge_cap = edges.header().elem_capacity;
    let bucket_cap = buckets.header().elem_capacity;
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
        let bucket_alloc = vertex.bucket_alloc_slots().max(vertex.degree());
        let bucket_end = vertex
            .base_slot_start()
            .saturating_add(u64::from(bucket_alloc));
        assert!(
            bucket_end <= bucket_cap,
            "vertex {vidx}: bucket allocation [{}, {bucket_end}) exceeds bucket capacity {bucket_cap}",
            vertex.base_slot_start()
        );
        let mut previous_label: Option<LabelId> = None;
        let mut span_base = None;
        let base_start = vertex.base_slot_start();
        let deg = vertex.degree() as u64;
        for offset in 0..deg {
            let slot = base_start.saturating_add(offset);
            let bucket = buckets
                .read_label_bucket_slot(slot)
                .expect("bucket slot must exist");
            if let Some(previous) = previous_label {
                assert!(
                    previous < bucket.label_id,
                    "vertex {vidx}: label buckets must be strictly sorted by LabelId"
                );
            }
            previous_label = Some(bucket.label_id);
            span_base.get_or_insert(bucket.edge_start);
            let mut successor_start = if offset.saturating_add(1) < deg {
                buckets
                    .read_label_bucket_slot(slot.saturating_add(1))
                    .expect("bucket slot must exist")
                    .edge_start
            } else {
                let first = buckets
                    .read_label_bucket_slot(base_start)
                    .expect("bucket slot must exist");
                first
                    .edge_start
                    .saturating_add(u64::from(vertex.vertex_edge_alloc_slots()))
            };
            successor_start = successor_start.max(bucket.edge_start);
            let gap = successor_start.saturating_sub(bucket.edge_start);
            let on_slab_len = if bucket.overflow_log_head < 0 {
                u64::from(bucket.edge_len)
            } else {
                gap.min(u64::from(bucket.edge_len))
            };
            let edge_end_physical = bucket.edge_start.saturating_add(on_slab_len);
            assert!(
                edge_end_physical <= edge_cap,
                "vertex {vidx} bucket {slot}: on-slab edge range [{}, {edge_end_physical}) exceeds edge capacity {edge_cap}",
                bucket.edge_start
            );
            if let Some(base) = span_base {
                let span_end = base.saturating_add(u64::from(vertex.vertex_edge_alloc_slots()));
                assert!(
                    edge_end_physical <= span_end,
                    "vertex {vidx} bucket {slot}: on-slab edge range [{}, {edge_end_physical}) exceeds VertexEdgeSpan [{base}, {span_end})",
                    bucket.edge_start
                );
            }
        }
    }
}

fn expected_vertex_pma_contribution<M>(
    vertices: &VertexStore<LabeledVertex, M>,
    buckets: &LabelBucketStore<M>,
    vid: VertexId,
) -> (i64, i64)
where
    M: Memory,
{
    let vertex = vertices.get(vid);
    if vertex.is_tombstone() {
        return (0, 0);
    }
    if vertex.is_default_edge_labeled() {
        let d = i64::from(vertex.degree());
        // Core `EdgeStore::insert_edge` bumps only `actual`; `total` is not advanced
        // incrementally for bypass rows (unlike normal labeled `VertexEdgeSpan` reservations).
        return (d, 0);
    }
    if vertex.degree() == 0 {
        return (0, i64::from(vertex.vertex_edge_alloc_slots()));
    }
    let mut live = 0i64;
    let start = vertex.base_slot_start();
    for i in 0..vertex.degree() {
        let bucket = buckets
            .read_label_bucket_slot(start.saturating_add(u64::from(i)))
            .expect("bucket slot must exist");
        live += i64::from(bucket.edge_len);
    }
    (live, i64::from(vertex.vertex_edge_alloc_slots()))
}

/// Asserts incremental PMA leaf [`crate::lara::edge::counts::SegmentEdgeCounts`] match
/// per-vertex labeled/default-bypass expectations (no double-counting between modes).
pub fn assert_labeled_edge_store_pma_counts<E, M>(
    vertices: &VertexStore<LabeledVertex, M>,
    buckets: &LabelBucketStore<M>,
    edges: &EdgeStore<E, M>,
) where
    E: CsrEdge,
    M: Memory,
{
    let header = edges.header();
    let seg = header.segment_size.max(1);
    let mut per_leaf_actual = vec![0i64; header.segment_count as usize];
    let mut per_leaf_total = vec![0i64; header.segment_count as usize];
    for vidx in 0..vertices.len() {
        let vid = VertexId::from(vidx);
        let leaf = (vidx as u32 / seg) as usize;
        if leaf >= per_leaf_actual.len() {
            continue;
        }
        let (a, t) = expected_vertex_pma_contribution(vertices, buckets, vid);
        per_leaf_actual[leaf] += a;
        per_leaf_total[leaf] += t;
    }
    for leaf in 0..header.segment_count {
        let idx = u64::from(leaf + header.segment_count);
        let got = edges.counts_store().get(idx);
        assert_eq!(
            got.actual, per_leaf_actual[leaf as usize],
            "leaf {leaf}: PMA actual mismatch (store vs labeled geometry)"
        );

        let start_vid = leaf.saturating_mul(seg) as usize;
        let end_vid = ((leaf + 1).saturating_mul(seg) as usize).min(vertices.len() as usize);
        let leaf_has_bypass = (start_vid..end_vid).any(|vidx| {
            vertices
                .get(VertexId::from(vidx as u32))
                .is_default_edge_labeled()
        });
        if !leaf_has_bypass {
            assert_eq!(
                got.total, per_leaf_total[leaf as usize],
                "leaf {leaf}: PMA total mismatch (store vs labeled geometry)"
            );
        }
    }
}
