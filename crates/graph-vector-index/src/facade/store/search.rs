//! Read-only exact `ivf_flat` top-k search over the live subject map (ADR 0031 Slice 5).
//!
//! Slice 5 scans `VECTOR_SUBJECT_TO_ID` — the source of truth for which subjects are live and at
//! which slot — rather than the page chain. Each non-deleted [`SubjectMapEntry`] already carries the
//! subject (its key), the current `slot`, and the `(embedding_incarnation, stored_embedding_version)`
//! clock, so tombstoned rows and superseded generations are never scored and result freshness is
//! exact. Page traversal / centroid pruning / `nprobe` are deferred to Slice 6 IVF partitioning.
//!
//! [`SubjectMapEntry`]: crate::records::SubjectMapEntry

use super::VectorIndexStore;
use crate::facade::stable::{VECTOR_INDEX_DEFS, VECTOR_PAGE, VECTOR_SUBJECT_TO_ID};
use crate::records::{PageKey, SlotRef, SubjectKey, VectorPage};
use gleaph_graph_kernel::vector_index::{
    MAX_VECTOR_SEARCH_TOP_K, VectorEncoding, VectorIndexError, VectorMetric, VectorSearchHit,
    VectorSearchRequest, VectorSearchResult, VectorSubject,
};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Bound;

/// Squared Euclidean distance between two equal-length `f32` vectors. Isolated so a SIMD variant can
/// replace the inner loop later without changing search semantics (ADR 0031 Slice 5).
fn l2_squared_f32(query: &[f32], vector: &[f32]) -> f32 {
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
fn decode_f32(bytes: &[u8]) -> Vec<f32> {
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

impl VectorIndexStore {
    /// Exact top-k vector search over the degenerate `ivf_flat` index (ADR 0031 Slice 5).
    ///
    /// Read-only: validates the request against the stored definition, then scans live subjects of
    /// `index_id`, scoring each against `query` and returning the `top_k` nearest ordered by
    /// `(distance ascending, subject ascending)`.
    pub fn vector_search(
        &self,
        req: &VectorSearchRequest,
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
        // Slice 5 supports only the degenerate F32 / L2Squared baseline; the request must also agree
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
                let Some(slot) = value.slot else {
                    continue;
                };
                let Some(bytes) =
                    slot_bytes_cached(&mut cache, req.index_id, slot, value.vector_id)
                else {
                    continue;
                };
                let distance = l2_squared_f32(&query, &decode_f32(&bytes));
                heap.push(Candidate {
                    distance,
                    subject: key.subject,
                    embedding_incarnation: value.embedding_incarnation,
                    embedding_version: value.stored_embedding_version,
                });
                if heap.len() as u32 > req.top_k {
                    heap.pop();
                }
            }
        });

        // `into_sorted_vec` yields ascending `(distance, subject)` order — the result contract.
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
        Ok(VectorSearchResult { hits })
    }
}
