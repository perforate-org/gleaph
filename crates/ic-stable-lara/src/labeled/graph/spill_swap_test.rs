//! Spill-swap tests: the behaviors ADR 0097 introduces, exercised end to end.
//!
//! The store's own allocator tests live in `labeled::spill_run_store`; these cover the layer above -
//! the descriptor, the producer, the delete, compaction, promotion, the batch boundary and the
//! scans - so each mechanism the swap added has a witness outside its own module.

use super::test_support::*;
use super::*;
use crate::traits::{CsrEdgeTombstone, NO_SPILL_RUN};
use crate::{VertexId, labeled::BucketLabelKey};
use std::ops::ControlFlow;

/// A graph whose buckets have no slab headroom (`segment_size` 1 leaves the initial bucket quota at
/// 0), so every edge beyond a tiny bucket's four inline slots lands in the bucket's spill run.
fn spilling_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    test_graph()
}

fn bucket_of<E: CsrEdge>(
    graph: &LabeledLaraGraph<E, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
) -> LabelBucket {
    let vertex = graph.vertices().get(src);
    let slot = graph.find_bucket_slot(&vertex, label).unwrap().unwrap();
    graph.buckets().read_label_bucket_slot(slot).unwrap()
}

fn run_rows<E: CsrEdge>(
    graph: &LabeledLaraGraph<E, crate::VectorMemory>,
    bucket: &LabelBucket,
) -> u32 {
    match graph.edges().spill_mint(bucket.edge_spill_run()) {
        Ok(run) => graph.edges().spill_used_len(run),
        Err(_) => 0,
    }
}

fn insert_many<E: CsrEdgeTombstone>(
    graph: &LabeledLaraGraph<E, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    targets: impl IntoIterator<Item = u32>,
) {
    for target in targets {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                E::read_from(&target.to_le_bytes()),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
}

#[test]
fn a_bucket_with_slab_headroom_owns_no_run() {
    // ADR 0097 §4: lazy allocation - nothing is reserved before the first row that does not fit.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, [10, 11]);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(run_rows(&graph, &bucket), 0);
}

#[test]
fn the_first_row_beyond_the_prefix_allocates_exactly_one_class() {
    // The run is minted for exactly this row: `class_rows(1)` rows plus the run's header row.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..6);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_ne!(bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(run_rows(&graph, &bucket), 1);
    assert_eq!(bucket.degree(), 6);
    // The tiny->slab promotion gave the prefix five rows; the sixth is the run's row 0.
}

#[test]
fn resident_width_is_the_prefix_plus_the_runs_rows() {
    // ADR 0097 §8: resident content is `prefix + spill`, one definition shared by sizing and publish.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..7);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(
        graph.bucket_reserved_edge_slots(VertexId::from(0), &bucket),
        bucket.stored_slots() + run_rows(&graph, &bucket)
    );
    assert_eq!(
        graph
            .iter_edges_for_label(VertexId::from(0), label)
            .unwrap()
            .len(),
        7
    );
}

#[test]
fn a_deleted_spill_row_is_tombstoned_in_place() {
    // ADR 0097 §7: the run's length does not shrink and no ordinal moves.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..8);
    let before = bucket_of(&graph, VertexId::from(0), label);
    let run = graph.edges().spill_mint(before.edge_spill_run()).unwrap();
    let rows_before = graph.edges().spill_used_len(run);

    // Slot 5 is a run row (4 tiny slots + run row 0 at slot 4).
    let removed = graph
        .remove_edge_at_slot(VertexId::from(0), label, 5)
        .unwrap()
        .expect("spill row removed");
    assert_eq!(removed.neighbor_vid(), VertexId::from(5));

    let after = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(after.degree(), 7);
    assert_eq!(after.edge_spill_run(), before.edge_spill_run());
    assert_eq!(graph.edges().spill_used_len(run), rows_before);
    // The tombstone is at the same ordinal, and the survivors kept theirs.
    let tombstone = graph.edges().spill_edge_at(run, 0).unwrap();
    assert!(tombstone.is_deleted_slot());
    let mut survivors: Vec<u32> = Vec::new();
    graph
        .visit_edges(
            VertexId::from(0),
            label,
            OutEdgeOrder::Ascending,
            |slot, _edge| {
                survivors.push(slot.raw());
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
    assert_eq!(survivors, vec![0, 1, 2, 3, 4, 6, 7]);
}

#[test]
fn deleting_every_row_keeps_the_run_until_compaction() {
    // A delete never frees a run (ADR 0097 §7): the descriptor still owns it, with tombstones.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..6);
    for slot in 0..6 {
        graph
            .remove_edge_at_slot(VertexId::from(0), label, slot)
            .unwrap();
    }
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(bucket.degree(), 0);
    assert_ne!(bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(run_rows(&graph, &bucket), 1);
}

#[test]
fn promoting_a_spilling_bucket_keeps_every_row() {
    // F4c: the promotion compacts first, so a run-owning bucket cannot lose rows to transcription.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    let rows = T_PROMOTE + 8;
    insert_many(&graph, VertexId::from(0), label, 0..rows);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(bucket.degree(), rows);

    super::promote::promote_bucket_to_tree_mode(&graph, VertexId::from(0), label).unwrap();
    let promoted = bucket_of(&graph, VertexId::from(0), label);
    assert!(promoted.is_tree_mode());
    assert_eq!(promoted.degree(), rows, "every row survives promotion");
    assert_eq!(
        promoted.stored_slots(),
        promoted.degree(),
        "a tree bucket is dense: its width covers every live row"
    );
    assert_eq!(promoted.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(
        graph
            .iter_edges_for_label(VertexId::from(0), label)
            .unwrap()
            .len(),
        rows as usize
    );
}

#[test]
fn the_batch_defers_a_run_backed_append_without_writing() {
    // F3c: the batch rejects the run before reserving anything, and the caller's scalar fallback
    // appends, grows and promotes.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..6);
    let before = bucket_of(&graph, VertexId::from(0), label);
    let census_before = graph.edges().header().num_edges;

    let plan = super::batch_write::OneOrientationBatchPlan {
        runs: vec![super::batch_write::OneOrientationBucketRun {
            owner_vertex_id: VertexId::from(0),
            label_id: label,
            inline_property_width: 0,
            placement: EdgePlacementPolicy::Insertion,
            edges: (100..104)
                .map(|target| super::batch_write::OneOrientationBatchEdge {
                    logical_ordinal: target,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(target),
                    label_id: label,
                    edge: TestEdge { target },
                })
                .collect(),
        }],
    };
    let error = graph
        .insert_one_orientation_batch(&plan)
        .expect_err("a run-backed batch run is deferred");
    assert!(
        error.is_recoverable_geometry(),
        "the deferral must be recoverable: {error:?}"
    );
    let after = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(after.degree(), before.degree());
    assert_eq!(graph.edges().header().num_edges, census_before);
}

#[test]
fn bypass_conversion_carries_the_run_and_the_total_width() {
    // ADR 0097 §3: a bypass row carries its own run id, and its `stored_degree` is the total width.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..5);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    let rows = run_rows(&graph, &bucket);

    graph
        .enable_default_edge_bypass(VertexId::from(0))
        .expect("bypass conversion");
    let vertex = graph.vertices().get(VertexId::from(0));
    assert!(vertex.is_default_edge_labeled());
    assert_eq!(vertex.bypass_spill_run(), bucket.edge_spill_run());
    assert_eq!(
        vertex.stored_degree(),
        bucket.stored_slots() + rows,
        "the bypass row's width is the total (prefix + run)"
    );
    assert_eq!(
        graph
            .out_edges(VertexId::from(0))
            .unwrap()
            .iter()
            .map(|edge| edge.target)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4]
    );
}

#[test]
fn scans_yield_a_multi_row_run_in_materialization_order_both_ways() {
    // ADR 0097 §7: scan is prefix then run, in ordinal order; descending is the reverse.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..9);
    let ascending: Vec<u32> = graph
        .out_edges(VertexId::from(0))
        .unwrap()
        .iter()
        .map(|edge| edge.target)
        .collect();
    assert_eq!(ascending, (0..9).collect::<Vec<_>>());

    let mut descending = Vec::new();
    graph
        .visit_edges(
            VertexId::from(0),
            label,
            OutEdgeOrder::Descending,
            |_slot, edge| {
                descending.push(edge.target);
                ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
    assert_eq!(descending, (0..9).rev().collect::<Vec<_>>());
}

#[test]
fn growing_the_run_preserves_the_rows_and_the_ordinals() {
    // ADR 0097 §6: a full class grows (in place at the arena tail, else into the next class) and
    // the rows keep their ordinals.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    // Tiny holds 4; class 8 holds 8 more, so 13 rows force the first class growth.
    insert_many(&graph, VertexId::from(0), label, 0..13);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(run_rows(&graph, &bucket), 8);
    assert_eq!(bucket.degree(), 13);
    let ascending: Vec<u32> = graph
        .out_edges(VertexId::from(0))
        .unwrap()
        .iter()
        .map(|edge| edge.target)
        .collect();
    assert_eq!(ascending, (0..13).collect::<Vec<_>>());
}

#[test]
fn a_slab_delete_leaves_the_run_alone() {
    // The two planes of one bucket are independent: a prefix tombstone never touches the run.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..6);
    let before = bucket_of(&graph, VertexId::from(0), label);
    let run = graph.edges().spill_mint(before.edge_spill_run()).unwrap();
    let rows_before = graph.edges().spill_used_len(run);

    graph
        .remove_edge_at_slot(VertexId::from(0), label, 1)
        .unwrap()
        .expect("tiny row removed");
    let after = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(graph.edges().spill_used_len(run), rows_before);
    assert_eq!(after.degree(), 5);
}

#[test]
fn a_reinsert_after_a_spill_delete_appends_at_the_dense_end() {
    // ADR 0097 §7 (Insertion): the run appends; the tombstone stays where it was.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..7);
    graph
        .remove_edge_at_slot(VertexId::from(0), label, 5)
        .unwrap();
    insert_many(&graph, VertexId::from(0), label, [99]);

    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(bucket.degree(), 7);
    assert_eq!(run_rows(&graph, &bucket), 3, "the tombstone stays resident");
    let ascending: Vec<u32> = graph
        .out_edges(VertexId::from(0))
        .unwrap()
        .iter()
        .map(|edge| edge.target)
        .collect();
    assert_eq!(ascending, vec![0, 1, 2, 3, 4, 6, 99]);
}

#[test]
fn a_tiny_bucket_never_allocates_a_run() {
    // ADR 0097 §4: only rows beyond the prefix allocate, and tiny rows are the descriptor.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..4);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert!(bucket.is_tiny_mode());
    assert_eq!(bucket.edge_spill_run(), NO_SPILL_RUN);
}

#[test]
fn the_sentinel_round_trips_through_a_bucket_slot_write() {
    // F4b: the wire's all-ones pattern is the sentinel's spelling and must decode back to it.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, [10]);
    let bucket = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(bucket.values_spill_run(), NO_SPILL_RUN);

    // A live run id round-trips too.
    insert_many(&graph, VertexId::from(0), label, 0..6);
    let spilling = bucket_of(&graph, VertexId::from(0), label);
    let live = spilling.edge_spill_run();
    assert_ne!(live, NO_SPILL_RUN);
    let mut bytes = [0u8; LabelBucket::BYTES];
    spilling.write_to(&mut bytes);
    let decoded = LabelBucket::read_from(&bytes);
    assert_eq!(decoded.edge_spill_run(), live);
    assert_eq!(decoded.degree(), spilling.degree());
}

#[test]
fn a_spill_delete_never_shifts_the_runs_other_rows() {
    // ADR 0097 "No row ever shifts": the delete tombstones ordinal 0 and every later row keeps
    // both its ordinal and its content.
    let graph = spilling_graph();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    insert_many(&graph, VertexId::from(0), label, 0..9);
    let before = bucket_of(&graph, VertexId::from(0), label);
    let run = graph.edges().spill_mint(before.edge_spill_run()).unwrap();
    let ordinal_one_before = graph.edges().spill_edge_at(run, 1).unwrap().target;

    graph
        .remove_edge_at_slot(VertexId::from(0), label, 5)
        .unwrap()
        .expect("the run's row 0 is removed");

    let after = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(after.edge_spill_run(), before.edge_spill_run());
    assert_eq!(
        graph
            .edges()
            .spill_edge_at(run, 0)
            .unwrap()
            .is_deleted_slot(),
        true
    );
    assert_eq!(
        graph.edges().spill_edge_at(run, 1).unwrap().target,
        ordinal_one_before,
        "the next row keeps its ordinal and its content"
    );
    let ascending: Vec<u32> = graph
        .out_edges(VertexId::from(0))
        .unwrap()
        .iter()
        .map(|edge| edge.target)
        .collect();
    assert_eq!(ascending, vec![0, 1, 2, 3, 4, 6, 7, 8]);
}

/// A relocation keeps the run, so the rewrite that follows a spill insert must not ask the
/// span for room to hold the run's rows.
///
/// GAP-2026-09-20-007: the plan counted them (`bucket_resident_rows` = prefix + run), so
/// `min_required` (65) exceeded the span it already had (64), the plan asked for a wider one
/// (144) and the dense leaf could not place it, trapping the insert with
/// `CollectAllocationOverflow`. The shape is the one `bench_l_nt_bp_ins_1024` uses: a promoted
/// bypass bucket with 64 rows, 256 successors pushed after it so the row is not the tail, then
/// same-label inserts that land in the run while the leaf cascade runs its rewrite.
#[test]
fn a_dense_leaf_rewrite_keeps_the_run_and_its_ordinals() {
    let graph = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::new(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(8192),
        BucketLabelKey::directed_from_index(1),
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();
    let label = graph.default_label();
    for target in 0..64u32 {
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge::read_from(&target.to_le_bytes()),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    for _ in 0..256 {
        graph.push_vertex(LabeledVertex::default()).unwrap();
    }
    for target in 1_000_000..1_000_016u32 {
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge::read_from(&target.to_le_bytes()),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap_or_else(|error| panic!("insert {target}: {error:?}"));
    }

    let after = bucket_of(&graph, VertexId::from(0), label);
    assert_eq!(after.degree(), 80, "every insert landed");
    assert_ne!(
        after.edge_spill_run(),
        NO_SPILL_RUN,
        "the rows beyond the prefix live in the run the rewrite kept"
    );
    assert_eq!(
        run_rows(&graph, &after),
        16,
        "the run keeps its rows and its ordinals"
    );
    assert_eq!(
        graph
            .iter_edges_for_label(VertexId::from(0), label)
            .unwrap()
            .len(),
        80,
        "the run is still reachable through the published descriptor"
    );
}
