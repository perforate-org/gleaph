//! Test/bench-only seeding of partitioned `ivf_flat` state (ADR 0031 Slice 6).
//!
//! Production cannot yet create `nlist > 1` indexes (centroid training / shadow rebuild is deferred
//! to Slice 7), so these helpers write a trained, partitioned layout directly: the def with
//! `nlist`, the centroids, ready centroid metadata, and one live slot per seeded vector assigned to
//! its nearest centroid partition, together with `VECTOR_SUBJECT_TO_ID`, `VECTOR_ID_TO_SLOT`, and
//! `VECTOR_ID_TO_SUBJECT`.
//!
//! **Seeded multi-partition indexes are immutable after seeding in Slice 6.** The production
//! mutation path still appends to `DEGENERATE_PARTITION_ID`, which is correct only while
//! `nlist == 1`. Mutating a seeded `nlist > 1` index would append fresh writes to partition 0 while
//! centroid selection routes elsewhere, hiding them for `nprobe < nlist`. Centroid-aware mutation
//! assignment is owned by Slice 7 (alongside the dual-write rebuild); tests/bench never mutate a
//! seeded partitioned index.

use super::search::l2_squared_f32;
use super::{
    DEFAULT_MAX_PAGE_BYTES, FIRST_ALLOCATION, INITIAL_INDEX_VERSION, PAGE_HEADER_BYTES,
    VectorIndexStore,
};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, VECTOR_ID_TO_SLOT, VECTOR_ID_TO_SUBJECT, VECTOR_INDEX_DEFS,
    VECTOR_SUBJECT_TO_ID,
};
use crate::records::{
    IvfCentroidMeta, PartitionKey, SubjectKey, SubjectMapEntry, VectorIdKey, VectorIndexDef,
    VectorSubjectRecord,
};
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
};

/// Encodes `f32` components as contiguous little-endian bytes (mirrors `search::decode_f32`).
fn encode_f32(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Index of the centroid nearest to `vector` (the assigned partition id).
fn nearest_partition(centroids: &[Vec<f32>], vector: &[f32]) -> u32 {
    let mut best = 0u32;
    let mut best_d = f32::INFINITY;
    for (p, centroid) in centroids.iter().enumerate() {
        let d = l2_squared_f32(centroid, vector);
        if d < best_d {
            best_d = d;
            best = p as u32;
        }
    }
    best
}

impl VectorIndexStore {
    /// Seeds a trained, partitioned `ivf_flat` index for tests and benchmarks.
    ///
    /// Writes the def (`nlist == centroids.len()`), the centroids, ready centroid metadata, and one
    /// live slot per vector assigned to its nearest centroid partition, plus the subject map and both
    /// reverse maps. The result is a read-only fixture (see module docs); callers must not mutate it
    /// through the production path afterwards.
    ///
    /// # Panics
    /// Panics if `centroids` is empty, if any centroid or vector length mismatches `dims`, or if the
    /// page-capacity computation rejects the stride.
    pub fn seed_ivf_for_test(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        centroids: &[Vec<f32>],
        vectors: &[(VectorSubject, Vec<f32>)],
    ) {
        assert!(!centroids.is_empty(), "seed requires at least one centroid");
        let nlist = centroids.len() as u32;
        let stride_bytes = encoding.stride_bytes(dims);
        assert!(stride_bytes > 0, "zero stride");
        let usable = DEFAULT_MAX_PAGE_BYTES.saturating_sub(PAGE_HEADER_BYTES);
        let slots_per_page = usable / stride_bytes;
        assert!(slots_per_page >= 1, "page capacity below one slot");
        for c in centroids {
            assert_eq!(c.len(), dims as usize, "centroid dims mismatch");
        }

        let active = INITIAL_INDEX_VERSION;
        let mut next_vector_id = FIRST_ALLOCATION;

        // Centroids + ready metadata.
        IVF_CENTROIDS.with_borrow_mut(|m| {
            for (p, centroid) in centroids.iter().enumerate() {
                m.insert(
                    PartitionKey::new(index_id, active, p as u32),
                    encode_f32(centroid),
                );
            }
        });
        IVF_CENTROID_META.with_borrow_mut(|meta| {
            meta.insert(
                index_id,
                IvfCentroidMeta {
                    centroid_ready: true,
                    centroid_epoch: 1,
                    trained_index_version: active,
                },
            )
        });

        // Live slots, assigned to the nearest centroid partition.
        for (subject, vector) in vectors {
            assert_eq!(vector.len(), dims as usize, "vector dims mismatch");
            let partition_id = nearest_partition(centroids, vector);
            let vector_id = next_vector_id;
            next_vector_id += 1;
            let slot = self.append_slot(
                index_id,
                active,
                partition_id,
                slots_per_page,
                vector_id,
                FIRST_ALLOCATION,
                encode_f32(vector),
            );
            let id_key = VectorIdKey::new(index_id, vector_id);
            VECTOR_ID_TO_SLOT.with_borrow_mut(|m| m.insert(id_key, slot));
            VECTOR_ID_TO_SUBJECT
                .with_borrow_mut(|m| m.insert(id_key, VectorSubjectRecord(*subject)));
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                m.insert(
                    SubjectKey::new(index_id, *subject),
                    SubjectMapEntry {
                        embedding_incarnation: 1,
                        stored_embedding_version: 1,
                        deleted: false,
                        slot: Some(slot),
                        shadow_slot: None,
                        vector_id: Some(vector_id),
                    },
                )
            });
        }

        // Persist the def last so its allocator reflects the seeded ids.
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist,
            active_index_version: active,
            stride_bytes,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page,
            next_vector_id,
        };
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
    }
}
