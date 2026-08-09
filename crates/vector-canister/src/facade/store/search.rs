//! Read-only `ivf_flat` top-k search (ADR 0031 Slice 5 exact scan + Slice 6 partition-page scan).
//!
//! The partitioned and exact scans are **pure page-walks**: they bulk-read the selected partitions'
//! page chains via the slab page store (ADR 0064 §7) and score every non-tombstoned row directly,
//! with **no subject-map access**. The write path guarantees a non-tombstoned row in the active
//! version is the subject's current live slot (re-upsert tombstones the old row, remove tombstones
//! both active and shadow, publish cleans the old version), so the scored rows are exactly the live
//! rows. The subject is rebuilt from the run-table shard plus the packed `VertexPayload` vertex id.
//!
//! - **Exact scan** (Slice 5): walk every partition `0..nlist` at the active version. Used when the
//!   index is degenerate (`nlist <= 1`) or its centroids are not ready.
//! - **Partition-page scan** (Slice 6): score `query` against the index's centroids, select the
//!   ε₂-pruned partitions, and scan only those partitions' page chains.
//! - **Filtered scan** (ADR 0034): a bounded candidate allowlist is resolved to its current slots via
//!   `VECTOR_SUBJECT_TO_ID` (inherent to a bounded allowlist), then scored page-major.
//!
//! `eps_query` is the recall knob: the selected partitions are scanned **in full**, so the result is
//! the exact top-k over those partitions. There is no mid-scan page/candidate budget that could
//! silently truncate the result (`VectorSearchResult` carries no partial/cursor marker).
//!
//! [`FixedSubjectMapEntry`]: crate::records::FixedSubjectMapEntry

use super::VectorCanisterStore;
use crate::facade::stable::page_store::PageScratch;
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, PAGE_STORE, VECTOR_INDEX_DEFS, VECTOR_SUBJECT_TO_ID,
};
use crate::records::{PageKey, PartitionKey, SlotRef, SubjectKey, VectorIndexDef};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_FILTER_CANDIDATES, MAX_VECTOR_SEARCH_TOP_K, VectorCanisterError,
    VectorEncoding, VectorMetric, VectorSearchHit, VectorSearchRequest, VectorSearchResult,
    VectorSubject,
};
use ic_stable_vector_page_store::kernel::{dot_f32, l2_squared_f32, l2_squared_f32_early_exit};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Default ε₂ query-pruning factor when none is supplied. `0.0` scans only the nearest partition; a
/// larger value scans partitions within `(1 + eps_query) * dist(q, c_best)`.
///
/// Slice 6 decision: keep `0.0` (cost-minimal), confirmed by the d=1536 canbench ε₂ sweep. The
/// sweep isolates the cost model at the design target — ~144K ins per centroid (fixed, scales with
/// `nlist`) and ~164K ins per scanned row — so raising `eps_query` strictly adds the scanned-partition
/// row cost (one→two partitions measured +7% at nlist256 and +53% at nlist64). `0.0` is the right
/// global default for well-clustered data; the recall cost of a single-partition scan is documented
/// by `partition_scan_eps_zero_loses_boundary_recall_that_eps_positive_recovers`. The design's
/// per-definition `eps_query` recall escape hatch is not yet wired into the definition config (a
/// follow-up); until then every search uses this global default.
const DEFAULT_EPS_QUERY: f32 = 0.0;

/// Internal, algorithm-specific search tuning. Never crosses the Router/kernel wire (the public
/// request stays algorithm-neutral); built in-canister or supplied by tests/bench.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SearchTuning {
    /// ε₂ query-aware pruning factor: scan every partition with `dist(q, c_p) <= (1 + eps_query) *
    /// dist(q, c_best)`. `0.0` scans only the nearest; `f32::INFINITY` scans all.
    pub eps_query: f32,
}

/// Decodes contiguous little-endian `f32` components (`VectorEncoding::F32`).
pub(super) fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    let (chunks, _) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// Encodes `f32` components as contiguous little-endian bytes (inverse of [`decode_f32`]). Used by
/// the Slice 8 `Training` phase to persist refined centroids and by the test/bench seed helpers.
/// Validates that `v` contains only finite components.
fn vector_is_finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// Validates that `v` has a strictly positive squared norm (used by cosine).
fn vector_has_nonzero_norm(v: &[f32]) -> bool {
    let mut norm_sq = 0.0f32;
    for x in v {
        norm_sq += x * x;
    }
    norm_sq > 0.0
}

/// Scores one stored row for `metric`, returning `None` when the row should be skipped: non-finite
/// components, zero norm for cosine, or — for L2 — a partial distance that already exceeds
/// `threshold` (the current k-th best). Uses the crate's SIMD kernels over the stored byte span.
///
/// `q_norm` is the precomputed query norm (only meaningful for cosine). The L2 early exit is exact:
/// partial sums are monotone, so a partial sum exceeding `threshold` means the full distance also
/// does and the row cannot be in the top-k. For both L2 and cosine, finiteness is fused into the
/// kernel result (a non-finite component makes the sum / dot / norm non-finite), so there is no
/// separate `row_is_finite` pre-scan.
fn score_row(
    metric: VectorMetric,
    bytes: &[u8],
    query: &[f32],
    q_norm: f32,
    threshold: f32,
) -> Option<f32> {
    match metric {
        VectorMetric::L2Squared => l2_squared_f32_early_exit(bytes, query, threshold),
        VectorMetric::Cosine => {
            // Stored rows are unit-normalized (cosine indexes store unit vectors), so cosine distance
            // is 1 - dot/‖q‖ where q_norm = ‖query‖ is precomputed. `dot_f32` over the unit row and
            // the raw query is the cosine similarity — no per-row norm computation.
            let dot = dot_f32(bytes, query);
            // Fused finiteness: a non-finite component makes the dot non-finite; skip it here.
            if !dot.is_finite() {
                return None;
            }
            Some(1.0 - dot / q_norm)
        }
    }
}

pub(super) fn encode_f32(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Returns the unit-normalized encoding of the first `dims` f32 components of `bytes`, or `None` when
/// the vector has zero norm (undefined for cosine). Used to establish the stored-row-unit invariant
/// for cosine indexes (write path) and to normalize `op.bytes` for the idempotency comparison.
pub(super) fn normalize_f32(bytes: &[u8], dims: usize) -> Option<Vec<u8>> {
    let chunks = bytes[..dims * 4].as_chunks::<4>().0;
    let norm_sq: f32 = chunks
        .iter()
        .map(|c| {
            let x = f32::from_le_bytes(*c);
            x * x
        })
        .sum();
    if norm_sq == 0.0 {
        return None;
    }
    let inv = 1.0 / norm_sq.sqrt();
    let mut out = Vec::with_capacity(chunks.len() * 4);
    for c in chunks {
        let x = f32::from_le_bytes(*c) * inv;
        out.extend_from_slice(&x.to_le_bytes());
    }
    Some(out)
}

/// One scored candidate. Ordered by `(distance, subject)` with `f32::total_cmp` so a max-heap evicts
/// the farthest (then largest-subject) candidate first, keeping the `top_k` nearest with a
/// deterministic tie-break.
struct Candidate {
    distance: f32,
    subject: VectorSubject,
}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.subject.cmp(&other.subject))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Candidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate {}

/// Pushes a candidate into a `top_k`-bounded max-heap, evicting the farthest when over capacity.
fn push_bounded(heap: &mut BinaryHeap<Candidate>, top_k: u32, candidate: Candidate) {
    heap.push(candidate);
    if heap.len() as u32 > top_k {
        heap.pop();
    }
}

/// Drains the heap into the `(distance asc, subject asc)` result contract.
fn finalize(heap: BinaryHeap<Candidate>) -> VectorSearchResult {
    let hits = heap
        .into_sorted_vec()
        .into_iter()
        .map(|c| VectorSearchHit {
            subject: c.subject,
            distance: c.distance,
        })
        .collect();
    VectorSearchResult { hits }
}

/// Scores a page-major-sorted list of `(page_key, subject, slot)` rows, bulk-reading each
/// distinct page once into `scratch` and scoring from the zero-copy slice. Used by the candidate
/// (bounded allowlist) scan. The early-exit threshold is order-independent, so the walk order does
/// not change the result.
fn score_sorted_rows(
    query: &[f32],
    metric: VectorMetric,
    q_norm: f32,
    top_k: u32,
    heap: &mut BinaryHeap<Candidate>,
    rows: &[(PageKey, VectorSubject, SlotRef)],
    scratch: &mut PageScratch,
) {
    PAGE_STORE.with_borrow(|store| {
        let mut i = 0;
        while i < rows.len() {
            let page_key = rows[i].0;
            let page_loaded = store.load_page(page_key, scratch);
            let mut end = i + 1;
            while end < rows.len() && rows[end].0 == page_key {
                end += 1;
            }
            if page_loaded {
                let row_count = scratch.row_count();
                for (_, subject, slot) in &rows[i..end] {
                    // Defensive parity with `read_row_bytes`: a slot at/after `row_count` is
                    // uninitialized; a tombstoned row is not the live slot.
                    if slot.slot >= row_count || scratch.is_tombstoned(slot.slot) {
                        continue;
                    }
                    // Positional + payload validation: the row's packed vertex id must match the
                    // subject (the shard is known from the allowlist).
                    let vertex_id = scratch.row_info(slot.slot).vertex_id;
                    let VectorSubject::Vertex {
                        shard_id: _,
                        vertex_id: subject_vertex,
                    } = *subject;
                    if vertex_id != subject_vertex {
                        continue;
                    }
                    // The L2 early-exit threshold is the current k-th best, applied only once the
                    // heap is full (a partial max before then would wrongly skip top-k candidates).
                    let threshold = if heap.len() as u32 == top_k {
                        heap.peek().map(|c| c.distance).unwrap_or(f32::INFINITY)
                    } else {
                        f32::INFINITY
                    };
                    let bytes = scratch.vec_slice(slot.slot);
                    let Some(distance) = score_row(metric, bytes, query, q_norm, threshold) else {
                        continue;
                    };
                    push_bounded(
                        heap,
                        top_k,
                        Candidate {
                            distance,
                            subject: *subject,
                        },
                    );
                }
            }
            i = end;
        }
    });
}

/// Bulk-reads the given partitions' page chains at the active version and scores every
/// non-tombstoned row directly, with **no subject-map access**. `visit_partition_pages` already skips
/// tombstoned rows, and the write path guarantees a non-tombstoned row in the active version is the
/// subject's current live slot (invariant: re-upsert tombstones the old row, remove tombstones both
/// active and shadow, publish cleans the old version), so the scored rows are exactly the live rows.
/// Shared by the partitioned scan (ε₂-selected partitions) and the exact fallback (`0..nlist`). The
/// early-exit threshold is order-independent, so the walk order does not change the result.
fn scan_partitions(
    index_id: u32,
    active_index_version: u64,
    partitions: impl Iterator<Item = u32>,
    query: &[f32],
    metric: VectorMetric,
    q_norm: f32,
    top_k: u32,
) -> VectorSearchResult {
    let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();
    let mut scratch = PageScratch::new();
    PAGE_STORE.with_borrow(|store| {
        for partition_id in partitions {
            store.visit_partition_pages(
                index_id,
                active_index_version,
                partition_id,
                &mut scratch,
                |_slot, info, bytes| {
                    // The L2 early-exit threshold is the current k-th best, applied only once the
                    // heap is full (a partial max before then would wrongly skip top-k candidates).
                    let threshold = if heap.len() as u32 == top_k {
                        heap.peek().map(|c| c.distance).unwrap_or(f32::INFINITY)
                    } else {
                        f32::INFINITY
                    };
                    let subject = VectorSubject::Vertex {
                        shard_id: ShardId::new(info.shard_id),
                        vertex_id: info.vertex_id,
                    };
                    let Some(distance) = score_row(metric, bytes, query, q_norm, threshold) else {
                        return;
                    };
                    push_bounded(&mut heap, top_k, Candidate { distance, subject });
                },
            );
        }
    });
    finalize(heap)
}

/// Reads centroids `0..nlist` for `(index_id, version)`, returning `None` unless exactly `nlist`
/// centroids of `dims` components are present (a partial/stale centroid set is not ready). Shared by
/// search (active version) and the rebuild build/publish paths (shadow target version, Slice 7).
pub(super) fn read_centroids_at(
    index_id: u32,
    version: u64,
    nlist: u32,
    dims: u16,
) -> Option<Vec<Vec<f32>>> {
    let mut centroids = Vec::with_capacity(nlist as usize);
    IVF_CENTROIDS.with_borrow(|m| {
        for p in 0..nlist {
            let bytes = m.get(&PartitionKey::new(index_id, version, p))?;
            let centroid = decode_f32(&bytes);
            if centroid.len() != dims as usize {
                return None;
            }
            centroids.push(centroid);
        }
        Some(())
    })?;
    Some(centroids)
}

/// Reads centroids `0..nlist` for `(index_id, active_version)`, returning `None` unless exactly
/// `nlist` centroids of `dims` components are present (a partial/stale centroid set is not ready).
///
/// Consults the heap centroid cache first (ADR 0031 Slice 9): a warmed entry returns immediately,
/// skipping the `IVF_CENTROIDS` stable read + `f32` decode. A miss falls back to the stable read for
/// this call only and does **not** populate the cache (a `#[query]`'s heap writes do not commit on
/// IC; warmup is an explicit `#[update]`).
fn read_centroids(def: &VectorIndexDef, index_id: u32) -> Option<Vec<Vec<f32>>> {
    if let Some(centroids) =
        super::centroid_cache::lookup(index_id, def.active_index_version, def.nlist, def.dims)
    {
        return Some(centroids);
    }
    read_centroids_at(index_id, def.active_index_version, def.nlist, def.dims)
}

/// Nearest-centroid partition id for an encoded vector (ADR 0031 Slice 6/7). Ties break to the
/// lowest partition id. Shared by the rebuild shadow build, dual-write shadow append, and
/// post-publish `nlist > 1` active upserts.
pub(super) fn assign_partition(centroids: &[Vec<f32>], bytes: &[u8]) -> u32 {
    let mut best = 0u32;
    let mut best_d = f32::INFINITY;
    for (p, centroid) in centroids.iter().enumerate() {
        // SIMD L2 over the encoded row bytes and the decoded centroid (same kernel and tie-break as
        // `training_step`), avoiding a `decode_f32` allocation and the scalar per-centroid loop.
        let d = l2_squared_f32(bytes, centroid);
        if d < best_d {
            best_d = d;
            best = p as u32;
        }
    }
    best
}

/// Whether the index has a ready, current, complete centroid set for the partition-page scan.
fn centroids_ready(def: &VectorIndexDef, index_id: u32) -> bool {
    let Some(meta) = IVF_CENTROID_META.with_borrow(|m| m.get(&index_id)) else {
        return false;
    };
    meta.centroid_ready
        && meta.trained_index_version == def.active_index_version
        && read_centroids(def, index_id).is_some()
}

/// Selects the partitions to scan under ε₂ query-aware pruning (ADR 0064 §9): every partition whose
/// centroid distance to the query is within `(1 + eps_query) * dist(q, c_best)` of the nearest
/// centroid. Deterministic `(distance asc, partition id asc)` order. `eps_query = 0` selects only the
/// nearest partition(s); a large value (e.g. `f32::INFINITY`) selects all.
///
/// `query_bytes` is the request's encoded query (≥ `centroid.len() * 4`); distances are scored with
/// the SIMD `l2_squared_f32(bytes, centroid)` kernel.
fn select_partitions(centroids: &[Vec<f32>], query_bytes: &[u8], eps_query: f32) -> Vec<u32> {
    let mut scored: Vec<(f32, u32)> = centroids
        .iter()
        .enumerate()
        .map(|(p, c)| (l2_squared_f32(query_bytes, c), p as u32))
        .collect();
    // A full scan (`eps_query = INF`) selects every partition. Special-case it: the generic
    // `(1 + eps) * best` threshold degenerates to `INF * 0 = NaN` when the query sits exactly on a
    // centroid (`best == 0`), which would filter out every partition.
    if eps_query == f32::INFINITY {
        scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        return scored.into_iter().map(|(_, p)| p).collect();
    }
    let best = scored.iter().map(|(d, _)| *d).fold(f32::INFINITY, f32::min);
    let threshold = (1.0 + eps_query) * best;
    scored.retain(|(d, _)| *d <= threshold);
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, p)| p).collect()
}

impl VectorCanisterStore {
    /// Exact top-k vector search over the `ivf_flat` index (ADR 0031 Slice 5/6).
    ///
    /// Read-only: validates the request against the stored definition, selects the read path (exact
    /// subject-map scan for degenerate/untrained indexes, partition-page scan otherwise), and returns
    /// the `top_k` nearest ordered by `(distance ascending, subject ascending)`. Uses the in-canister
    /// default `nprobe` (clamped to `1..=nlist`).
    pub fn vector_search(
        &self,
        req: &VectorSearchRequest,
    ) -> Result<VectorSearchResult, VectorCanisterError> {
        self.search_impl(req, None)
    }

    /// Test/bench entry point that overrides `nprobe`. Out-of-range `nprobe` (`0` or `> nlist`) is a
    /// caller bug and panics, rather than silently returning fewer/empty hits and masking a
    /// regression. This is an internal assertion distinct from the public `InvalidSearchTopK` wire
    /// error.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn vector_search_tuned(
        &self,
        req: &VectorSearchRequest,
        tuning: SearchTuning,
    ) -> Result<VectorSearchResult, VectorCanisterError> {
        self.search_impl(req, Some(tuning))
    }

    fn search_impl(
        &self,
        req: &VectorSearchRequest,
        tuning_override: Option<SearchTuning>,
    ) -> Result<VectorSearchResult, VectorCanisterError> {
        if req.top_k == 0 || req.top_k > MAX_VECTOR_SEARCH_TOP_K {
            return Err(VectorCanisterError::InvalidSearchTopK);
        }
        // ADR 0034 Slice 6: a bounded candidate allowlist restricts the search to an exact top-k
        // over current live vector slots. Validate the allowlist shape before the physical def check
        // so protocol violations fail closed even on an empty index.
        if let Some(candidates) = &req.candidate_subjects {
            Self::validate_candidate_allowlist(candidates)?;
        }
        // The physical def is created lazily on the first upsert (see `mutation.rs`). A
        // Router-registered, activated index with no embeddings yet has no physical def, but it is a
        // known-empty index, not an unknown one — return an empty result rather than `UnknownIndex`.
        let Some(def) = VECTOR_INDEX_DEFS.with_borrow(|defs| defs.get(&req.index_id)) else {
            return Ok(VectorSearchResult { hits: Vec::new() });
        };
        // The request must agree with the stored definition; F32 encoding is the only supported
        // encoding in this slice.
        if req.encoding != VectorEncoding::F32
            || req.encoding != def.encoding
            || req.metric != def.metric
            || req.dims != def.dims
        {
            return Err(VectorCanisterError::DimensionMismatch);
        }
        if req.query.len() != def.stride_bytes as usize {
            return Err(VectorCanisterError::ByteWidthMismatch);
        }

        let query = decode_f32(&req.query);
        if !vector_is_finite(&query)
            || (req.metric == VectorMetric::Cosine && !vector_has_nonzero_norm(&query))
        {
            return Err(VectorCanisterError::InvalidQueryVector);
        }
        // Precompute the query norm once for cosine scoring (the query is validated non-zero-norm).
        let q_norm = if req.metric == VectorMetric::Cosine {
            query.iter().map(|x| x * x).sum::<f32>().sqrt()
        } else {
            0.0
        };

        // ADR 0034 Slice 6: a bounded candidate allowlist restricts the search to an exact top-k
        // over current live vector slots. The receiving boundary validates count, vertex-only
        // subjects, and duplicates independently of the Router.
        if let Some(candidates) = &req.candidate_subjects {
            return self.candidate_subject_scan(
                req.index_id,
                def.active_index_version,
                &query,
                def.metric,
                candidates,
                req.top_k,
                q_norm,
            );
        }

        // Resolve tuning. The default path uses `DEFAULT_EPS_QUERY`; the tuned path rejects a
        // negative `eps_query` (see `vector_search_tuned`).
        let tuning = match tuning_override {
            Some(t) => {
                assert!(
                    t.eps_query >= 0.0,
                    "tuned eps_query {} must be >= 0",
                    t.eps_query
                );
                t
            }
            None => SearchTuning {
                eps_query: DEFAULT_EPS_QUERY,
            },
        };

        // Mode selection: exact subject scan for degenerate or untrained indexes; otherwise the
        // partition-page scan. A stale/incomplete centroid set falls back to exact (no error).
        if def.nlist <= 1 || !centroids_ready(&def, req.index_id) {
            Ok(self.exact_subject_scan(
                req,
                def.active_index_version,
                def.nlist,
                &query,
                def.metric,
                q_norm,
            ))
        } else {
            Ok(self.partition_page_scan(req, &def, &query, tuning, q_norm))
        }
    }

    /// Validate a candidate allowlist before consulting the physical index definition.
    ///
    /// Fails closed for oversized, duplicate, or non-vertex candidates. The receiving canister must
    /// not depend on the Router to police the wire contract.
    fn validate_candidate_allowlist(
        candidates: &[VectorSubject],
    ) -> Result<(), VectorCanisterError> {
        if candidates.len() > MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            return Err(VectorCanisterError::InvalidSearchCandidates);
        }
        let mut seen = std::collections::HashSet::with_capacity(candidates.len());
        for subject in candidates {
            if !matches!(subject, VectorSubject::Vertex { .. }) || !seen.insert(*subject) {
                return Err(VectorCanisterError::InvalidSearchCandidates);
            }
        }
        Ok(())
    }

    /// Filtered scan over a bounded candidate allowlist, batch-read page-major.
    ///
    /// Precondition: `candidates` has already passed [`validate_candidate_allowlist`], so the scan
    /// only resolves each subject to its current live slot, scores, and pushes through the bounded
    /// top-k heap. Deleted, stale, or superseded subjects are skipped silently; they represent
    /// derived-index drift rather than protocol violations.
    ///
    /// Pass 1 resolves each live candidate's current slot via the subject map (inherent to a bounded
    /// allowlist); pass 2 groups those slots by page and bulk-reads each distinct page once (via
    /// [`VectorSlabStore::load_page`] into a reused [`PageScratch`]) instead of calling `read_row_bytes`
    /// once per candidate. The early-exit threshold is order-independent (a partial sum already
    /// exceeding the k-th best proves the row cannot be in the top-k), so the page-major order yields
    /// the same exact top-k.
    fn candidate_subject_scan(
        &self,
        index_id: u32,
        active_index_version: u64,
        query: &[f32],
        metric: VectorMetric,
        candidates: &[VectorSubject],
        top_k: u32,
        q_norm: f32,
    ) -> Result<VectorSearchResult, VectorCanisterError> {
        // Pass 1: resolve current slots. The list is bounded by `MAX_VECTOR_SEARCH_FILTER_CANDIDATES`.
        let mut rows: Vec<(PageKey, VectorSubject, SlotRef)> = Vec::with_capacity(candidates.len());
        VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
            for subject in candidates {
                let key = SubjectKey::new(index_id, *subject);
                let Some(value) = subjects.get(&key) else {
                    continue;
                };
                if value.deleted {
                    continue;
                }
                let Some(slot) = value.current_slot_for(active_index_version) else {
                    continue;
                };
                let page_key = PageKey::new(
                    index_id,
                    slot.index_version as u64,
                    slot.partition_id,
                    slot.page_id as u64,
                );
                rows.push((page_key, *subject, slot));
            }
        });
        if rows.is_empty() {
            return Ok(VectorSearchResult { hits: Vec::new() });
        }
        // Pass 2: page-major order so each distinct page is bulk-read once.
        rows.sort_by_key(|(page, _, slot)| (*page, slot.slot));

        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();
        let mut scratch = PageScratch::new();
        score_sorted_rows(query, metric, q_norm, top_k, &mut heap, &rows, &mut scratch);

        Ok(finalize(heap))
    }

    /// Exact fallback scan: bulk-read every partition `0..nlist` at the active version and score each
    /// non-tombstoned row directly (no subject-map access; the write path guarantees a non-tombstoned
    /// row is the subject's current live slot). Walking all partitions covers the degenerate `nlist = 1`
    /// case (all rows in partition 0) and a trained-then-cleared index (`nlist > 1`, centroids missing)
    /// whose rows are spread across `0..nlist`. The deterministic top-k and shadow/current-slot handling
    /// are unchanged.
    fn exact_subject_scan(
        &self,
        req: &VectorSearchRequest,
        active_index_version: u64,
        nlist: u32,
        query: &[f32],
        metric: VectorMetric,
        q_norm: f32,
    ) -> VectorSearchResult {
        scan_partitions(
            req.index_id,
            active_index_version,
            0..nlist,
            query,
            metric,
            q_norm,
            req.top_k,
        )
    }

    /// Slice 6 partition-page scan: select the ε₂-pruned centroid partitions and scan their page
    /// chains in full, scoring every non-tombstoned row directly (no subject-map access; the write
    /// path guarantees a non-tombstoned row is the subject's current live slot).
    fn partition_page_scan(
        &self,
        req: &VectorSearchRequest,
        def: &VectorIndexDef,
        query: &[f32],
        tuning: SearchTuning,
        q_norm: f32,
    ) -> VectorSearchResult {
        // `centroids_ready` already verified the set is complete; default to exact-equivalent empty
        // if it somehow vanished between the gate and here.
        let Some(centroids) = read_centroids(def, req.index_id) else {
            return self.exact_subject_scan(
                req,
                def.active_index_version,
                def.nlist,
                query,
                def.metric,
                q_norm,
            );
        };
        let active = def.active_index_version;
        let selected = select_partitions(&centroids, &req.query, tuning.eps_query);
        scan_partitions(
            req.index_id,
            active,
            selected.into_iter(),
            query,
            def.metric,
            q_norm,
            req.top_k,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[f32]) -> Vec<u8> {
        encode_f32(values)
    }

    #[test]
    fn select_partitions_eps_zero_selects_nearest() {
        let centroids = vec![vec![0.0f32; 4], vec![10.0f32; 4]];
        let query = vec![0.5f32; 4];
        let qb = encode_f32(&query);
        assert_eq!(select_partitions(&centroids, &qb, 0.0), vec![0]);
        assert_eq!(
            select_partitions(&centroids, &qb, f32::INFINITY),
            vec![0, 1]
        );
    }

    #[test]
    fn select_partitions_eps_inf_with_query_at_centroid_selects_all() {
        // Regression: a full scan (`eps = INF`) must select every partition even when the query sits
        // exactly on a centroid (`best == 0`). The old `(1 + INF) * best = INF * 0 = NaN` threshold
        // filtered out all partitions, returning an empty scan.
        let centroids = vec![vec![2.5f32; 4], vec![0.5f32; 4], vec![4.5f32; 4]];
        let qb = encode_f32(&[2.5f32; 4]);
        assert_eq!(
            select_partitions(&centroids, &qb, f32::INFINITY),
            vec![0, 1, 2],
            "full scan selects every partition in (distance, partition) order"
        );
    }

    #[test]
    fn select_partitions_eps_threshold_boundary() {
        // Query exactly at centroid 0: threshold = 0, so only partition 0 is selected even with a
        // large eps_query.
        let centroids = vec![vec![0.0f32; 4], vec![10.0f32; 4]];
        let qb = encode_f32(&[0.0f32; 4]);
        assert_eq!(select_partitions(&centroids, &qb, 1.0), vec![0]);
    }

    #[test]
    fn score_row_l2_early_exit_skips_beyond_threshold() {
        let q = vec![0.0f32; 4];
        // Row at distance 4.0 (all components 1.0).
        let bytes = row(&[1.0f32; 4]);
        // Threshold 1.0: the partial sum exceeds it -> None.
        assert_eq!(
            score_row(VectorMetric::L2Squared, &bytes, &q, 0.0, 1.0),
            None
        );
        // Threshold 4.0: exact tie -> Some(4.0).
        assert_eq!(
            score_row(VectorMetric::L2Squared, &bytes, &q, 0.0, 4.0),
            Some(4.0)
        );
        // Threshold INF: full sum.
        assert_eq!(
            score_row(VectorMetric::L2Squared, &bytes, &q, 0.0, f32::INFINITY),
            Some(4.0)
        );
    }

    #[test]
    fn score_row_skips_non_finite_row() {
        let q = vec![0.0f32; 4];
        let bytes = row(&[f32::NAN, 1.0, 1.0, 1.0]);
        assert_eq!(
            score_row(VectorMetric::L2Squared, &bytes, &q, 0.0, f32::INFINITY),
            None
        );
    }

    #[test]
    fn score_row_cosine_matches_scalar() {
        let q = vec![1.0f32, 2.0, 3.0, 4.0];
        let v = [2.0f32, 0.0, 3.0, 1.0];
        // Cosine stores unit-normalized rows (invariant), so score against the normalized vector.
        let v_norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let vn: Vec<f32> = v.iter().map(|x| x / v_norm).collect();
        let bytes = row(&vn);
        let q_norm = q.iter().map(|x| x * x).sum::<f32>().sqrt();
        let dot: f32 = q.iter().zip(vn.iter()).map(|(a, b)| a * b).sum();
        let expected = 1.0 - dot / q_norm;
        let got =
            score_row(VectorMetric::Cosine, &bytes, &q, q_norm, f32::INFINITY).expect("cosine");
        assert!((got - expected).abs() < 1e-6);
    }

    #[test]
    fn score_row_cosine_skips_non_finite_row() {
        let q = vec![0.0f32; 4];
        // A NaN/Inf component makes the dot non-finite; the fused check skips it (no row_is_finite
        // pre-pass). Zero-norm rows are rejected at ingest and never stored, so they are not tested
        // here.
        let nan = row(&[f32::NAN, 1.0, 1.0, 1.0]);
        assert_eq!(
            score_row(
                VectorMetric::Cosine,
                &nan,
                &q,
                q.iter().map(|x| x * x).sum::<f32>().sqrt(),
                f32::INFINITY
            ),
            None
        );
        let inf = row(&[f32::INFINITY, 1.0, 1.0, 1.0]);
        assert_eq!(
            score_row(VectorMetric::Cosine, &inf, &q, 0.0, f32::INFINITY),
            None
        );
    }

    #[test]
    fn assign_partition_nearest_centroid_and_tie_break() {
        let centroids = vec![vec![10.0f32; 4], vec![0.0f32; 4]];
        // Value 0 is nearest to centroid 1 (distance 0), value 10 to centroid 0.
        assert_eq!(assign_partition(&centroids, &row(&[0.0f32; 4])), 1);
        assert_eq!(assign_partition(&centroids, &row(&[10.0f32; 4])), 0);
        // Exact tie keeps the lowest partition id.
        let tie = vec![vec![3.0f32; 4], vec![3.0f32; 4]];
        assert_eq!(assign_partition(&tie, &row(&[3.0f32; 4])), 0);
    }
}
