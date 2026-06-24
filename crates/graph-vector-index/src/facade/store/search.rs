//! Read-only `ivf_flat` top-k search (ADR 0031 Slice 5 exact scan + Slice 6 partition-page scan).
//!
//! Two read paths share one freshness contract — `VECTOR_SUBJECT_TO_ID` is the source of truth for
//! which subjects are live, at which slot, and at which `(embedding_incarnation,
//! stored_embedding_version)` clock:
//!
//! - **Exact subject-map scan** (Slice 5): walk every live subject of the index and score its
//!   current slot. Used when the index is degenerate (`nlist <= 1`) or its centroids are not ready.
//! - **Partition-page scan** (Slice 6): score `query` against the index's centroids, select the
//!   `nprobe` nearest partitions, and scan only those partitions' page chains. Each candidate row is
//!   reverse-mapped to its subject (`VECTOR_ID_TO_SUBJECT`) and re-validated against the subject map
//!   so tombstoned / superseded / inconsistent rows are never scored.
//!
//! `nprobe` is the only recall knob: the selected partitions are scanned **in full**, so the result
//! is the exact top-k over those partitions. There is no mid-scan page/candidate budget that could
//! silently truncate the result (`VectorSearchResult` carries no partial/cursor marker).
//!
//! [`SubjectMapEntry`]: crate::records::SubjectMapEntry

use super::VectorIndexStore;
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, VECTOR_ID_TO_SUBJECT, VECTOR_INDEX_DEFS, VECTOR_PAGE,
    VECTOR_SUBJECT_TO_ID,
};
use crate::records::{
    PageKey, PartitionKey, SlotRef, SubjectKey, VectorIdKey, VectorIndexDef, VectorPage,
};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_TOP_K, VectorEncoding, VectorIndexError, VectorMetric, VectorSearchHit,
    VectorSearchRequest, VectorSearchResult, VectorSubject,
};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
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

/// Decodes contiguous little-endian `f32` components (`VectorEncoding::F32`).
pub(super) fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
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

/// Reads the stored bytes of a live slot, decoding each page at most once per query via `cache`.
///
/// Normal mutation keeps the subject map pointing at the current live row, so these checks should
/// never fire; they cheaply enforce the search contract (tombstoned / old-generation rows are never
/// scored) and turn any drift in stable state into a skipped inconsistent row rather than a silent
/// bad hit.
fn slot_bytes_cached(
    cache: &mut HashMap<PageKey, VectorPage>,
    index_id: u32,
    slot: SlotRef,
    expected_vector_id: Option<u64>,
) -> Option<Vec<u8>> {
    let page_key = PageKey::new(
        index_id,
        slot.index_version,
        slot.partition_id,
        slot.page_id,
    );
    let page = cache.entry(page_key).or_insert_with(|| {
        VECTOR_PAGE.with_borrow(|pages| pages.get(&page_key).unwrap_or_else(VectorPage::empty))
    });
    let row = page.rows.get(slot.slot as usize)?;
    // A live entry always carries a `vector_id`; a `slot: Some / vector_id: None` row is inconsistent
    // drift and must be skipped, not scored.
    let expected_vector_id = expected_vector_id?;
    if row.tombstoned || row.generation != slot.generation || row.vector_id != expected_vector_id {
        return None;
    }
    Some(row.bytes.clone())
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
fn read_centroids(def: &VectorIndexDef, index_id: u32) -> Option<Vec<Vec<f32>>> {
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
        // The physical def is created lazily on the first upsert (see `mutation.rs`). A
        // Router-registered, activated index with no embeddings yet has no physical def, but it is a
        // known-empty index, not an unknown one — return an empty result rather than `UnknownIndex`.
        let Some(def) = VECTOR_INDEX_DEFS.with_borrow(|defs| defs.get(&req.index_id)) else {
            return Ok(VectorSearchResult { hits: Vec::new() });
        };
        // Slice 5/6 support only the degenerate F32 / L2Squared baseline; the request must also agree
        // with the stored definition.
        if req.encoding != VectorEncoding::F32
            || req.encoding != def.encoding
            || req.metric != VectorMetric::L2Squared
            || req.metric != def.metric
            || req.dims != def.dims
        {
            return Err(VectorIndexError::DimensionMismatch);
        }
        if req.query.len() != def.stride_bytes as usize {
            return Err(VectorIndexError::ByteWidthMismatch);
        }

        let query = decode_f32(&req.query);

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
        if def.nlist <= 1 || !centroids_ready(&def, req.index_id) {
            Ok(self.exact_subject_scan(req, def.active_index_version, &query))
        } else {
            Ok(self.partition_page_scan(req, &def, &query, tuning))
        }
    }

    /// Slice 5 exact scan: walk every live subject of the index and score its current slot. The live
    /// slot is resolved via `current_slot_for(active)` (ADR 0031 Slice 7) so a post-publish exact
    /// fallback reads the new active version (`shadow_slot`), never the stale old `entry.slot`.
    fn exact_subject_scan(
        &self,
        req: &VectorSearchRequest,
        active_index_version: u64,
        query: &[f32],
    ) -> VectorSearchResult {
        let mut cache: HashMap<PageKey, VectorPage> = HashMap::new();
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

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
                let Some(bytes) =
                    slot_bytes_cached(&mut cache, req.index_id, slot, value.vector_id)
                else {
                    continue;
                };
                let distance = l2_squared_f32(query, &decode_f32(&bytes));
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
            return self.exact_subject_scan(req, def.active_index_version, query);
        };
        let active = def.active_index_version;
        let selected = select_partitions(&centroids, query, tuning.nprobe);
        let mut heap: BinaryHeap<Candidate> = BinaryHeap::new();

        VECTOR_PAGE.with_borrow(|pages| {
            for partition_id in selected {
                let lower = PageKey::new(req.index_id, active, partition_id, 0);
                for page_entry in pages.range((Bound::Included(lower), Bound::Unbounded)) {
                    let page_key = page_entry.key();
                    if page_key.index_id != req.index_id
                        || page_key.index_version != active
                        || page_key.partition_id != partition_id
                    {
                        break; // partition-major order: past this partition's pages.
                    }
                    let page = page_entry.value();
                    for (slot_idx, row) in page.rows.iter().enumerate() {
                        if row.tombstoned {
                            continue;
                        }
                        let Some(candidate) = self.fresh_row_candidate(
                            req.index_id,
                            page_key,
                            slot_idx as u32,
                            row.vector_id,
                            row.generation,
                            query,
                            &row.bytes,
                        ) else {
                            continue;
                        };
                        push_bounded(&mut heap, req.top_k, candidate);
                    }
                }
            }
        });

        finalize(heap)
    }

    /// Re-validates a page row against the subject map and, if it is the subject's current live slot,
    /// returns a scored candidate. Returns `None` for any reverse-map miss, deleted/mismatched
    /// subject entry, or slot drift — the freshness contract shared with the exact scan.
    #[allow(clippy::too_many_arguments)]
    fn fresh_row_candidate(
        &self,
        index_id: u32,
        page_key: &PageKey,
        slot_idx: u32,
        vector_id: u64,
        generation: u64,
        query: &[f32],
        bytes: &[u8],
    ) -> Option<Candidate> {
        let subject = VECTOR_ID_TO_SUBJECT
            .with_borrow(|m| m.get(&VectorIdKey::new(index_id, vector_id)))?
            .0;
        let entry =
            VECTOR_SUBJECT_TO_ID.with_borrow(|m| m.get(&SubjectKey::new(index_id, subject)))?;
        if entry.deleted || entry.vector_id != Some(vector_id) {
            return None;
        }
        let expected = SlotRef {
            index_version: page_key.index_version,
            partition_id: page_key.partition_id,
            page_id: page_key.page_id,
            slot: slot_idx,
            generation,
        };
        // Pages are scanned at the active version, so the subject's live slot for that version
        // (active `slot`, or `shadow_slot` once an atomic publish flips active onto the rebuilt one)
        // must point at exactly this row (ADR 0031 Slice 7).
        if entry.current_slot_for(page_key.index_version) != Some(expected) {
            return None;
        }
        let distance = l2_squared_f32(query, &decode_f32(bytes));
        Some(Candidate {
            distance,
            subject,
            embedding_incarnation: entry.embedding_incarnation,
            embedding_version: entry.stored_embedding_version,
        })
    }
}
