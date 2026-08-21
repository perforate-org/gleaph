//! Vector-search benchmarks (ADR 0031 Slice 5 exact scan + Slice 6 ε₂/early-exit partition-page scan).
//!
//! The exact-scan benches establish the baseline cost of the degenerate `ivf_flat` exact scan —
//! live subject-map scan + L2-squared scoring (crate early-exit kernel) + bounded top-k. The Slice 6
//! benches measure the partition-page scan over **clustered** seeded datasets under ε₂ query-aware
//! pruning: `eps_query = 0.0` scans only the nearest partition, `f32::INFINITY` is the exact-parity
//! upper bound (scans every partition, same result set as exact), and intermediate values skip
//! populated partitions so the cost reduction is visible. The partition scan is *not* expected to
//! match exact-scan instruction cost even at `eps_query = INF` — it adds centroid scoring plus the
//! per-row subject-map freshness re-validation (the row subject is rebuilt from the slab row-local
//! locator, ADR 0032).
//!
//! The `bench_ivf_d1536_*` sweep covers the ADR 0064 §8 design target (`d = 1536`) across the nlist
//! values that stay trainable at that width; it isolated the ~144K ins/centroid and ~164K ins/row
//! cost model that confirmed `DEFAULT_EPS_QUERY = 0.0` (recall is covered by the boundary-recall unit
//! test).
//!
//! Run from `crates/vector-canister`: `canbench` (see `canbench.yml`).

use crate::facade::stable::subject_store;
use crate::facade::{SearchTuning, VectorCanisterStore};
use crate::init::{DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, VectorCanisterInitArgs};
use crate::records::SubjectKey;
use canbench_rs::bench;
use candid::{Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMaintenancePolicy, VectorMaintenanceStepRequest,
    VectorMetric, VectorRebuildPhase, VectorSearchRequest, VectorSubject, VectorSyncBatchOutcome,
    VectorSyncBatchUnavailable, VectorSyncTerminalError,
};
use std::cell::Cell;
use std::hint::black_box;

const INDEX_ID: u32 = 1;
const SCAN_N: u32 = 4096;

/// Number of cosine exact-scan rows seeded exactly aligned with the query (`[1,1,..,1]`, distance 0).
/// This makes the k-th best distance small so the Cauchy-Schwarz early exit is exercised on the
/// remaining varied rows; must be >= the largest `top_k` used by the cosine exact-scan benches (100).
const COSINE_ALIGNED_ROWS: u32 = 100;

/// Distance between adjacent cluster centroids — far larger than the in-cluster jitter so each
/// seeded vector's nearest centroid is unambiguously its own cluster.
const CLUSTER_SPACING: f32 = 1000.0;

/// Query offset for the ε₂ sweep, placed between the first two clusters (45% of the way toward
/// cluster 1). A query here makes `dist(q, c_best)` non-zero so raising `eps_query` selects
/// progressively more partitions (0.0 → nearest only, 0.5 → two, 2.5 → three, INF → all). The zero
/// vector previously used sits exactly on centroid 0, which pins `c_best = 0` and degenerates the
/// whole sweep to a single-partition scan for every `eps_query`.
const SWEEP_QUERY: f32 = CLUSTER_SPACING * 0.45;

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

/// A deterministic varied-direction vector for cosine exact-scan benches. Constant rows (all
/// components equal) are degenerate for cosine: every row is unit-normalized to the same all-ones
/// direction, so all rows sit at distance 0 and the Cauchy-Schwarz early exit never triggers. This
/// pattern hashes `(vid, j)` into `[-1, 1]` per component, so each row has a distinct direction and
/// the cosine similarity to a constant query varies widely (including anti-correlated rows),
/// exercising the early exit.
fn vec_bytes_varied(dims: u16, vid: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(dims as usize * 4);
    for j in 0..dims {
        let h = (vid.wrapping_mul(2654435761) ^ (j as u32).wrapping_mul(2246822519)) & 0xFFFF;
        let value = (h as f32 / 65535.0) * 2.0 - 1.0;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Fresh store with `n` width-`dims` vectors on shard 0; vector `i` is filled with the value `i` so
/// the scored set is fully distinct.
fn setup_search_store(dims: u16, n: u32) -> VectorCanisterStore {
    setup_search_store_metric(dims, n, VectorMetric::L2Squared)
}

/// Like [`setup_search_store`] but seeds the index with the given metric, so a metric-mismatched
/// request (e.g. a cosine search against an L2 index) is avoided and the metric's scoring path is
/// exercised directly.
fn setup_search_store_metric(dims: u16, n: u32, metric: VectorMetric) -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .reset_for_test_or_bench(&VectorCanisterInitArgs {
            router_canister: router(),
            definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
            subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
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
        // Cosine stores unit-normalized rows and rejects zero-norm, so seed a non-zero value
        // (vid 0 would be the zero vector). Constant rows are degenerate for cosine (all rows at
        // distance 0), so cosine uses varied-direction rows to exercise the early exit. The first
        // `COSINE_ALIGNED_ROWS` rows are seeded exactly aligned with the query so the k-th best
        // distance is small and the early exit triggers on the varied rows.
        let bytes = if metric == VectorMetric::Cosine {
            if vid < COSINE_ALIGNED_ROWS {
                vec_bytes(dims, 1.0)
            } else {
                vec_bytes_varied(dims, vid)
            }
        } else {
            vec_bytes(dims, vid as f32)
        };
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: vid,
            },
            mutation_id: 1,
            encoding: VectorEncoding::F32,
            dims,
            metric,
            bytes,
            remove: false,
        };
        store
            .vector_upsert(shard_owner(), &op)
            .expect("seed vector");
    }
    store
}

/// Like [`setup_search_store_metric`] but seeds an `I8` index (Model Y: the wire op bytes are still
/// canonical F32; the canister quantizes at write). Benchmarks the fused i8×f32 scoring path.
fn setup_i8_search_store_metric(dims: u16, n: u32, metric: VectorMetric) -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .reset_for_test_or_bench(&VectorCanisterInitArgs {
            router_canister: router(),
            definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
            subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
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
        // Cosine stores unit-normalized rows and rejects zero-norm, so seed a non-zero value. Constant
        // rows are degenerate for cosine (all rows at distance 0), so cosine uses varied-direction rows
        // to exercise the early exit; the first `COSINE_ALIGNED_ROWS` rows are aligned with the query
        // so the k-th best distance is small. Model Y: the wire op bytes are canonical F32; the
        // canister quantizes to I8 at write.
        let bytes = if metric == VectorMetric::Cosine {
            if vid < COSINE_ALIGNED_ROWS {
                vec_bytes(dims, 1.0)
            } else {
                vec_bytes_varied(dims, vid)
            }
        } else {
            vec_bytes(dims, vid as f32)
        };
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: vid,
            },
            mutation_id: 1,
            encoding: VectorEncoding::I8,
            dims,
            metric,
            bytes,
            remove: false,
        };
        store
            .vector_upsert(shard_owner(), &op)
            .expect("seed i8 vector");
    }
    store
}

fn search_req(dims: u16, top_k: u32) -> VectorSearchRequest {
    search_req_value(dims, top_k, 0.0)
}

/// Build a search request whose query vector is a constant `value` in every dimension. The exact-
/// scan and centroid-cache benches use `0.0`; the ε₂ sweep uses [`SWEEP_QUERY`] (off any centroid).
fn search_req_value(dims: u16, top_k: u32, value: f32) -> VectorSearchRequest {
    search_req_metric_value(dims, top_k, value, VectorMetric::L2Squared)
}

/// Like [`search_req_value`] but with an explicit metric. Cosine benches pass a non-zero query value
/// (a zero-norm query is rejected by the search boundary) and a `Cosine` metric matching the
/// cosine-seeded store.
fn search_req_metric_value(
    dims: u16,
    top_k: u32,
    value: f32,
    metric: VectorMetric,
) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(dims, value),
        encoding: VectorEncoding::F32,
        dims,
        metric,
        top_k,
        candidate_subjects: None,
    }
}

/// An `I8`-index search request (Model Y: the query bytes are canonical F32; `encoding` names the
/// stored/index encoding so the canister selects the fused i8×f32 kernels).
fn i8_search_req_metric_value(
    dims: u16,
    top_k: u32,
    value: f32,
    metric: VectorMetric,
) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(dims, value),
        encoding: VectorEncoding::I8,
        dims,
        metric,
        top_k,
        candidate_subjects: None,
    }
}

macro_rules! i8_search_bench {
    ($name:ident, $dims:expr, $top_k:expr, $metric:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_i8_search_store_metric($dims, SCAN_N, $metric);
            let value = if $metric == VectorMetric::Cosine {
                1.0
            } else {
                0.0
            };
            let req = i8_search_req_metric_value($dims, $top_k, value, $metric);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store
                    .vector_search(black_box(&req))
                    .expect("i8 vector_search");
                black_box(result);
            })
        }
    };
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
search_bench!(bench_vector_search_d1536_k10, 1536, 10);
search_bench!(bench_vector_search_d1536_k100, 1536, 100);

i8_search_bench!(
    bench_vector_search_i8_d1536_k10,
    1536,
    10,
    VectorMetric::L2Squared
);
i8_search_bench!(
    bench_vector_search_i8_d1536_k100,
    1536,
    100,
    VectorMetric::L2Squared
);

i8_search_bench!(
    bench_vector_search_cosine_i8_d1536_k10,
    1536,
    10,
    VectorMetric::Cosine
);
i8_search_bench!(
    bench_vector_search_cosine_i8_d1536_k100,
    1536,
    100,
    VectorMetric::Cosine
);

/// L2 metric-parameterized search bench over a cosine-seeded store (exact-scan path; cosine supports
/// only the exact scan). Uses a non-zero query value so the cosine query passes the zero-norm guard.
macro_rules! cosine_search_bench {
    ($name:ident, $dims:expr, $top_k:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_search_store_metric($dims, SCAN_N, VectorMetric::Cosine);
            let req = search_req_metric_value($dims, $top_k, 1.0, VectorMetric::Cosine);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store.vector_search(black_box(&req)).expect("vector_search");
                black_box(result);
            })
        }
    };
}

cosine_search_bench!(bench_vector_search_cosine_d128_k10, 128, 10);
cosine_search_bench!(bench_vector_search_cosine_d128_k100, 128, 100);
cosine_search_bench!(bench_vector_search_cosine_d1536_k10, 1536, 10);
cosine_search_bench!(bench_vector_search_cosine_d1536_k100, 1536, 100);

/// Cost of one `VECTOR_SUBJECT_TO_ID` lookup — the freshness revalidation the partitioned scan does
/// per row. Measures `SCAN_N` gets over a populated subject map so the per-get cost (stable BTreeMap
/// node decode) is isolated from the row reads and scoring. Per-get = total / SCAN_N.
#[bench(raw)]
fn bench_subject_map_get_d128() -> canbench_rs::BenchResult {
    let _store = setup_search_store(128, SCAN_N);
    let subjects: Vec<VectorSubject> = (0..SCAN_N)
        .map(|v| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: v,
        })
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_subject_map_get_d128");
        let mut sum = 0u64;
        for s in &subjects {
            let key = SubjectKey::new(INDEX_ID, *s);
            if let Some(e) = subject_store::get(&key).expect("subject lookup") {
                sum = sum.wrapping_add(e.stamp);
            }
        }
        black_box(sum);
    })
}

/// A constant-valued width-`dims` `f32` vector.
fn cvec(dims: u16, value: f32) -> Vec<f32> {
    vec![value; dims as usize]
}

/// Seeds a trained, clustered partitioned `ivf_flat` index: `nlist` centroids spaced by
/// `CLUSTER_SPACING`, with `n` vectors round-robin assigned to clusters and a tiny in-cluster jitter
/// so every vector is distinct yet nearest to its own centroid. Lower `nprobe` therefore skips whole
/// populated clusters.
fn setup_partitioned_store(dims: u16, n: u32, nlist: u32) -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .reset_for_test_or_bench(&VectorCanisterInitArgs {
            router_canister: router(),
            definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
            subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
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

/// A deterministic varied-direction raw f32 vector (values in `[-1, 1]`) for cosine partitioned
/// benches. The previous centroid construction `(c+1)*0.3 + j*0.01 + 1.0` is degenerate: for large
/// `dims` the `j*0.01` gradient dominates, so every centroid/row points in the same direction and the
/// cosine similarity to a constant query is uniformly high, which never exercises the early exit.
fn varied_raw(dims: u16, seed: u32) -> Vec<f32> {
    (0..dims)
        .map(|j| {
            let h = (seed.wrapping_mul(2654435761) ^ (j as u32).wrapping_mul(2246822519)) & 0xFFFF;
            (h as f32 / 65535.0) * 2.0 - 1.0
        })
        .collect()
}

/// Seeds a trained, clustered cosine partitioned `ivf_flat` index: **unit** centroids in distinct
/// directions (so L2-based partition selection is cosine-ordered), with `n` varied-direction vectors
/// assigned round-robin to clusters. Cosine rows are unit-normalized at append, so each cluster holds
/// unit vectors near its centroid direction; a smaller `eps_query` scans fewer populated clusters. The
/// first `COSINE_ALIGNED_ROWS` rows are aligned with the query `[1,1,..,1]` (distance 0) so the k-th
/// best distance is small and the Cauchy-Schwarz early exit triggers on the varied rows.
fn setup_partitioned_cosine_store(dims: u16, n: u32, nlist: u32) -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .reset_for_test_or_bench(&VectorCanisterInitArgs {
            router_canister: router(),
            definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
            subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
        })
        .expect("init");
    let centroids: Vec<Vec<f32>> = (0..nlist)
        .map(|c| {
            let raw = varied_raw(dims, c + 1);
            let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
            raw.iter().map(|x| x / norm).collect()
        })
        .collect();
    let vectors: Vec<(VectorSubject, Vec<f32>)> = (0..n)
        .map(|i| {
            let subject = VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: i,
            };
            if i < COSINE_ALIGNED_ROWS {
                // Aligned with the query `[1,1,..,1]` (distance 0) so the k-th best distance is small
                // and the Cauchy-Schwarz early exit triggers on the varied rows below.
                (subject, vec![1.0; dims as usize])
            } else {
                // Varied direction so the cosine similarity to the query varies (some rows far from
                // the query), exercising the early exit.
                (subject, varied_raw(dims, i + 1))
            }
        })
        .collect();
    store.seed_ivf_with_metric_for_test(
        INDEX_ID,
        VectorEncoding::F32,
        dims,
        VectorMetric::Cosine,
        &centroids,
        &vectors,
    );
    store
}

macro_rules! partitioned_bench {
    ($name:ident, $dims:expr, $nlist:expr, $eps_query:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_partitioned_store($dims, SCAN_N, $nlist);
            // The query sits between the first two clusters so the ε sweep actually varies how many
            // partitions are scanned (see [`SWEEP_QUERY`]).
            let req = search_req_value($dims, 10, SWEEP_QUERY);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store
                    .vector_search_tuned(
                        black_box(&req),
                        SearchTuning {
                            eps_query: $eps_query,
                        },
                    )
                    .expect("vector_search_tuned");
                black_box(result);
            })
        }
    };
}

// ε₂ sweep at fixed (dims, nlist) — demonstrates that a smaller eps_query reduces cost, and that
// eps_query = INF is the exact-parity upper bound.
partitioned_bench!(bench_ivf_d128_nlist16_eps0, 128, 16, 0.0);
partitioned_bench!(bench_ivf_d128_nlist16_eps05, 128, 16, 0.5);
partitioned_bench!(bench_ivf_d128_nlist16_eps1, 128, 16, 1.0);
partitioned_bench!(bench_ivf_d128_nlist16_epsinf, 128, 16, f32::INFINITY);
partitioned_bench!(bench_ivf_d128_nlist64_eps0, 128, 64, 0.0);
partitioned_bench!(bench_ivf_d128_nlist64_eps05, 128, 64, 0.5);
partitioned_bench!(bench_ivf_d128_nlist64_eps1, 128, 64, 1.0);

// Dimensional coverage at representative eps_query values.
partitioned_bench!(bench_ivf_d384_nlist16_eps0, 384, 16, 0.0);
partitioned_bench!(bench_ivf_d384_nlist16_eps05, 384, 16, 0.5);
partitioned_bench!(bench_ivf_d384_nlist64_eps1, 384, 64, 1.0);
partitioned_bench!(bench_ivf_d768_nlist16_eps0, 768, 16, 0.0);
partitioned_bench!(bench_ivf_d768_nlist16_eps05, 768, 16, 0.5);
partitioned_bench!(bench_ivf_d768_nlist64_eps1, 768, 64, 1.0);

// d = 1536 design target (ADR 0064 §8): ε₂ sweep at the per-level nlist ceilings that remain
// trainable at this width. With the query at [`SWEEP_QUERY`], eps_query = 0 scans only the nearest
// partition, 0.5 scans two, and INF is the exact-parity upper bound (all partitions).
partitioned_bench!(bench_ivf_d1536_nlist16_eps0, 1536, 16, 0.0);
partitioned_bench!(bench_ivf_d1536_nlist16_eps05, 1536, 16, 0.5);
partitioned_bench!(bench_ivf_d1536_nlist16_eps1, 1536, 16, 1.0);
partitioned_bench!(bench_ivf_d1536_nlist16_epsinf, 1536, 16, f32::INFINITY);
partitioned_bench!(bench_ivf_d1536_nlist64_eps0, 1536, 64, 0.0);
partitioned_bench!(bench_ivf_d1536_nlist64_eps05, 1536, 64, 0.5);
partitioned_bench!(bench_ivf_d1536_nlist64_eps1, 1536, 64, 1.0);
partitioned_bench!(bench_ivf_d1536_nlist256_eps0, 1536, 256, 0.0);
partitioned_bench!(bench_ivf_d1536_nlist256_eps05, 1536, 256, 0.5);
partitioned_bench!(bench_ivf_d1536_nlist256_eps1, 1536, 256, 1.0);

/// Cosine ε₂ sweep over a partitioned cosine index (unit centroids make L2 selection cosine-ordered):
/// a smaller `eps_query` scans fewer populated clusters, so `eps_query = INF` is the exact-parity
/// upper bound and the eps0/eps05 costs show the scan-row reduction vs the exact scan.
macro_rules! partitioned_cosine_bench {
    ($name:ident, $dims:expr, $nlist:expr, $eps_query:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_partitioned_cosine_store($dims, SCAN_N, $nlist);
            let req = search_req_metric_value($dims, 10, 1.0, VectorMetric::Cosine);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store
                    .vector_search_tuned(
                        black_box(&req),
                        SearchTuning {
                            eps_query: $eps_query,
                        },
                    )
                    .expect("vector_search_tuned");
                black_box(result);
            })
        }
    };
}

partitioned_cosine_bench!(bench_ivf_cosine_d1536_nlist64_eps0, 1536, 64, 0.0);
partitioned_cosine_bench!(bench_ivf_cosine_d1536_nlist64_eps05, 1536, 64, 0.5);
partitioned_cosine_bench!(
    bench_ivf_cosine_d1536_nlist64_epsinf,
    1536,
    64,
    f32::INFINITY
);

// --- ADR 0031 Slice 7: production shadow-version rebuild + dual-write ---

const REBUILD_N: u32 = 1024;

/// Drives a full rebuild (start -> bounded steps -> publish) of a degenerate index of `n` distinct
/// vectors into `nlist` partitions. Steps run in `n`-sized batches.
fn run_full_rebuild(store: &VectorCanisterStore, n: u32, nlist: u32) {
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, nlist, n + 1)
        .expect("start");
    loop {
        let status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, n)
            .expect("step");
        match status.phase {
            VectorRebuildPhase::ReadyToPublish => break,
            VectorRebuildPhase::Failed => panic!("rebuild failed"),
            _ => {}
        }
    }
    store
        .admin_publish_vector_rebuild(router(), INDEX_ID)
        .expect("publish");
}

/// Advances a freshly-started rebuild into `Building` (centroids written) without shadowing any
/// subject yet, so a follow-up `Building` step measures shadow-append cost in isolation.
fn start_into_building(store: &VectorCanisterStore, n: u32, nlist: u32) {
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, nlist, n + 1)
        .expect("start");
    loop {
        let status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, n)
            .expect("sampling/training step");
        if status.phase == VectorRebuildPhase::Building {
            break;
        }
        assert!(
            matches!(
                status.phase,
                VectorRebuildPhase::Sampling | VectorRebuildPhase::Training
            ),
            "unexpected phase before Building: {:?}",
            status.phase
        );
    }
}

/// Advances a freshly-started rebuild into `Training` (iteration 0, candidate pool collected, no
/// centroids written yet), so a follow-up `Training` step measures one k-means-lite iteration over
/// the full pool in isolation (ADR 0031 Slice 8).
fn start_into_training(store: &VectorCanisterStore, n: u32, nlist: u32) {
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, nlist, n + 1)
        .expect("start");
    loop {
        let status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, n)
            .expect("sampling step");
        match status.phase {
            VectorRebuildPhase::Training => break,
            VectorRebuildPhase::Sampling => {}
            other => panic!("unexpected phase before Training: {other:?}"),
        }
    }
}

fn new_subject_upsert(dims: u16, vid: u32, value: f32) -> VectorEmbeddingSyncOp {
    VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: vid,
        },
        mutation_id: 1,
        encoding: VectorEncoding::F32,
        dims,
        metric: VectorMetric::L2Squared,
        bytes: vec_bytes(dims, value),
        remove: false,
    }
}

macro_rules! rebuild_full_bench {
    ($name:ident, $dims:expr, $nlist:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_search_store($dims, REBUILD_N);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                run_full_rebuild(&store, REBUILD_N, $nlist);
            })
        }
    };
}

rebuild_full_bench!(bench_rebuild_full_d128_nlist16, 128, 16);
rebuild_full_bench!(bench_rebuild_full_d384_nlist16, 384, 16);
rebuild_full_bench!(bench_rebuild_full_d768_nlist64, 768, 64);

/// Full rebuild over well-separated clustered data (`setup_partitioned_store`), the realistic case
/// for embeddings. With furthest-point seeding + the early-convergence exit, k-means converges in a
/// few iterations, so this measures the real-world rebuild cost that the linear-ramp variant (a
/// k-means worst case) understates.
macro_rules! rebuild_full_clustered_bench {
    ($name:ident, $dims:expr, $nlist:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_partitioned_store($dims, REBUILD_N, $nlist);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                run_full_rebuild(&store, REBUILD_N, $nlist);
            })
        }
    };
}

rebuild_full_clustered_bench!(bench_rebuild_full_clustered_d128_nlist16, 128, 16);
rebuild_full_clustered_bench!(bench_rebuild_full_clustered_d384_nlist16, 384, 16);
rebuild_full_clustered_bench!(bench_rebuild_full_clustered_d768_nlist64, 768, 64);

macro_rules! training_step_bench {
    ($name:ident, $dims:expr, $nlist:expr) => {
        /// Cost of one k-means-lite `Training` iteration over the full candidate pool (ADR 0031
        /// Slice 8). This is the per-message work the candidate-pool cap bounds.
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_search_store($dims, REBUILD_N);
            start_into_training(&store, REBUILD_N, $nlist);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let status = store
                    .admin_vector_rebuild_step(router(), INDEX_ID, REBUILD_N)
                    .expect("training step");
                black_box(status);
            })
        }
    };
}

training_step_bench!(bench_rebuild_training_step_d128_nlist16, 128, 16);
training_step_bench!(bench_rebuild_training_step_d384_nlist16, 384, 16);
training_step_bench!(bench_rebuild_training_step_d768_nlist64, 768, 64);

/// Cost of one `Building` step that shadows all `REBUILD_N` subjects into their nearest partition.
#[bench(raw)]
fn bench_rebuild_building_step_d128_nlist16() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    start_into_building(&store, REBUILD_N, 16);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_rebuild_building_step_d128_nlist16");
        let status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, REBUILD_N)
            .expect("building step");
        black_box(status);
    })
}

/// Baseline: a normal new-subject upsert with no rebuild in flight.
#[bench(raw)]
fn bench_upsert_normal_d128() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    let op = new_subject_upsert(128, REBUILD_N + 1, 7.0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_upsert_normal_d128");
        store
            .vector_upsert(shard_owner(), black_box(&op))
            .expect("upsert");
    })
}

/// Baseline: an authoritative remove of an existing live subject with no rebuild in flight
/// (tombstones the live slot via the slab page store, ADR 0032).
#[bench(raw)]
fn bench_remove_normal_d128() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    let op = VectorEmbeddingSyncOp {
        index_id: INDEX_ID,
        embedding_name_id: 0,
        subject: VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: 0,
        },
        mutation_id: 1,
        encoding: VectorEncoding::F32,
        dims: 128,
        metric: VectorMetric::L2Squared,
        bytes: Vec::new(),
        remove: true,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_remove_normal_d128");
        store
            .vector_remove(shard_owner(), black_box(&op))
            .expect("remove");
    })
}

/// A new-subject upsert while `Building`: dual-writes into both the active and shadow versions.
#[bench(raw)]
fn bench_upsert_dualwrite_d128_nlist16() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    start_into_building(&store, REBUILD_N, 16);
    let op = new_subject_upsert(128, REBUILD_N + 1, 7.0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_upsert_dualwrite_d128_nlist16");
        store
            .vector_upsert(shard_owner(), black_box(&op))
            .expect("upsert");
    })
}

// --- ADR 0031 Slice 9: bounded page-meta health scan + heap centroid cache ---

/// Re-upserts vectors `0..tombstoned` of a degenerate store at a newer `embedding_version`, which
/// tombstones each subject's prior row (a new live row is appended). Produces a page store with a
/// known live/tombstone mix for the health-scan benchmark.
fn tombstone_first(store: &VectorCanisterStore, dims: u16, tombstoned: u32) {
    for vid in 0..tombstoned {
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: vid,
            },
            mutation_id: 2,
            encoding: VectorEncoding::F32,
            dims,
            metric: VectorMetric::L2Squared,
            bytes: vec_bytes(dims, vid as f32 + 0.5),
            remove: false,
        };
        store.vector_upsert(shard_owner(), &op).expect("re-upsert");
    }
}

/// Drives the bounded page-meta health scan over the active version to exhaustion, summing the
/// additive partials (the operator-side merge contract).
fn drive_health_scan(store: &VectorCanisterStore, max_pages: u32) -> u64 {
    let mut cursor: Option<Vec<u8>> = None;
    let mut total = 0u64;
    loop {
        let step = store
            .admin_vector_partition_health_step(router(), INDEX_ID, cursor, max_pages)
            .expect("health step");
        total += step.partial.total_rows;
        if step.exhausted {
            return total;
        }
        cursor = step.cursor;
    }
}

/// Full bounded page-meta health scan over a clean degenerate store (`REBUILD_N` live rows). Measures
/// the per-page-meta scan cost the tombstone-accounting endpoint adds.
#[bench(raw)]
fn bench_partition_health_scan_d128() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_partition_health_scan_d128");
        black_box(drive_health_scan(&store, REBUILD_N));
    })
}

/// Full health scan over a tombstone-heavy store (half the subjects re-upserted, so ~1.5x the pages
/// of the clean case carry tombstones). Regression guard for the scan cost when tombstones dominate.
#[bench(raw)]
fn bench_partition_health_scan_tombstoned_d128() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    tombstone_first(&store, 128, REBUILD_N / 2);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_partition_health_scan_tombstoned_d128");
        black_box(drive_health_scan(&store, REBUILD_N));
    })
}

macro_rules! cache_search_bench {
    ($name:ident, $dims:expr, $nlist:expr, $eps_query:expr, $warm:expr) => {
        /// Partition-page search with the heap centroid cache cold vs warm (ADR 0031 Slice 9). The
        /// warm variant first runs `admin_vector_centroid_cache_warmup` (an update path) so the
        /// `#[query]` search reads decoded centroids from the heap instead of `IVF_CENTROIDS`.
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_partitioned_store($dims, SCAN_N, $nlist);
            if $warm {
                store
                    .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
                    .expect("warmup");
            }
            let req = search_req($dims, 10);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store
                    .vector_search_tuned(
                        black_box(&req),
                        SearchTuning {
                            eps_query: $eps_query,
                        },
                    )
                    .expect("vector_search_tuned");
                black_box(result);
            })
        }
    };
}

cache_search_bench!(bench_ivf_cache_cold_d128_nlist64_eps1, 128, 64, 1.0, false);
cache_search_bench!(bench_ivf_cache_warm_d128_nlist64_eps1, 128, 64, 1.0, true);
cache_search_bench!(bench_ivf_cache_cold_d768_nlist64_eps1, 768, 64, 1.0, false);
cache_search_bench!(bench_ivf_cache_warm_d768_nlist64_eps1, 768, 64, 1.0, true);

/// Cost of a single centroid-cache warmup (`IVF_CENTROIDS` read + decode + heap insert) for an
/// `nlist`-partition index — the bounded update-path work an operator pays once per generation.
#[bench(raw)]
fn bench_centroid_cache_warmup_d768_nlist64() -> canbench_rs::BenchResult {
    let store = setup_partitioned_store(768, SCAN_N, 64);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_centroid_cache_warmup_d768_nlist64");
        let status = store
            .admin_vector_centroid_cache_warmup(router(), INDEX_ID)
            .expect("warmup");
        black_box(status);
    })
}

// --- ADR 0031 Slice 10: Router-forwarded maintenance step (one bounded vector unit) ---

/// A tombstone-required policy + per-step budgets (skew disabled) sized to complete each bounded unit
/// in a single step for the degenerate fixture — the snapshot the Router forwards per push.
fn maint_req(target_nlist: u32) -> VectorMaintenanceStepRequest {
    VectorMaintenanceStepRequest {
        policy: VectorMaintenancePolicy {
            recommended_tombstone_ratio_bps: 1_000,
            required_tombstone_ratio_bps: 2_000,
            recommended_skew_ratio_bps: u32::MAX,
            required_skew_ratio_bps: u32::MAX,
            min_total_rows: 1,
            min_tombstoned_rows: 1,
        },
        target_nlist: Some(target_nlist),
        sample_limit: REBUILD_N + 1,
        scan_max_pages: REBUILD_N,
        rebuild_max_subjects: REBUILD_N,
        cleanup_max_work: REBUILD_N,
    }
}

/// Drives a freshly-started rebuild to `ReadyToPublish` without publishing, so a follow-up
/// maintenance step measures the bounded `AwaitingPublish` no-op (publish stays explicit).
fn start_into_ready_to_publish(store: &VectorCanisterStore, n: u32, nlist: u32) {
    store
        .admin_start_vector_rebuild(router(), INDEX_ID, nlist, n + 1)
        .expect("start");
    loop {
        let status = store
            .admin_vector_rebuild_step(router(), INDEX_ID, n)
            .expect("step");
        match status.phase {
            VectorRebuildPhase::ReadyToPublish => break,
            VectorRebuildPhase::Failed => panic!("rebuild failed"),
            _ => {}
        }
    }
}

/// One Router-push maintenance unit that drives the bounded page-health scan (Idle -> Scanning + one
/// bounded `partition_page_health_step`). The scan/dispatch end of the maintenance step.
#[bench(raw)]
fn bench_maintenance_step_scan_d128() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    let req = maint_req(16);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_maintenance_step_scan_d128");
        let result = store
            .admin_vector_maintenance_step(router(), INDEX_ID, black_box(req))
            .expect("maintenance step");
        black_box(result);
    })
}

/// One maintenance unit while a rebuild is `Building`: the step drives one bounded rebuild step.
#[bench(raw)]
fn bench_maintenance_step_rebuild_d128_nlist16() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    start_into_building(&store, REBUILD_N, 16);
    let req = maint_req(16);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bench_maintenance_step_rebuild_d128_nlist16");
        let result = store
            .admin_vector_maintenance_step(router(), INDEX_ID, black_box(req))
            .expect("maintenance step");
        black_box(result);
    })
}

/// One maintenance unit at `ReadyToPublish`: the bounded no-op that returns `AwaitingPublish`. The
/// step-dispatch floor with no scan or rebuild work performed.
#[bench(raw)]
fn bench_maintenance_step_awaiting_publish_d128_nlist16() -> canbench_rs::BenchResult {
    let store = setup_search_store(128, REBUILD_N);
    start_into_ready_to_publish(&store, REBUILD_N, 16);
    let req = maint_req(16);
    canbench_rs::bench_fn(|| {
        let _scope =
            canbench_rs::bench_scope("bench_maintenance_step_awaiting_publish_d128_nlist16");
        let result = store
            .admin_vector_maintenance_step(router(), INDEX_ID, black_box(req))
            .expect("maintenance step");
        black_box(result);
    })
}

// --- ADR 0034 Slice 6: bounded candidate-subject exact search ---

/// Build a candidate allowlist of the first `count` live vertex subjects from the seeded store.
fn candidate_subjects(count: u32) -> Vec<VectorSubject> {
    (0..count)
        .map(|vid| VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: vid,
        })
        .collect()
}

fn filtered_search_req(
    dims: u16,
    top_k: u32,
    candidates: Vec<VectorSubject>,
) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: vec_bytes(dims, 0.0),
        encoding: VectorEncoding::F32,
        dims,
        metric: VectorMetric::L2Squared,
        top_k,
        candidate_subjects: Some(candidates),
    }
}

macro_rules! filtered_search_bench {
    ($name:ident, $dims:expr, $candidates:expr, $top_k:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let store = setup_search_store($dims, SCAN_N);
            let candidates = candidate_subjects($candidates);
            let req = filtered_search_req($dims, $top_k, candidates);
            canbench_rs::bench_fn(|| {
                let _scope = canbench_rs::bench_scope(stringify!($name));
                let result = store.vector_search(black_box(&req)).expect("vector_search");
                black_box(result);
            })
        }
    };
}

filtered_search_bench!(bench_filtered_search_d128_c128_k10, 128, 128, 10);
filtered_search_bench!(bench_filtered_search_d128_c128_k100, 128, 128, 100);
filtered_search_bench!(bench_filtered_search_d128_c1024_k10, 128, 1024, 10);
filtered_search_bench!(bench_filtered_search_d128_c1024_k100, 128, 1024, 100);
filtered_search_bench!(bench_filtered_search_d128_c4096_k10, 128, 4096, 10);
filtered_search_bench!(bench_filtered_search_d128_c4096_k100, 128, 4096, 100);

filtered_search_bench!(bench_filtered_search_d384_c128_k10, 384, 128, 10);
filtered_search_bench!(bench_filtered_search_d384_c1024_k10, 384, 1024, 10);
filtered_search_bench!(bench_filtered_search_d384_c4096_k10, 384, 4096, 10);

filtered_search_bench!(bench_filtered_search_d768_c128_k10, 768, 128, 10);
filtered_search_bench!(bench_filtered_search_d768_c1024_k10, 768, 1024, 10);
filtered_search_bench!(bench_filtered_search_d768_c4096_k10, 768, 4096, 10);

// --- `vector_sync_batch_outcome` admission (GAP-2026-07-17-001) ---
//
// The canister `vector_sync_batch_outcome` driver applies each operation with an
// instruction-budget exhausted check that reserves
// `VECTOR_BATCH_RESERVE_INSTRUCTIONS` (100M) below `VECTOR_BATCH_MAX_INSTRUCTIONS` (32B). These
// benches exercise that typed driver with the authorized attached-shard caller (a canbench query
// caller is not an attached shard) to measure representative and adversarial per-operation cost
// plus the exhausted-check overhead. The response-construction bench below measures both
// reachable public `Result` envelopes; it does not by itself establish an instruction or payload
// ceiling.

thread_local! {
    static SYNC_BENCH_SEQ: Cell<u32> = const { Cell::new(0) };
}

fn sync_ops(count: usize, dims: u16) -> Vec<VectorEmbeddingSyncOp> {
    let seq = SYNC_BENCH_SEQ.with(|c| {
        let n = c.get();
        c.set(n.wrapping_add(1));
        n
    });
    (0..count)
        .map(|index| VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: seq.wrapping_add(index as u32),
            },
            mutation_id: 1,
            encoding: VectorEncoding::F32,
            dims,
            metric: VectorMetric::L2Squared,
            bytes: vec_bytes(dims, index as f32),
            remove: false,
        })
        .collect()
}

/// A batch mixing existing-subject Greater updates (first `count/2` ops on pre-seeded subjects
/// `0..count/2`) with fresh upserts (last `count/2` ops on subjects `count..count+count/2`). Exercises
/// the canonical typed driver's per-operation path across existing and fresh subjects.
fn sync_ops_mixed(count: usize, dims: u16) -> Vec<VectorEmbeddingSyncOp> {
    (0..count)
        .map(|index| {
            let (vertex_id, mutation_id) = if index < count / 2 {
                (index as u32, 2) // existing subject, Greater update
            } else {
                (count as u32 + index as u32, 1) // fresh subject
            };
            VectorEmbeddingSyncOp {
                index_id: INDEX_ID,
                embedding_name_id: 0,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id,
                encoding: VectorEncoding::F32,
                dims,
                metric: VectorMetric::L2Squared,
                bytes: vec_bytes(dims, index as f32),
                remove: false,
            }
        })
        .collect()
}

fn sync_batch_outcome_round(
    caller: Principal,
    ops: &[VectorEmbeddingSyncOp],
) -> VectorSyncBatchOutcome {
    crate::canister::vector_sync_batch_outcome_for_caller(caller, ops)
        .expect("vector sync batch outcome")
}

fn assert_sync_batch_progress(caller: Principal, ops: &[VectorEmbeddingSyncOp]) {
    assert_eq!(
        sync_batch_outcome_round(caller, ops),
        VectorSyncBatchOutcome::Progress { applied: 256 },
        "typed sync benchmark fixture must apply the complete batch"
    );
}

/// The `vector_sync_batch_outcome` driver processing 256 upserts of 8-dimensional embeddings
/// (representative per-operation cost).
#[bench(raw)]
fn bench_vector_sync_batch_outcome_256() -> canbench_rs::BenchResult {
    let ops = sync_ops(256, 8);
    let _validation_store = setup_search_store(8, 0);
    assert_sync_batch_progress(shard_owner(), &ops);
    let store = setup_search_store(8, 0);
    canbench_rs::bench_fn(|| {
        black_box(&store);
        let outcome = sync_batch_outcome_round(shard_owner(), black_box(&ops));
        black_box(outcome)
    })
}

/// Adversarial: 256 upserts of 768-dimensional embeddings (3072 bytes each) — the maximum
/// single-operation cost the 100M reserve must cover.
#[bench(raw)]
fn bench_vector_sync_batch_outcome_256_768_dims() -> canbench_rs::BenchResult {
    let ops = sync_ops(256, 768);
    let _validation_store = setup_search_store(768, 0);
    assert_sync_batch_progress(shard_owner(), &ops);
    let store = setup_search_store(768, 0);
    canbench_rs::bench_fn(|| {
        black_box(&store);
        let outcome = sync_batch_outcome_round(shard_owner(), black_box(&ops));
        black_box(outcome)
    })
}

/// Mixed batch: 128 existing-subject Greater updates + 128 fresh upserts. Measures the canonical
/// typed driver's per-operation path across both subject states.
#[bench(raw)]
fn bench_vector_sync_batch_outcome_256_768_dims_mixed() -> canbench_rs::BenchResult {
    let ops = sync_ops_mixed(256, 768);
    let _validation_store = setup_search_store(768, 128); // pre-seed 128 subjects
    assert_sync_batch_progress(shard_owner(), &ops);
    let store = setup_search_store(768, 128); // pre-seed 128 subjects
    canbench_rs::bench_fn(|| {
        black_box(&store);
        let outcome = sync_batch_outcome_round(shard_owner(), black_box(&ops));
        black_box(outcome)
    })
}

/// Response construction: measure both public typed `Result` envelopes reachable from the driver.
#[bench(raw)]
fn bench_vector_sync_batch_outcome_encode() -> canbench_rs::BenchResult {
    let responses: [Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable>; 2] = [
        Ok(VectorSyncBatchOutcome::Progress { applied: 256 }),
        Ok(VectorSyncBatchOutcome::Terminal {
            applied: 33,
            failed_index: 33,
            error: VectorSyncTerminalError::SubjectTablePressure,
        }),
    ];
    for response in &responses {
        response
            .as_ref()
            .expect("reachable typed response")
            .validate(256)
            .expect("valid typed response envelope");
        Encode!(response).expect("encode sync outcome response");
    }
    canbench_rs::bench_fn(|| {
        let progress = Encode!(black_box(&responses[0])).expect("encode progress response");
        let terminal = Encode!(black_box(&responses[1])).expect("encode terminal response");
        black_box((progress, terminal))
    })
}
