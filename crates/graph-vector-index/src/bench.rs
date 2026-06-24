//! Phase 8 exact-scan vector-search benchmarks (ADR 0031 Slice 5).
//!
//! Establishes the baseline cost of the degenerate `ivf_flat` exact scan — live subject-map scan +
//! L2-squared scoring + bounded top-k — ahead of Slice 6 IVF partitioning. `l2_squared_f32` is kept
//! isolated in the store so a future SIMD variant can be benchmarked against this baseline.
//!
//! Run from `crates/graph-vector-index`: `canbench` (see `canbench.yml`).

use crate::facade::VectorIndexStore;
use crate::init::VectorIndexInitArgs;
use canbench_rs::bench;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSearchRequest, VectorSubject,
};
use std::hint::black_box;

const INDEX_ID: u32 = 1;
const SCAN_N: u32 = 4096;

fn router() -> Principal {
    Principal::from_slice(&[9])
}

fn shard_owner() -> Principal {
    Principal::from_slice(&[1])
}

fn vec_bytes(dims: u16, value: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(dims as usize * 4);
    for _ in 0..dims {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Fresh store with `n` width-`dims` vectors on shard 0; vector `i` is filled with the value `i` so
/// the scored set is fully distinct.
fn setup_search_store(dims: u16, n: u32) -> VectorIndexStore {
    let store = VectorIndexStore::new();
    store
        .init_from_args(&VectorIndexInitArgs {
            router_canister: router(),
        })
        .expect("init");
    store
        .admin_attach_shard_canister(
            router(),
            GraphId::from_raw(1),
            ShardId::new(0),
            shard_owner(),
        )
        .expect("attach shard");
    for vid in 0..n {
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: vid,
            },
            embedding_incarnation: 1,
            embedding_version: 1,
            encoding: VectorEncoding::F32,
            dims,
            bytes: vec_bytes(dims, vid as f32),
            remove: false,
        };
        store
            .vector_upsert(shard_owner(), &op)
            .expect("seed vector");
    }
    store
}

fn search_req(dims: u16, top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(dims, 0.0),
        encoding: VectorEncoding::F32,
        dims,
        metric: VectorMetric::L2Squared,
        top_k,
    }
}

macro_rules! search_bench {
    ($name:ident, $dims:expr, $top_k:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_search_store($dims, SCAN_N);
            let req = search_req($dims, $top_k);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store.vector_search(black_box(&req)).expect("vector_search");
                black_box(result);
            })
        }
    };
}

search_bench!(bench_vector_search_d128_k10, 128, 10);
search_bench!(bench_vector_search_d128_k100, 128, 100);
search_bench!(bench_vector_search_d384_k10, 384, 10);
search_bench!(bench_vector_search_d384_k100, 384, 100);
search_bench!(bench_vector_search_d768_k10, 768, 10);
search_bench!(bench_vector_search_d768_k100, 768, 100);
