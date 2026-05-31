//! Debug invariant helpers for labeled CSR layouts.

use super::bucket_store::LabelBucketStore;
use super::record::LabelBucket;
use crate::{
    VertexId,
    labeled::{BucketLabelKey, LabeledVertex, slot_index::checked_add_slot_index},
    lara::{edge::EdgeStore, vertex::VertexStore},
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

/// Resident value bytes charged to a bucket's slab span (`stored_slots × width`, or `degree` when larger).
#[inline]
pub(crate) fn bucket_resident_payload_bytes(bucket: &LabelBucket) -> u64 {
    if !bucket.is_payload_allocated() {
        return 0;
    }
    u64::from(bucket.stored_slots.max(bucket.degree))
        .saturating_mul(u64::from(bucket.payload_byte_width()))
}

#[inline]
fn slot_end_exclusive(base: u64, width: u32, context: &str) -> u64 {
    checked_add_slot_index(base, u64::from(width))
        .unwrap_or_else(|| panic!("{context}: slot index overflow (base={base}, width={width})"))
}

#[inline]
fn slot_at(base: u64, offset: u64, context: &str) -> u64 {
    checked_add_slot_index(base, offset)
        .unwrap_or_else(|| panic!("{context}: slot index overflow (base={base}, offset={offset})"))
}

/// Asserts that every vertex row and LabelBucket exposes valid clean-scan ranges.
pub(crate) fn assert_labeled_layout_invariants<E, M>(
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
        if !vertex.is_default_edge_labeled() {
            assert!(
                LabeledVertex::label_bucket_count_fits(vertex.degree()),
                "vertex {vidx}: label bucket count {} exceeds MAX_VERTEX_LABEL_BUCKETS",
                vertex.degree()
            );
            assert!(
                vertex.label_bucket_descriptor_span().is_some(),
                "vertex {vidx}: label bucket descriptor span (degree + slack) overflows u32",
            );
        }
        if vertex.is_default_edge_labeled() {
            let end = slot_end_exclusive(
                vertex.base_slot_start(),
                vertex.stored_degree(),
                &format!("vertex {vidx} default-edge bypass range"),
            );
            assert!(
                end <= edge_cap,
                "vertex {vidx}: default edge range [{}, {end}) exceeds edge capacity {edge_cap}",
                vertex.base_slot_start()
            );
            continue;
        }
        let bucket_end = slot_end_exclusive(
            vertex.base_slot_start(),
            vertex
                .label_bucket_descriptor_span()
                .expect("normal vertex must have descriptor span"),
            &format!("vertex {vidx} bucket descriptor span"),
        );
        assert!(
            bucket_end <= bucket_cap,
            "vertex {vidx}: bucket allocation [{}, {bucket_end}) exceeds bucket capacity {bucket_cap}",
            vertex.base_slot_start()
        );
        let mut previous_label: Option<BucketLabelKey> = None;
        let mut span_base = None;
        let base_start = vertex.base_slot_start();
        let deg = vertex.degree() as u64;
        for offset in 0..deg {
            let slot = slot_at(
                base_start,
                offset,
                &format!("vertex {vidx} bucket descriptor index"),
            );
            let bucket = buckets
                .read_label_bucket_slot(slot)
                .expect("bucket slot must exist");
            if let Some(previous) = previous_label {
                assert!(
                    previous < bucket.bucket_label_key(),
                    "vertex {vidx}: label buckets must be strictly sorted by BucketLabelKey"
                );
            }
            previous_label = Some(bucket.bucket_label_key());
            span_base.get_or_insert(bucket.edge_start());
            let mut successor_start = if offset.saturating_add(1) < deg {
                buckets
                    .read_label_bucket_slot(slot_at(
                        base_start,
                        offset.saturating_add(1),
                        &format!("vertex {vidx} bucket successor index"),
                    ))
                    .expect("bucket slot must exist")
                    .edge_start()
            } else {
                let first = buckets
                    .read_label_bucket_slot(base_start)
                    .expect("bucket slot must exist");
                slot_end_exclusive(
                    first.edge_start(),
                    vertex.stored_slots,
                    &format!("vertex {vidx} tail bucket edge span"),
                )
            };
            successor_start = successor_start.max(bucket.edge_start());
            let gap = successor_start.saturating_sub(bucket.edge_start());
            let on_slab_len = if bucket.overflow_log_head() < 0 {
                u64::from(bucket.stored_slots)
            } else {
                gap.min(u64::from(bucket.stored_slots))
            };
            let edge_end_physical = checked_add_slot_index(bucket.edge_start(), on_slab_len)
                .unwrap_or_else(|| {
                    panic!(
                        "vertex {vidx} bucket {slot}: on-slab edge range overflow (start={}, len={on_slab_len})",
                        bucket.edge_start()
                    )
                });
            assert!(
                edge_end_physical <= edge_cap,
                "vertex {vidx} bucket {slot}: on-slab edge range [{}, {edge_end_physical}) exceeds edge capacity {edge_cap}",
                bucket.edge_start()
            );
            if let Some(base) = span_base {
                let span_end = slot_end_exclusive(
                    base,
                    vertex.stored_slots,
                    &format!("vertex {vidx} bucket {slot} VertexEdgeSpan"),
                );
                assert!(
                    edge_end_physical <= span_end,
                    "vertex {vidx} bucket {slot}: on-slab edge range [{}, {edge_end_physical}) exceeds VertexEdgeSpan [{base}, {span_end})",
                    bucket.edge_start()
                );
            }
            if bucket.is_payload_allocated() {
                assert!(
                    bucket.payload_byte_width() > 0,
                    "vertex {vidx} bucket {slot}: value_allocated bucket must have non-zero width"
                );
            }
        }
        let mut resident_payload_bytes = 0u64;
        for offset in 0..deg {
            let slot = slot_at(
                base_start,
                offset,
                &format!("vertex {vidx} value accounting bucket index"),
            );
            let bucket = buckets
                .read_label_bucket_slot(slot)
                .expect("bucket slot must exist");
            resident_payload_bytes =
                resident_payload_bytes.saturating_add(bucket_resident_payload_bytes(&bucket));
        }
        assert_eq!(
            vertex.payload_allocated_bytes(),
            resident_payload_bytes,
            "vertex {vidx}: payload_allocated_bytes must equal sum of resident bucket value spans"
        );
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
        return (0, i64::from(vertex.stored_slots));
    }
    let mut live = 0i64;
    let start = vertex.base_slot_start();
    for i in 0..vertex.degree() {
        let bucket = buckets
            .read_label_bucket_slot(slot_at(
                start,
                u64::from(i),
                "expected_vertex_pma_contribution bucket index",
            ))
            .expect("bucket slot must exist");
        live += i64::from(bucket.degree());
    }
    (live, i64::from(vertex.stored_slots))
}

/// Asserts incremental PMA leaf [`crate::lara::edge::counts::SegmentEdgeCounts`] match
/// per-vertex labeled/default-bypass expectations (no double-counting between modes).
pub(crate) fn assert_labeled_edge_store_pma_counts<E, M>(
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
