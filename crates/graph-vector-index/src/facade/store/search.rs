//! Read-only `ivf_flat` top-k search (ADR 0031 Slice 5 exact scan + Slice 6 partition-page scan).
//!
//! Two read paths share one freshness contract — `VECTOR_SUBJECT_TO_ID` is the source of truth for
//! which subjects are live, at which slot, and at which `(embedding_incarnation,
//! stored_embedding_version)` clock:
//!
//! - **Exact subject-map scan** (Slice 5): walk every live subject of the index and score its
//!   current slot. Used when the index is degenerate (`nlist <= 1`) or its centroids are not ready.
//! - **Partition-page scan** (Slice 6): score `query` against the index's centroids, select the
//!   `nprobe` nearest partitions, and scan only those partitions' page chains via the slab page
//!   store (ADR 0032). Each candidate row's subject is rebuilt from the row-local `subject_locator`
//!   (retiring `VECTOR_ID_TO_SUBJECT` from this hot path) and re-validated against the subject map so
//!   tombstoned / superseded / inconsistent rows are never scored.
//!
//! `nprobe` is the only recall knob: the selected partitions are scanned **in full**, so the result
//! is the exact top-k over those partitions. There is no mid-scan page/candidate budget that could
//! silently truncate the result (`VectorSearchResult` carries no partial/cursor marker).
//!
//! [`SubjectMapEntry`]: crate::records::SubjectMapEntry

use super::VectorIndexStore;
use crate::facade::stable::page_store::{PageScratch, RowHeader};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, PAGE_STORE, VECTOR_INDEX_DEFS, VECTOR_SUBJECT_TO_ID,
};
use crate::records::{PartitionKey, SlotRef, SubjectKey, VectorIndexDef};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_FILTER_CANDIDATES, MAX_VECTOR_SEARCH_TOP_K, VectorEncoding, VectorIndexError,
    VectorMetric, VectorSearchHit, VectorSearchRequest, VectorSearchResult, VectorSubject,
};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::ops::Bound;

/// Default number of partitions to probe when none is supplied. Clamped to `1..=nlist`.
const DEFAULT_NPROBE: u32 = 4;

/// Internal, algorithm-specific search tuning. Never crosses the Router/kernel wire (the public
/// request stays algorithm-neutral); built in-canister or supplied by tests/bench.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SearchTuning {
    /// Number of nearest centroid partitions to scan. Valid range is `1..=nlist`.
    pub nprobe: u32,
}

/// Squared Euclidean distance between two equal-length `f32` vectors. Isolated so a SIMD variant can
/// replace the inner loop later without changing search semantics (ADR 0031 Slice 5).
pub(super) fn l2_squared_f32(query: &[f32], vector: &[f32]) -> f32 {
    query
        .iter()
        .zip(vector.iter())
        .map(|(q, v)| {
            let d = q - v;
            d * d
        })
        .sum()
}

/// Returns `(dot, query_norm_sq, vector_norm_sq)` for cosine scoring. Used by `cosine_distance_f32`
/// so the caller can validate non-zero norms.
pub(super) fn cosine_dot_and_norms_f32(query: &[f32], vector: &[f32]) -> (f32, f32, f32) {
    let mut dot = 0.0f32;
    let mut q_norm_sq = 0.0f32;
    let mut v_norm_sq = 0.0f32;
    for (q, v) in query.iter().zip(vector.iter()) {
        dot += q * v;
        q_norm_sq += q * q;
        v_norm_sq += v * v;
    }
    (dot, q_norm_sq, v_norm_sq)
}

/// Cosine "distance" used internally by the top-k heap: `1 - cosine_similarity`.
///
/// Both vectors must be finite and have non-zero norm. The public entry points (`search_impl` and
/// `raw_distance_f32`) enforce this before calling this helper, but the assertions below keep the
/// invariant explicit and prevent silent NaN from propagating if a future caller bypasses them.
pub(super) fn cosine_distance_f32(query: &[f32], vector: &[f32]) -> f32 {
    assert!(
        vector_is_finite(query) && vector_has_nonzero_norm(query),
        "cosine_distance_f32 called with non-finite or zero-norm query"
    );
    assert!(
        vector_is_finite(vector) && vector_has_nonzero_norm(vector),
        "cosine_distance_f32 called with non-finite or zero-norm indexed vector"
    );
    let (dot, q_norm_sq, v_norm_sq) = cosine_dot_and_norms_f32(query, vector);
    let similarity = dot / (q_norm_sq.sqrt() * v_norm_sq.sqrt());
    1.0 - similarity
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

/// Computes the internal raw distance for a metric. Returns `None` for indexed vectors that should
/// be skipped (non-finite components or zero norm where required); never returns `NaN`/`Inf`.
fn raw_distance_f32(metric: VectorMetric, query: &[f32], vector: &[f32]) -> Option<f32> {
    // Both metrics require finite components to keep the top-k heap ordered and the Router seed
    // conversion contract honest. Zero norm only matters for cosine, but skipping NaN/Inf is
    // unconditional for all metrics in this slice.
    if !vector_is_finite(vector) {
        return None;
    }
    match metric {
        VectorMetric::L2Squared => Some(l2_squared_f32(query, vector)),
        VectorMetric::Cosine => {
            if !vector_has_nonzero_norm(vector) {
                return None;
            }
            Some(cosine_distance_f32(query, vector))
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

/// One scored candidate. Ordered by `(distance, subject)` with `f32::total_cmp` so a max-heap evicts
/// the farthest (then largest-subject) candidate first, keeping the `top_k` nearest with a
/// deterministic tie-break.
struct Candidate {
    distance: f32,
    subject: VectorSubject,
    embedding_incarnation: u64,
    embedding_version: u64,
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
            embedding_incarnation: c.embedding_incarnation,
            embedding_version: c.embedding_version,
        })
        .collect();
    VectorSearchResult { hits }
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
    let vector = decode_f32(bytes);
    let mut best = 0u32;
    let mut best_d = f32::INFINITY;
    for (p, centroid) in centroids.iter().enumerate() {
        let d = l2_squared_f32(centroid, &vector);
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

/// Selects the `nprobe` nearest centroid partitions to `query` (distance asc, partition id asc).
fn select_partitions(centroids: &[Vec<f32>], query: &[f32], nprobe: u32) -> Vec<u32> {
    let mut scored: Vec<(f32, u32)> = centroids
        .iter()
        .enumerate()
        .map(|(p, c)| (l2_squared_f32(query, c), p as u32))
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(nprobe as usize)
        .map(|(_, p)| p)
        .collect()
}

impl VectorIndexStore {
    /// Exact top-k vector search over the `ivf_flat` index (ADR 0031 Slice 5/6).
    ///
    /// Read-only: validates the request against the stored definition, selects the read path (exact
    /// subject-map scan for degenerate/untrained indexes, partition-page scan otherwise), and returns
    /// the `top_k` nearest ordered by `(distance ascending, subject ascending)`. Uses the in-canister
    /// default `nprobe` (clamped to `1..=nlist`).
    pub fn vector_search(
        &self,
        req: &VectorSearchRequest,
    ) -> Result<VectorSearchResult, VectorIndexError> {
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
    ) -> Result<VectorSearchResult, VectorIndexError> {
        self.search_impl(req, Some(tuning))
    }

    fn search_impl(
        &self,
        req: &VectorSearchRequest,
        tuning_override: Option<SearchTuning>,
    ) -> Result<VectorSearchResult, VectorIndexError> {
        if req.top_k == 0 || req.top_k > MAX_VECTOR_SEARCH_TOP_K {
            return Err(VectorIndexError::InvalidSearchTopK);
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
            return Err(VectorIndexError::DimensionMismatch);
        }
        if req.query.len() != def.stride_bytes as usize {
            return Err(VectorIndexError::ByteWidthMismatch);
        }

        let query = decode_f32(&req.query);
        if !vector_is_finite(&query)
            || (req.metric == VectorMetric::Cosine && !vector_has_nonzero_norm(&query))
        {
            return Err(VectorIndexError::InvalidQueryVector);
        }

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
            );
        }

        // Resolve tuning. The default path clamps defensively to `1..=nlist`; the tuned path rejects
        // out-of-range values (see `vector_search_tuned`).
        let nlist = def.nlist.max(1);
        let tuning = match tuning_override {
            Some(t) => {
                assert!(
                    t.nprobe >= 1 && t.nprobe <= nlist,
                    "tuned nprobe {} out of range 1..={nlist}",
                    t.nprobe
                );
                t
            }
            None => SearchTuning {
                nprobe: DEFAULT_NPROBE.clamp(1, nlist),
            },
        };

        // Mode selection: exact subject scan for degenerate or untrained indexes; otherwise the
        // partition-page scan. A stale/incomplete centroid set falls back to exact (no error).
        // Cosine only supports the exact-scan path in this slice.
        if def.nlist <= 1 || !centroids_ready(&def, req.index_id) {
            Ok(self.exact_subject_scan(req, def.active_index_version, &query, def.metric))
        } else if def.metric == VectorMetric::Cosine {
            Err(VectorIndexError::MetricNotSupportedForPartitionScan)
        } else {
            Ok(self.partition_page_scan(req, &def, &query, tuning))
        }
    }

    /// Validate a candidate allowlist before consulting the physical index definition.
    ///
    /// Fails closed for oversized, duplicate, or non-vertex candidates. The receiving canister must
    /// not depend on the Router to police the wire contract.
    fn validate_candidate_allowlist(candidates: &[VectorSubject]) -> Result<(), VectorIndexError> {
        if candidates.len() > MAX_VECTOR_SEARCH_FILTER_CANDIDATES {
            return Err(VectorIndexError::InvalidSearchCandidates);
        }
        let mut seen = std::collections::HashSet::with_capacity(candidates.len());
        for subject in candidates {
            if !matches!(subject, VectorSubject::Vertex { .. }) || !seen.insert(*subject) {
                return Err(VectorIndexError::InvalidSearchCandidates);
            }
        }
        Ok(())
    }

    /// Slice 6 exact scan restricted to a bounded candidate allowlist.
    ///
    /// Precondition: `candidates` has already passed [`validate_candidate_allowlist`], so the scan
    /// only resolves each subject to its current live slot, scores, and pushes through the bounded
    /// top-k heap. Deleted, stale, or superseded subjects are skipped silently; they represent
    /// derived-index drift rather than protocol violations.
    fn candidate_subject_scan(
        &self,
        index_id: u32,
        active_index_version: u64,
        query: &[f32],
        metric: VectorMetric,
        candidates: &[VectorSubject],
        top_k: u32,
    ) -> Result<VectorSearchResult, VectorIndexError> {
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

        PAGE_STORE.with_borrow(|store| {
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
                    let Some(expected_vector_id) = value.vector_id else {
                        continue;
                    };
                    let Some((header, bytes)) = store.read_row_bytes(index_id, slot) else {
                        continue;
                    };
                    if header.vector_id != expected_vector_id {
                        continue;
                    }
                    let Some(distance) = raw_distance_f32(metric, query, &decode_f32(&bytes))
                    else {
                        continue;
                    };
                    push_bounded(
                        &mut heap,
                        top_k,
                        Candidate {
                            distance,
                            subject: *subject,
                            embedding_incarnation: value.embedding_incarnation,
                            embedding_version: value.stored_embedding_version,
                        },
                    );
                }
            });
        });

        Ok(finalize(heap))
    }

    /// Slice 5 exact scan: walk every live subject of the index and score its current slot. The live
    /// slot is resolved via `current_slot_for(active)` (ADR 0031 Slice 7) so a post-publish exact
    /// fallback reads the new active version (`shadow_slot`), never the stale old `entry.slot`.
    fn exact_subject_scan(
        &self,
        req: &VectorSearchRequest,
        active_index_version: u64,
        query: &[f32],
        metric: VectorMetric,
    ) -> VectorSearchResult {
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

        PAGE_STORE.with_borrow(|store| {
            VECTOR_SUBJECT_TO_ID.with_borrow(|subjects| {
                let lower = SubjectKey::index_lower(req.index_id);
                for entry in subjects.range((Bound::Included(lower), Bound::Unbounded)) {
                    let key = entry.key();
                    if key.index_id != req.index_id {
                        break; // index-major order: past this index's prefix.
                    }
                    let value = entry.value();
                    if value.deleted {
                        continue;
                    }
                    let Some(slot) = value.current_slot_for(active_index_version) else {
                        continue;
                    };
                    // A live entry always carries a `vector_id`; a `slot: Some / vector_id: None` row
                    // is inconsistent drift and must be skipped, not scored.
                    let Some(expected_vector_id) = value.vector_id else {
                        continue;
                    };
                    // `read_row_bytes` already rejects tombstoned / stale-generation / out-of-range
                    // slots; the `vector_id` check closes the remaining drift case.
                    let Some((header, bytes)) = store.read_row_bytes(req.index_id, slot) else {
                        continue;
                    };
                    if header.vector_id != expected_vector_id {
                        continue;
                    }
                    // Skip indexed vectors that are non-finite or zero-norm for cosine; for L2Squared
                    // the raw value is always finite unless the bytes are non-finite, which is also
                    // skipped by this helper for consistency.
                    let Some(distance) = raw_distance_f32(metric, query, &decode_f32(&bytes))
                    else {
                        continue;
                    };
                    push_bounded(
                        &mut heap,
                        req.top_k,
                        Candidate {
                            distance,
                            subject: key.subject,
                            embedding_incarnation: value.embedding_incarnation,
                            embedding_version: value.stored_embedding_version,
                        },
                    );
                }
            });
        });

        finalize(heap)
    }

    /// Slice 6 partition-page scan: select `nprobe` nearest centroid partitions and scan their page
    /// chains in full, re-validating each row against the subject map before scoring.
    fn partition_page_scan(
        &self,
        req: &VectorSearchRequest,
        def: &VectorIndexDef,
        query: &[f32],
        tuning: SearchTuning,
    ) -> VectorSearchResult {
        // `centroids_ready` already verified the set is complete; default to exact-equivalent empty
        // if it somehow vanished between the gate and here.
        let Some(centroids) = read_centroids(def, req.index_id) else {
            return self.exact_subject_scan(req, def.active_index_version, query, def.metric);
        };
        let active = def.active_index_version;
        let selected = select_partitions(&centroids, query, tuning.nprobe);
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();
        let mut scratch = PageScratch::new();

        PAGE_STORE.with_borrow(|store| {
            for partition_id in selected {
                store.visit_partition_pages(
                    req.index_id,
                    active,
                    partition_id,
                    &mut scratch,
                    |slot, header, bytes| {
                        if let Some(candidate) = self.fresh_row_candidate(
                            req.index_id,
                            slot,
                            header,
                            query,
                            bytes,
                            def.metric,
                        ) {
                            push_bounded(&mut heap, req.top_k, candidate);
                        }
                    },
                );
            }
        });

        finalize(heap)
    }

    /// Re-validates a visited page row against the subject map and, if it is the subject's current
    /// live slot, returns a scored candidate. The subject is rebuilt from the row-local
    /// `subject_locator` (no `VECTOR_ID_TO_SUBJECT` read); `VECTOR_SUBJECT_TO_ID` remains the
    /// freshness source of truth. Returns `None` for any missing/deleted/mismatched subject entry or
    /// slot drift — the freshness contract shared with the exact scan.
    fn fresh_row_candidate(
        &self,
        index_id: u32,
        slot: SlotRef,
        header: &RowHeader,
        query: &[f32],
        bytes: &[u8],
        metric: VectorMetric,
    ) -> Option<Candidate> {
        let subject = header.subject();
        let entry =
            VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&SubjectKey::new(index_id, subject)))?;
        if entry.deleted || entry.vector_id != Some(header.vector_id) {
            return None;
        }
        // Pages are scanned at the active version, so the subject's live slot for that version
        // (active `slot`, or `shadow_slot` once an atomic publish flips active onto the rebuilt one)
        // must point at exactly this row (ADR 0031 Slice 7).
        if entry.current_slot_for(slot.index_version) != Some(slot) {
            return None;
        }
        let distance = raw_distance_f32(metric, query, &decode_f32(bytes))?;
        Some(Candidate {
            distance,
            subject,
            embedding_incarnation: entry.embedding_incarnation,
            embedding_version: entry.stored_embedding_version,
        })
    }
}
