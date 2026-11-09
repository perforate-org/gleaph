//! Fixture helpers that force a specific bucket geometry.
//!
//! Compiled for the crate's own tests and, under the `test-support` feature, for a dependent
//! crate's tests. It exists because a bucket that carries inline property bytes cannot be given a
//! span from outside: those inserts go through the deferred path, which skips the leaf cascade and
//! spills from the first row, and the maintenance fold resolves `SlackMayBeDropped`, which refuses
//! to grow the leaf for rows that already live in the run. The rows therefore never reach the span
//! a fixture wants to test.

use super::*;
use crate::traits::CsrEdgeTombstone;

/// Folds one bucket's spill run into its slab span, growing the leaf when it must.
///
/// This is the eager `SpanResolutionPolicy::Required` compaction the write path uses, exposed so a
/// fixture can reach a span-resident bucket that holds inline property values.
pub fn force_span_fold<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    _bucket_index: u32,
) -> Result<(), LabeledOperationError> {
    graph.vertex_layout(vid).compact_span()
}

/// Every stored row of one bucket's spill run, as `(index, neighbour)`.
///
/// Unlike a print at a write site, this reads what the run actually holds, which is the only
/// observation that matched the read-back in earlier investigations.
pub fn dump_spill_rows<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Vec<(u32, VertexId)> {
    let vertex = graph.vertices().get(vid);
    let bucket = match graph.find_bucket(vid, &vertex, label).expect("find bucket") {
        crate::labeled::graph::BucketSearch::Found { bucket, .. } => bucket,
        crate::labeled::graph::BucketSearch::Missing { .. } => return Vec::new(),
    };
    let Ok(run) = graph.edges.spill_mint(bucket.edge_spill_run()) else {
        return Vec::new();
    };
    let used = graph.edges.spill_used_len(run);
    (0..used)
        .map(|index| {
            let edge = graph
                .edges
                .spill_edge_at(run, index)
                .expect("spill row readable");
            (index, edge.neighbor_vid())
        })
        .collect()
}

/// One bucket's physical geometry: `(edge_start, stored_slots, degree, spill_run)`.
pub fn bucket_geometry<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> Option<(u64, u32, u32, u32)> {
    let vertex = graph.vertices().get(vid);
    match graph.find_bucket(vid, &vertex, label).expect("find bucket") {
        crate::labeled::graph::BucketSearch::Found { bucket, .. } => Some((
            bucket.edge_start(),
            bucket.stored_slots(),
            bucket.degree,
            bucket.edge_spill_run(),
        )),
        crate::labeled::graph::BucketSearch::Missing { .. } => None,
    }
}

/// Every layout error `LeafArena::validate` reports for the leaf that holds `vid`.
///
/// Includes the base-versus-allocation check ADR 0098 adds, which the derived slice partition
/// cannot express.
pub fn leaf_layout_errors<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
) -> Vec<String> {
    graph.vertex_layout(vid).validate()
}

/// `(vid, base_slot_start, stored_slots, first non-tiny bucket edge_start)` for every vertex of the
/// leaf that holds `vid`. Used to see how placement assigned the bases.
pub fn leaf_vertex_rows<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
) -> Vec<(u32, u64, u32, Option<u64>)> {
    let header = graph.edges.header();
    let seg = header.segment_size.max(1);
    let leaf = LabeledLaraGraph::<E, M>::leaf_index_for_vid(vid, seg);
    let start = leaf.saturating_mul(seg);
    let end = start.saturating_add(seg).min(graph.vertices.len());
    (start..end)
        .map(|ordinal| {
            let vertex_id = VertexId::from(ordinal);
            let vertex = graph.vertices.get(vertex_id);
            let bucket_start = graph
                .read_vertex_label_buckets(&vertex)
                .ok()
                .and_then(|buckets| {
                    buckets
                        .iter()
                        .find(|bucket| !bucket.is_tiny_mode())
                        .map(|bucket| bucket.edge_start())
                });
            (
                ordinal,
                vertex.base_slot_start(),
                vertex.stored_slots,
                bucket_start,
            )
        })
        .collect()
}
