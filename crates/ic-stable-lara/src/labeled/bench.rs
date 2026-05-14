//! Benchmarks for the labeled CSR core.

use crate::bench as helper;
use crate::labeled::{graph::LabeledLaraGraph, record::LabelId};
use crate::{VertexId, test_support::vector_memory, traits::CsrEdge};
use canbench_rs::{bench, bench_fn, bench_scope};
use std::hint::black_box;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchEdge(u32);

impl CsrEdge for BenchEdge {
    const BYTES: usize = 10;

    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        bytes[4..10].fill(0);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}

#[bench(raw)]
fn bench_labeled_iter_edges_for_label_128() -> canbench_rs::BenchResult {
    let graph = LabeledLaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        4096,
        LabelId::from_raw(1),
    )
    .expect("graph");
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = LabelId::from_raw(2);
    for i in 0..128u32 {
        graph
            .insert_edge(VertexId::from(0), label, BenchEdge(i))
            .expect("insert");
    }
    bench_fn(|| {
        let _scope = bench_scope("labeled_iter_edges_for_label");
        let mut count = 0usize;
        for edge in graph
            .iter_edges_for_label(VertexId::from(0), label)
            .expect("iter")
        {
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_labeled_default_bypass_iter_128() -> canbench_rs::BenchResult {
    let graph = LabeledLaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        4096,
        LabelId::from_raw(1),
    )
    .expect("graph");
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    graph
        .enable_default_edge_bypass(VertexId::from(0))
        .expect("bypass");
    for i in 0..128u32 {
        graph
            .insert_edge(VertexId::from(0), graph.default_label(), BenchEdge(i))
            .expect("insert");
    }
    bench_fn(|| {
        let _scope = bench_scope("labeled_default_bypass_iter");
        let mut count = 0usize;
        for edge in graph.iter_out_edges(VertexId::from(0)).expect("iter") {
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_labeled_insert_existing_bucket_128() -> canbench_rs::BenchResult {
    let graph = LabeledLaraGraph::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        4096,
        LabelId::from_raw(1),
    )
    .expect("graph");
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = LabelId::from_raw(2);
    bench_fn(|| {
        let _scope = bench_scope("labeled_insert_existing_bucket");
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(VertexId::from(0), label, BenchEdge(i))
                .expect("insert");
        }
    })
}

#[bench(raw)]
fn bench_compact_edge_decode_scan_128() -> canbench_rs::BenchResult {
    let mut bytes = Vec::with_capacity(128 * BenchEdge::BYTES);
    for i in 0..128u32 {
        let mut slot = [0u8; BenchEdge::BYTES];
        BenchEdge(i).write_to(&mut slot);
        bytes.extend_from_slice(&slot);
    }
    bench_fn(|| {
        let _scope = bench_scope("compact_edge_decode_scan");
        let mut count = 0usize;
        for chunk in bytes.chunks_exact(BenchEdge::BYTES) {
            let edge = BenchEdge::read_from(chunk);
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}
