//! Vector-search benchmarks (ADR 0031 Slice 5 exact scan + Slice 6 partition-page scan).
//!
//! The exact-scan benches establish the baseline cost of the degenerate `ivf_flat` exact scan —
//! live subject-map scan + L2-squared scoring + bounded top-k. The Slice 6 benches measure the
//! partition-page scan over **clustered** seeded datasets: `nprobe = nlist` is the parity point
//! (scans every partition, same result set as exact), and lower `nprobe` skips populated partitions
//! so the cost reduction is visible. The partition scan is *not* expected to match exact-scan
//! instruction cost even at `nprobe = nlist` — it adds centroid scoring plus the per-row subject-map
//! freshness re-validation (the row subject is rebuilt from the slab row-local locator, ADR 0032).
//! `l2_squared_f32` is kept isolated in the store so a future SIMD variant can be benchmarked against
//! these baselines.
//!
//! Run from `crates/graph-vector-index`: `canbench` (see `canbench.yml`).

use crate::facade::{SearchTuning, VectorIndexStore};
use crate::init::VectorIndexInitArgs;
use canbench_rs::bench;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMaintenancePolicy, VectorMaintenanceStepRequest,
    VectorMetric, VectorRebuildPhase, VectorSearchRequest, VectorSubject,
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
            metric: VectorMetric::L2Squared,
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
        candidate_subjects: None,
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

// --- ADR 0031 Slice 7: production shadow-version rebuild + dual-write ---

const REBUILD_N: u32 = 1024;

/// Drives a full rebuild (start -> bounded steps -> publish) of a degenerate index of `n` distinct
/// vectors into `nlist` partitions. Steps run in `n`-sized batches.
fn run_full_rebuild(store: &VectorIndexStore, n: u32, nlist: u32) {
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
fn start_into_building(store: &VectorIndexStore, n: u32, nlist: u32) {
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
fn start_into_training(store: &VectorIndexStore, n: u32, nlist: u32) {
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
        embedding_incarnation: 1,
        embedding_version: 1,
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
        embedding_incarnation: 1,
        embedding_version: 1,
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
fn tombstone_first(store: &VectorIndexStore, dims: u16, tombstoned: u32) {
    for vid in 0..tombstoned {
        let op = VectorEmbeddingSyncOp {
            index_id: INDEX_ID,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: vid,
            },
            embedding_incarnation: 1,
            embedding_version: 2,
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
fn drive_health_scan(store: &VectorIndexStore, max_pages: u32) -> u64 {
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
    ($name:ident, $dims:expr, $nlist:expr, $nprobe:expr, $warm:expr) => {
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
                    .vector_search_tuned(black_box(&req), SearchTuning { nprobe: $nprobe })
                    .expect("vector_search_tuned");
                black_box(result);
            })
        }
    };
}

cache_search_bench!(bench_ivf_cache_cold_d128_nlist64_nprobe8, 128, 64, 8, false);
cache_search_bench!(bench_ivf_cache_warm_d128_nlist64_nprobe8, 128, 64, 8, true);
cache_search_bench!(bench_ivf_cache_cold_d768_nlist64_nprobe8, 768, 64, 8, false);
cache_search_bench!(bench_ivf_cache_warm_d768_nlist64_nprobe8, 768, 64, 8, true);

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
fn start_into_ready_to_publish(store: &VectorIndexStore, n: u32, nlist: u32) {
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
