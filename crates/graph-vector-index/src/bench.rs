//! Vector-search benchmarks (ADR 0031 Slice 5 exact scan + Slice 6 partition-page scan).
//!
//! The exact-scan benches establish the baseline cost of the degenerate `ivf_flat` exact scan —
//! live subject-map scan + L2-squared scoring + bounded top-k. The Slice 6 benches measure the
//! partition-page scan over **clustered** seeded datasets: `nprobe = nlist` is the parity point
//! (scans every partition, same result set as exact), and lower `nprobe` skips populated partitions
//! so the cost reduction is visible. The partition scan is *not* expected to match exact-scan
//! instruction cost even at `nprobe = nlist` — it adds centroid scoring plus reverse-map and
//! subject-map lookups. `l2_squared_f32` is kept isolated in the store so a future SIMD variant can
//! be benchmarked against these baselines.
//!
//! Run from `crates/graph-vector-index`: `canbench` (see `canbench.yml`).

use crate::facade::{SearchTuning, VectorIndexStore};
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

/// Distance between adjacent cluster centroids — far larger than the in-cluster jitter so each
/// seeded vector's nearest centroid is unambiguously its own cluster.
const CLUSTER_SPACING: f32 = 1000.0;

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

/// A constant-valued width-`dims` `f32` vector.
fn cvec(dims: u16, value: f32) -> Vec<f32> {
    vec![value; dims as usize]
}

/// Seeds a trained, clustered partitioned `ivf_flat` index: `nlist` centroids spaced by
/// `CLUSTER_SPACING`, with `n` vectors round-robin assigned to clusters and a tiny in-cluster jitter
/// so every vector is distinct yet nearest to its own centroid. Lower `nprobe` therefore skips whole
/// populated clusters.
fn setup_partitioned_store(dims: u16, n: u32, nlist: u32) -> VectorIndexStore {
    let store = VectorIndexStore::new();
    store
        .init_from_args(&VectorIndexInitArgs {
            router_canister: router(),
        })
        .expect("init");
    let centroids: Vec<Vec<f32>> = (0..nlist)
        .map(|c| cvec(dims, c as f32 * CLUSTER_SPACING))
        .collect();
    let vectors: Vec<(VectorSubject, Vec<f32>)> = (0..n)
        .map(|i| {
            let cluster = i % nlist;
            let jitter = (i / nlist) as f32 * 0.001;
            let value = cluster as f32 * CLUSTER_SPACING + jitter;
            (
                VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: i,
                },
                cvec(dims, value),
            )
        })
        .collect();
    store.seed_ivf_for_test(INDEX_ID, VectorEncoding::F32, dims, &centroids, &vectors);
    store
}

macro_rules! partitioned_bench {
    ($name:ident, $dims:expr, $nlist:expr, $nprobe:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_partitioned_store($dims, SCAN_N, $nlist);
            let req = search_req($dims, 10);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store
                    .vector_search_tuned(black_box(&req), SearchTuning { nprobe: $nprobe })
                    .expect("vector_search_tuned");
                black_box(result);
            })
        }
    };
}

// nprobe sweep at fixed (dims, nlist) — demonstrates that lower nprobe reduces cost, and that
// nprobe = nlist is the exact-parity upper bound.
partitioned_bench!(bench_ivf_d128_nlist16_nprobe1, 128, 16, 1);
partitioned_bench!(bench_ivf_d128_nlist16_nprobe4, 128, 16, 4);
partitioned_bench!(bench_ivf_d128_nlist16_nprobe8, 128, 16, 8);
partitioned_bench!(bench_ivf_d128_nlist16_nprobe16, 128, 16, 16);
partitioned_bench!(bench_ivf_d128_nlist64_nprobe1, 128, 64, 1);
partitioned_bench!(bench_ivf_d128_nlist64_nprobe4, 128, 64, 4);
partitioned_bench!(bench_ivf_d128_nlist64_nprobe8, 128, 64, 8);

// Dimensional coverage at representative nprobe values.
partitioned_bench!(bench_ivf_d384_nlist16_nprobe1, 384, 16, 1);
partitioned_bench!(bench_ivf_d384_nlist16_nprobe4, 384, 16, 4);
partitioned_bench!(bench_ivf_d384_nlist64_nprobe8, 384, 64, 8);
partitioned_bench!(bench_ivf_d768_nlist16_nprobe1, 768, 16, 1);
partitioned_bench!(bench_ivf_d768_nlist16_nprobe4, 768, 16, 4);
partitioned_bench!(bench_ivf_d768_nlist64_nprobe8, 768, 64, 8);
