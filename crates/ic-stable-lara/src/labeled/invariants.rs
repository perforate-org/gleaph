//! Debug invariant helpers for labeled CSR layouts.

use super::bucket_store::LabelBucketStore;
use super::graph::leaf_pin::labeled_leaf_physical_block_len;
use super::record::LabelBucket;
use crate::{
    VertexId,
    labeled::{BucketLabelKey, LabeledVertex, slot_index::checked_add_slot_index},
    lara::{
        edge::{EdgeStore, span_meta::SPAN_PHYSICAL_UNASSIGNED},
        operation_error::LaraOperationError,
        vertex::VertexStore,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

/// Dense slab inline property bytes reads: no inline property bytes log and the inline-property-bytes-owned slab width
/// matches the live degree. Edge slab residency is intentionally irrelevant.
#[inline]
pub(crate) fn bucket_dense_slab_inline_property_bytes_readable(bucket: &LabelBucket) -> bool {
    bucket.is_inline_property_bytes_allocated()
        && bucket.inline_property_byte_width() > 0
        && bucket.inline_property_bytes_log_len() == 0
        && bucket.inline_property_bytes_slab_slots() == bucket.degree()
}

/// Dense inline property bytes batch traversal: no edge/inline property bytes logs and full slab residency.
#[inline]
pub(crate) fn bucket_dense_inline_property_batch_eligible(bucket: &LabelBucket) -> bool {
    // ADR 0096 §5: tiny buckets hold no values (width bytes are payload).
    if bucket.is_tiny_mode() {
        return false;
    }
    bucket.degree() > 0
        && bucket.inline_property_byte_width() > 0
        && bucket.inline_property_bytes_log_head() < 0
        && bucket.overflow_log_head() < 0
        && bucket.inline_property_bytes_slab_slots() == bucket.degree()
}

/// Contiguous ascending runs in a sorted slot list: `(first_slot, run_len)`.
pub(crate) fn ascending_contiguous_u32_runs(slots: &[u32]) -> Vec<(u32, u32)> {
    if slots.is_empty() {
        return Vec::new();
    }
    let mut runs = Vec::new();
    let mut first = slots[0];
    let mut count = 1u32;
    for &slot in &slots[1..] {
        if slot == first + count {
            count += 1;
        } else {
            runs.push((first, count));
            first = slot;
            count = 1;
        }
    }
    runs.push((first, count));
    runs
}

/// Byte offset of one fixed-width inline property bytes slot inside a bucket's dense slab span.
#[inline]
pub(crate) fn inline_property_bytes_byte_offset_at_slot(
    bucket: &LabelBucket,
    slot_index: u32,
) -> Result<u64, LaraOperationError> {
    bucket
        .inline_property_bytes_offset()
        .checked_add(u64::from(slot_index) * u64::from(bucket.inline_property_byte_width()))
        .ok_or(LaraOperationError::CollectAllocationOverflow)
}

/// Resident value bytes charged to a bucket's inline property bytes slab span.
#[inline]
pub(crate) fn bucket_resident_inline_property_bytes(bucket: &LabelBucket) -> u64 {
    if !bucket.is_inline_property_bytes_allocated() {
        return 0;
    }
    u64::from(bucket_resident_inline_property_bytes_slots(bucket))
        .saturating_mul(u64::from(bucket.inline_property_byte_width()))
}

#[inline]
pub(crate) fn bucket_resident_inline_property_bytes_slots(bucket: &LabelBucket) -> u32 {
    if !bucket.is_inline_property_bytes_allocated() || bucket.inline_property_byte_width() == 0 {
        return 0;
    }
    bucket.inline_property_bytes_slab_slots()
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
            // ADR 0096 §5: the vertex-span base is the first NON-TINY bucket
            // (tiny anchors are placeholders, not span bases).
            if !bucket.is_tiny_mode() {
                span_base.get_or_insert(bucket.edge_start());
            }
            // ADR 0096 §5: tiny successors hold no slab bytes — clamping to a
            // tiny anchor would zero the gap and exempt real slab spans from
            // containment. Skip forward to the next non-tiny bucket (tail when
            // none remains); tail anchors at the first non-tiny bucket.
            let mut successor_start: Option<u64> = None;
            let mut scan = offset.saturating_add(1);
            while scan < deg {
                let candidate = buckets
                    .read_label_bucket_slot(slot_at(
                        base_start,
                        scan,
                        &format!("vertex {vidx} bucket successor index"),
                    ))
                    .expect("bucket slot must exist");
                if !candidate.is_tiny_mode() {
                    successor_start = Some(candidate.edge_start());
                    break;
                }
                scan = scan.saturating_add(1);
            }
            let mut successor_start = match successor_start {
                Some(start) => start,
                None => {
                    let mut base = bucket.edge_start();
                    let mut back = 0u64;
                    while back <= offset {
                        let candidate = buckets
                            .read_label_bucket_slot(slot_at(
                                base_start,
                                back,
                                &format!("vertex {vidx} bucket tail anchor index"),
                            ))
                            .expect("bucket slot must exist");
                        if !candidate.is_tiny_mode() {
                            base = candidate.edge_start();
                            break;
                        }
                        back = back.saturating_add(1);
                    }
                    slot_end_exclusive(
                        base,
                        vertex.stored_slots,
                        &format!("vertex {vidx} tail bucket edge span"),
                    )
                }
            };
            successor_start = successor_start.max(bucket.edge_start());
            let gap = successor_start.saturating_sub(bucket.edge_start());
            // ADR 0096 §5: tiny buckets hold no slab bytes (inline targets);
            // their anchor is not inside any span.
            // ADR 0088 §2 + GAP-2026-09-17-001: a tree bucket is resident as its
            // root region (edge + property root), never as its logical
            // `stored_slots` width — that width counts LTB-block rows, which
            // occupy no LEG slots. Route through the resident-geometry SSOT.
            let on_slab_len = if bucket.is_tree_mode() {
                u64::from(crate::labeled::graph::bucket_physical_resident_slots(
                    &bucket,
                ))
            } else if bucket.is_tiny_mode() {
                0
            } else if bucket.overflow_log_head() < 0 {
                u64::from(bucket.stored_slots_raw())
            } else {
                gap.min(u64::from(bucket.stored_slots_raw()))
            };
            // Spanless buckets (tiny anchors, emptied spans) hold no slab
            // bytes; containment against capacity and the vertex span is
            // vacuous (their anchor/edge_start is a placeholder, not storage).
            // Wire rules below still apply.
            if on_slab_len > 0 {
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
            }
            if bucket.is_inline_property_bytes_allocated() {
                assert!(
                    bucket.inline_property_byte_width() > 0,
                    "vertex {vidx} bucket {slot}: value_allocated bucket must have non-zero width"
                );
            } else if bucket.is_tiny_mode() {
                // ADR 0096 §1 + delete redesign: tiny wire rules on live state.
                // `stored` = prefix width (≤ 3), `degree` = live (≤ stored);
                // dead-slot content is layout-native (E::tombstone_edge()
                // encoded, read via E::is_deleted_slot()).
                assert!(
                    bucket.degree() <= LabelBucket::TINY_MAX_DEGREE,
                    "vertex {vidx} bucket {slot}: tiny degree exceeds cap"
                );
                assert!(
                    bucket.stored_slots_raw() <= LabelBucket::TINY_MAX_DEGREE
                        && bucket.degree() <= bucket.stored_slots_raw(),
                    "vertex {vidx} bucket {slot}: tiny stored/degree out of range"
                );
                assert!(
                    bucket.overflow_log_head() < 0,
                    "vertex {vidx} bucket {slot}: tiny log head must be none"
                );
            }
            // ADR 0088 §2 + GAP-2026-09-20-001: tree mode has no overflow log, so
            // every live row must sit inside the transcribed prefix. `degree >
            // stored_slots` is the shape produced by orphaning log-resident rows
            // at promotion (the rows stayed in `degree` but vanished from every
            // scan). Checked outside the branch chain above because a tree bucket
            // with inline properties also reports `inline_property_bytes_allocated`.
            if bucket.is_tree_mode() {
                assert!(
                    bucket.overflow_log_head() < 0,
                    "vertex {vidx} bucket {slot}: tree log head must be none"
                );
                assert!(
                    bucket.degree() <= bucket.stored_slots_raw(),
                    "vertex {vidx} bucket {slot}: tree degree {} exceeds stored width {}",
                    bucket.degree(),
                    bucket.stored_slots_raw()
                );
            }
        }
        let mut resident_inline_property_bytes = 0u64;
        for offset in 0..deg {
            let slot = slot_at(
                base_start,
                offset,
                &format!("vertex {vidx} value accounting bucket index"),
            );
            let bucket = buckets
                .read_label_bucket_slot(slot)
                .expect("bucket slot must exist");
            resident_inline_property_bytes = resident_inline_property_bytes
                .saturating_add(bucket_resident_inline_property_bytes(&bucket));
        }
        assert_eq!(
            vertex.inline_property_bytes_allocated_bytes(),
            resident_inline_property_bytes,
            "vertex {vidx}: inline_property_bytes_allocated_bytes must equal sum of resident bucket value spans"
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
        // ADR 0096 §5: tiny edges are excluded from leaf `actual` (they occupy
        // no leaf slots), so the geometry recomputation must skip them too —
        // otherwise the audit would demand counting what the density contract
        // forbids.
        if bucket.is_tiny_mode() {
            continue;
        }
        // ADR 0096 §5 (LARA parity) is the same contract for tree mode: tree
        // rows live in LTB blocks, so a tree bucket contributes zero. Promote
        // (`promote_bypass_to_tree_mode`) subtracts the live degree and demote
        // (`tree_mode_demote_to_slab`) re-adds it; tree inserts/removes never
        // adjust `actual` (see `tree_write` module header).
        if bucket.is_tree_mode() {
            continue;
        }
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
        let leaf = (vidx / seg) as usize;
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
            let span_rec = edges.span_meta_store().get(u64::from(leaf));
            let expected_total = if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
                per_leaf_total[leaf as usize]
                    .max(i64::try_from(labeled_leaf_physical_block_len(seg)).unwrap_or(i64::MAX))
            } else {
                per_leaf_total[leaf as usize]
            };
            assert_eq!(
                got.total, expected_total,
                "leaf {leaf}: PMA total mismatch (store vs labeled geometry)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::labeled::record::LabelBucket;

    #[test]
    fn ascending_contiguous_u32_runs_groups_runs() {
        assert!(ascending_contiguous_u32_runs(&[]).is_empty());
        assert_eq!(
            ascending_contiguous_u32_runs(&[0, 1, 2, 5, 7, 8]),
            vec![(0, 3), (5, 1), (7, 2)]
        );
    }

    #[test]
    fn inline_property_bytes_byte_offset_at_slot_scales_by_width() {
        let bucket = LabelBucket::default()
            .with_inline_property_bytes_offset(128)
            .with_inline_property_byte_width(4);
        assert_eq!(
            inline_property_bytes_byte_offset_at_slot(&bucket, 0).unwrap(),
            128
        );
        assert_eq!(
            inline_property_bytes_byte_offset_at_slot(&bucket, 3).unwrap(),
            140
        );
    }
}
