//! Source-valid hot/cold spill screen. Layout/capacity alternatives use fresh producers.
//! Backing I/O and phase scopes exclude observations, logical oracles and reopen.

use super::leaf_cover::EdgeSlot;
use super::{BucketSearch, EdgePlacementPolicy, LabeledLaraGraph, OutEdgeOrder};
use crate::{
    VertexId,
    labeled::{BucketLabelKey, InitialCapacities, LabeledVertex, spill_run_store::SpillRunStore},
    test_support::{CountMemory, TestEdge},
    traits::{CsrVertex, NO_SPILL_RUN},
    traverse::BucketEntryPosition,
};
use ic_stable_structures::Memory;
use std::{hint::black_box, ops::ControlFlow};

const HOT: u32 = 3;
const SETUP: u32 = 32;
const UPDATED: u32 = 288;
type Graph = LabeledLaraGraph<TestEdge, CountMemory>;

fn label() -> BucketLabelKey {
    BucketLabelKey::directed_from_index(11)
}

fn target(slot: u32) -> u32 {
    if slot < SETUP {
        32 + slot
    } else {
        32 + slot % 64
    }
}

fn emit(text: &str) {
    #[cfg(target_family = "wasm")]
    ic_cdk::println!("{text}");
    #[cfg(not(target_family = "wasm"))]
    println!("{text}");
}

fn io(memories: &[CountMemory; 16]) -> [u64; 6] {
    memories.iter().fold([0; 6], |mut sum, memory| {
        for (total, count) in sum.iter_mut().zip([
            memory.reads.get() as u64,
            memory.bytes.get() as u64,
            memory.write_calls.get() as u64,
            memory.writes.get() as u64,
            memory.grow_calls.get() as u64,
            memory.grown_pages.get(),
        ]) {
            *total += count;
        }
        sum
    })
}

fn phase<T>(name: &'static str, memories: &[CountMemory; 16], work: impl FnOnce() -> T) -> T {
    let before = io(memories);
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let scope = canbench_rs::bench_scope(name);
    let result = work();
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    drop(scope);
    let after = io(memories);
    let delta: [u64; 6] = std::array::from_fn(|i| after[i] - before[i]);
    let pages: [u64; 16] = std::array::from_fn(|i| memories[i].size());
    emit(&format!(
        "HEADROOM_IO {{\"phase\":\"{name}\",\"read_calls\":{},\"read_bytes\":{},\"write_calls\":{},\"write_bytes\":{},\"grow_calls\":{},\"grown_pages\":{},\"backing_pages\":{pages:?}}}",
        delta[0], delta[1], delta[2], delta[3], delta[4], delta[5],
    ));
    result
}

fn scan(graph: &Graph, owner: u32, order: OutEdgeOrder) -> Vec<(u32, u32)> {
    let mut rows = Vec::new();
    let flow = graph
        .visit_edges(VertexId::from(owner), label(), order, |slot, edge| {
            rows.push((slot.raw(), edge.0));
            ControlFlow::<()>::Continue(())
        })
        .expect("screen scan");
    assert_eq!(flow, ControlFlow::Continue(()));
    rows
}

fn measured_scan(graph: &Graph, order: OutEdgeOrder) -> (u32, u64, ControlFlow<()>) {
    let mut count = 0;
    let mut checksum = 0u64;
    let flow = graph
        .visit_edges(VertexId::from(HOT), label(), order, |slot, edge| {
            count += 1;
            checksum = checksum
                .wrapping_add(u64::from(slot.raw()) * 31)
                .wrapping_add(u64::from(edge.0));
            black_box((slot.raw(), edge.0));
            ControlFlow::<()>::Continue(())
        })
        .expect("measured screen scan");
    black_box((count, checksum, flow))
}

fn assert_state(graph: &Graph, extent: u32, cold: &[Option<crate::labeled::LabelBucket>]) {
    assert_eq!(graph.vertex_count(), 256u32.into());
    assert_eq!(graph.edges.header().segment_size, 16);
    assert_eq!(graph.buckets.header().segment_size, 16);
    assert_eq!(graph.edges.header().num_edges, u64::from(extent + 27));
    for owner in 0..256 {
        let vertex = graph.vertices.get(VertexId::from(owner));
        assert!(!vertex.is_default_edge_labeled());
        assert_eq!(vertex.inline_property_bytes_allocated_bytes(), 0);
        let expected: Vec<_> = if owner == HOT {
            (0..extent)
                .map(|slot| (slot, if slot == UPDATED { 0 } else { target(slot) }))
                .collect()
        } else if owner < 16 {
            (0..owner % 5).map(|slot| (slot, 64 + slot)).collect()
        } else {
            Vec::new()
        };
        assert_eq!(scan(graph, owner, OutEdgeOrder::Ascending), expected);
        let descending: Vec<_> = expected.iter().copied().rev().collect();
        assert_eq!(scan(graph, owner, OutEdgeOrder::Descending), descending);
        for &(slot, neighbor) in &expected {
            assert_eq!(
                graph
                    .read_edge(
                        VertexId::from(owner),
                        label(),
                        BucketEntryPosition::new(slot)
                    )
                    .expect("point read"),
                Some(TestEdge(neighbor)),
            );
        }
        assert_eq!(
            graph
                .read_edge(
                    VertexId::from(owner),
                    label(),
                    BucketEntryPosition::new(expected.len() as u32),
                )
                .expect("point just past extent"),
            None,
        );
        match graph
            .find_bucket(VertexId::from(owner), &vertex, label())
            .expect("bucket")
        {
            BucketSearch::Found { bucket, .. } => {
                assert_eq!(bucket.bucket_label_key(), label());
                assert_eq!(bucket.values_spill_run(), NO_SPILL_RUN);
                if owner == HOT {
                    assert!(!bucket.is_tiny_mode() && !bucket.is_tree_mode());
                    assert_eq!(bucket.inline_property_byte_width(), 0);
                    assert_eq!(bucket.inline_property_bytes_slab_slots(), 0);
                    let spill = if bucket.edge_spill_run() == NO_SPILL_RUN {
                        0
                    } else {
                        graph.edges.spill_used_len(
                            graph.edges.spill_mint(bucket.edge_spill_run()).unwrap(),
                        )
                    };
                    assert_eq!(bucket.stored_slots() + spill, extent);
                    assert!(bucket.stored_slots() <= vertex.stored_slots);
                } else {
                    assert!(bucket.is_tiny_mode());
                    assert_eq!(vertex.stored_slots, 0);
                    assert_eq!(bucket.edge_spill_run(), NO_SPILL_RUN);
                    // Descriptor relocation may reanchor a Tiny row, never change its payload.
                    assert_eq!(
                        Some(
                            bucket
                                .with_edge_range(EdgeSlot::from_raw(0), bucket.stored_slots_raw())
                        ),
                        cold[owner as usize]
                    );
                }
            }
            BucketSearch::Missing { .. } => {
                assert!(expected.is_empty());
                assert_eq!(vertex.degree(), 0);
                assert_eq!(vertex.stored_slots, 0);
                if owner < 16 {
                    assert_eq!(cold[owner as usize], None);
                }
            }
        }
    }
    crate::labeled::invariants::assert_labeled_layout_invariants(
        &graph.vertices,
        &graph.buckets,
        &graph.edges,
    );
    let header = graph.edges.header();
    let mut assigned = Vec::new();
    for leaf in 0..header.segment_count {
        if let Some(span) =
            crate::lara::edge::EdgeStore::<TestEdge, CountMemory>::assigned_range_from_source(
                graph.edges.span_meta_store().get(u64::from(leaf)),
                graph
                    .edges
                    .counts_store()
                    .get(u64::from(header.segment_count + leaf)),
                header.elem_capacity,
            )
            .expect("assigned range")
        {
            assigned.push(span);
        }
    }
    let free = graph.edges.free_span_store().spans();
    for (index, span) in assigned.iter().enumerate() {
        for other in assigned.iter().skip(index + 1).chain(&free) {
            assert!(
                span.start_slot + span.len <= other.start_slot
                    || other.start_slot + other.len <= span.start_slot
            );
        }
    }
}

fn hot_bucket(graph: &Graph) -> crate::labeled::LabelBucket {
    let vertex = graph.vertices.get(VertexId::from(HOT));
    let BucketSearch::Found { bucket, .. } = graph
        .find_bucket(VertexId::from(HOT), &vertex, label())
        .unwrap()
    else {
        panic!("hot missing")
    };
    bucket
}

fn report_state(graph: &Graph, name: &str) {
    let vertex = graph.vertices.get(VertexId::from(HOT));
    let bucket = hot_bucket(graph);
    let used = if bucket.edge_spill_run() == NO_SPILL_RUN {
        0
    } else {
        graph
            .edges
            .spill_used_len(graph.edges.spill_mint(bucket.edge_spill_run()).unwrap())
    };
    let class = if used == 0 {
        0
    } else {
        SpillRunStore::<CountMemory>::class_rows(used)
    };
    let (arena_rows, free_classes) = graph.edges.spill_allocator_inventory();
    emit(&format!(
        "HEADROOM_STATE {{\"phase\":\"{name}\",\"origin\":{},\"vertex_capacity\":{},\"prefix\":{},\"csr_tail_slack\":{},\"spill_id\":{},\"spill_used\":{used},\"spill_class\":{class},\"spill_arena_rows\":{arena_rows},\"free_classes\":{free_classes:?},\"csr_allocator\":\"{:?}\"}}",
        bucket.edge_start(),
        vertex.stored_slots,
        bucket.stored_slots(),
        vertex.stored_slots - bucket.stored_slots(),
        bucket.edge_spill_run(),
        graph.edges.allocator_stats(),
    ));
}

fn run_w2() {
    // Sixteen fresh, independent backing regions. Clones below allocate no backing region.
    let memories: [CountMemory; 16] = std::array::from_fn(|_| CountMemory::default());
    let graph = phase("construction", &memories, || {
        let [v, b, bf, bs, c, e, s, p, ef, es, val, vf, vs, vl, vb, ltb] = memories.clone();
        Graph::new(
            crate::labeled::graph::GraphMemories {
                vertices: v,
                buckets: b,
                bucket_free_spans: bf,
                bucket_free_span_by_start: bs,
                edge_counts: c,
                edges: e,
                edge_log: s,
                edge_span_meta: p,
                edge_free_spans: ef,
                edge_free_span_by_start: es,
                inline_property_bytes_slab: val,
                value_free_spans: vf,
                value_free_span_by_start: vs,
                inline_property_bytes_log: vl,
                value_blobs: vb,
                ltb,
            },
            InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(7),
        )
        .unwrap()
    });
    phase("setup", &memories, || {
        for _ in 0..256 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        for slot in 0..SETUP {
            graph
                .insert_edge(
                    VertexId::from(HOT),
                    label(),
                    TestEdge(target(slot)),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        for owner in (0..16).filter(|&owner| owner != HOT) {
            for slot in 0..owner % 5 {
                graph
                    .insert_edge(
                        VertexId::from(owner),
                        label(),
                        TestEdge(64 + slot),
                        EdgePlacementPolicy::Insertion,
                    )
                    .unwrap();
            }
        }
    });
    let cold: Vec<_> = (0..16)
        .map(|owner| {
            let vertex = graph.vertices.get(VertexId::from(owner));
            match graph
                .find_bucket(VertexId::from(owner), &vertex, label())
                .unwrap()
            {
                BucketSearch::Found { bucket, .. } if owner != HOT => {
                    Some(bucket.with_edge_range(EdgeSlot::from_raw(0), bucket.stored_slots_raw()))
                }
                BucketSearch::Found { .. } | BucketSearch::Missing { .. } => None,
            }
        })
        .collect();
    assert_state(&graph, SETUP, &cold);
    report_state(&graph, "setup");
    let setup_bucket = hot_bucket(&graph);
    assert_ne!(setup_bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(setup_bucket.edge_spill_run(), 0);
    assert_eq!(
        scan(&graph, 14, OutEdgeOrder::Ascending),
        (0..4).map(|slot| (slot, 64 + slot)).collect::<Vec<_>>()
    );
    phase("updates", &memories, || {
        for slot in SETUP..UPDATED {
            graph
                .insert_edge(
                    VertexId::from(HOT),
                    label(),
                    TestEdge(target(slot)),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
    });
    assert_state(&graph, UPDATED, &cold);
    report_state(&graph, "updates");
    let updated_bucket = hot_bucket(&graph);
    assert_eq!(
        updated_bucket.edge_spill_run(),
        setup_bucket.edge_spill_run()
    );
    assert_eq!(updated_bucket.stored_slots(), setup_bucket.stored_slots());
    assert_eq!(graph.edges.spill_allocator_inventory(), (513, [0; 8]));
    let expected_checksum = (0..UPDATED)
        .map(|slot| u64::from(slot) * 31 + u64::from(target(slot)))
        .sum();
    assert_eq!(
        phase("ascending", &memories, || measured_scan(
            &graph,
            OutEdgeOrder::Ascending
        )),
        (UPDATED, expected_checksum, ControlFlow::Continue(()))
    );
    assert_eq!(
        phase("descending", &memories, || measured_scan(
            &graph,
            OutEdgeOrder::Descending
        )),
        (UPDATED, expected_checksum, ControlFlow::Continue(()))
    );
    // Reopen while the run is still retained; reopening only after fold would miss its codec.
    drop(graph);
    let [v, b, bf, bs, c, e, s, p, ef, es, val, vf, vs, vl, vb, ltb] = memories.clone();
    let graph = Graph::init(
        crate::labeled::graph::GraphMemories {
            vertices: v,
            buckets: b,
            bucket_free_spans: bf,
            bucket_free_span_by_start: bs,
            edge_counts: c,
            edges: e,
            edge_log: s,
            edge_span_meta: p,
            edge_free_spans: ef,
            edge_free_span_by_start: es,
            inline_property_bytes_slab: val,
            value_free_spans: vf,
            value_free_span_by_start: vs,
            inline_property_bytes_log: vl,
            value_blobs: vb,
            ltb,
        },
        InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(7),
    )
    .unwrap();
    assert_state(&graph, UPDATED, &cold);
    assert_eq!(hot_bucket(&graph), updated_bucket);
    assert_eq!(graph.edges.spill_allocator_inventory(), (513, [0; 8]));
    report_state(&graph, "reopened_retained_run");
    phase("explicit_fold", &memories, || {
        let bucket = hot_bucket(&graph);
        super::promote::compact_run_into_prefix_with_growth_retry(
            &graph,
            VertexId::from(HOT),
            0,
            bucket,
        )
        .unwrap();
    });
    assert_state(&graph, UPDATED, &cold);
    report_state(&graph, "explicit_fold");
    let bucket = hot_bucket(&graph);
    assert_eq!(bucket.stored_slots(), UPDATED);
    assert_eq!(bucket.edge_spill_run(), NO_SPILL_RUN);
    assert_eq!(
        graph.edges.spill_allocator_inventory(),
        (513, [0, 0, 0, 0, 0, 0, 1, 0])
    );
    assert_eq!(
        phase("folded_ascending", &memories, || measured_scan(
            &graph,
            OutEdgeOrder::Ascending
        )),
        (UPDATED, expected_checksum, ControlFlow::Continue(()))
    );
    assert_eq!(
        phase("folded_descending", &memories, || measured_scan(
            &graph,
            OutEdgeOrder::Descending
        )),
        (UPDATED, expected_checksum, ControlFlow::Continue(()))
    );
    // An unmeasured distinct write proves writable reopen, not merely decode success.
    graph
        .insert_edge(
            VertexId::from(HOT),
            label(),
            TestEdge(0),
            EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    assert_state(&graph, UPDATED + 1, &cold);
}

#[cfg(feature = "canbench")]
#[canbench_rs::bench(raw)]
fn vertex_span_hot_with_cold_shared() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(run_w2)
}

#[cfg(test)]
mod tests {
    #[test]
    fn shared_hot_with_cold_preserves_slots_spill_and_reopen() {
        super::run_w2();
    }
}
