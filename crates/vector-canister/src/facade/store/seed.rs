//! Test/bench-only seeding of partitioned `ivf_flat` state (ADR 0031 Slice 6).
//!
//! Production cannot yet create `nlist > 1` indexes (centroid training / shadow rebuild is deferred
//! to Slice 7), so these helpers write a trained, partitioned layout directly: the def with
//! `nlist`, the centroids, ready centroid metadata, and one live slot per seeded vector assigned to
//! its nearest centroid partition, together with `VECTOR_SUBJECT_TO_ID`.
//!
//! **Seeded multi-partition indexes are immutable after seeding in Slice 6.** The production
//! mutation path still appends to `DEGENERATE_PARTITION_ID`, which is correct only while
//! `nlist == 1`. Mutating a seeded `nlist > 1` index would append fresh writes to partition 0 while
//! centroid selection routes elsewhere, hiding them for `nprobe < nlist`. Centroid-aware mutation
//! assignment is owned by Slice 7 (alongside the dual-write rebuild); tests/bench never mutate a
//! seeded partitioned index.

use super::search::{encode_f32, l2_squared_f32};
use super::{DEFAULT_MAX_PAGE_BYTES, INITIAL_INDEX_VERSION, VectorCanisterStore};
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, VECTOR_INDEX_DEFS, VECTOR_SUBJECT_TO_ID,
};
use crate::records::{IvfCentroidMeta, PartitionKey, SubjectKey, SubjectMapEntry, VectorIndexDef};
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
};
use ic_stable_vector_page_store::PageLayout;

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

impl VectorCanisterStore {
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
        self.seed_ivf_with_metric_for_test(
            index_id,
            encoding,
            dims,
            VectorMetric::L2Squared,
            centroids,
            vectors,
        );
    }

    /// Seeded variant that pins a specific metric on the def (e.g. `Cosine` tests).
    pub fn seed_ivf_with_metric_for_test(
        &self,
        index_id: u32,
        encoding: VectorEncoding,
        dims: u16,
        metric: VectorMetric,
        centroids: &[Vec<f32>],
        vectors: &[(VectorSubject, Vec<f32>)],
    ) {
        assert!(!centroids.is_empty(), "seed requires at least one centroid");
        let nlist = centroids.len() as u32;
        let stride_bytes = encoding.stride_bytes(dims);
        assert!(stride_bytes > 0, "zero stride");
        // Seeded fixtures are F32 on a single shard: pad stride = ceil(dims/4)*16, meta 4, one run.
        let pad_stride_bytes = u32::from(dims).div_ceil(4) * 16;
        let meta_stride_bytes = 4u32;
        let run_capacity = 1u32;
        let slots_per_page = PageLayout::max_capacity_for(
            DEFAULT_MAX_PAGE_BYTES as usize,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
        )
        .expect("seed page capacity below one slot");
        for c in centroids {
            assert_eq!(c.len(), dims as usize, "centroid dims mismatch");
        }

        let active = INITIAL_INDEX_VERSION;

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

        // The def is persisted last, but the slab `append_row` needs `slots_per_page`/`stride_bytes`,
        // so build it up front and reuse it.
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric,
            nlist,
            active_index_version: active,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes,
            run_capacity,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            slots_per_page,
        };

        // Live slots, assigned to the nearest centroid partition.
        for (subject, vector) in vectors {
            assert_eq!(vector.len(), dims as usize, "vector dims mismatch");
            let partition_id = nearest_partition(centroids, vector);
            let slot = self
                .append_slot(
                    index_id,
                    active,
                    partition_id,
                    &def,
                    *subject,
                    &encode_f32(vector),
                )
                .expect("seed append");
            VECTOR_SUBJECT_TO_ID.with_borrow_mut(|m| {
                m.insert(
                    SubjectKey::new(index_id, *subject),
                    SubjectMapEntry {
                        stamp: 1,
                        deleted: false,
                        slot: Some(slot),
                        shadow_slot: None,
                    },
                )
            });
        }

        // Persist the def last.
        VECTOR_INDEX_DEFS.with_borrow_mut(|defs| defs.insert(index_id, def));
    }
}
