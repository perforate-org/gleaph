//! Transient heap centroid cache for `ivf_flat` search (ADR 0031 Slice 9).
//!
//! The partition-page search path scores `query` against an index's `0..nlist` centroids on every
//! call. Reading them from `IVF_CENTROIDS` (a `StableBTreeMap`) and decoding `f32` bytes on each
//! query is pure, repeatable work; this module memoizes the decoded centroid set on the heap so a
//! warmed index skips the stable read + decode.
//!
//! **IC query semantics.** `vector_search` is a `#[query]` and IC query execution is non-committing:
//! heap mutations made during a query are discarded when the call returns. The cache therefore has a
//! strict read/write split:
//!
//! - [`lookup`] is the only path a query touches. It *reads* an already-warmed entry and never
//!   writes. A miss simply returns `None`; the caller then performs a one-call stable read (it does
//!   **not** populate the cache, since that write would be rolled back anyway).
//! - Population ([`VectorCanisterStore::admin_vector_centroid_cache_warmup`]) and eviction
//!   ([`VectorCanisterStore::admin_vector_centroid_cache_clear`], [`invalidate`]) happen only on
//!   `#[update]` paths, whose heap writes are committed.
//!
//! **Freshness.** Each entry is keyed by `(index_id -> {version, nlist, dims})`. The active centroid
//! set only changes at a rebuild publish, which increments `active_index_version`, so a stale entry
//! can never satisfy a [`lookup`] for the new version (the version field would differ). Publish also
//! calls [`invalidate`] to free the heap promptly. The cache is purely derived, so it is dropped on
//! init/upgrade.

use super::VectorCanisterStore;
use super::search::read_centroids_at;
use crate::facade::stable::definition_store;
use candid::Principal;
use gleaph_graph_kernel::vector_index::{VectorCanisterError, VectorCentroidCacheStatus};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem::size_of;

/// Heap byte budget for the centroid cache. Generous enough to hold a few `MAX_NLIST`-sized centroid
/// sets at typical dims, while still bounding worst-case heap growth across many warmed indexes.
const MAX_CENTROID_CACHE_BYTES: u64 = 8 * 1024 * 1024;

/// One warmed centroid set, with the generation key it was read at and its accounted heap bytes.
struct CachedCentroids {
    version: u64,
    nlist: u32,
    dims: u16,
    centroids: Vec<Vec<f32>>,
    bytes: u64,
}

/// Byte-bounded heap cache of decoded centroid sets, one entry per `index_id`.
#[derive(Default)]
struct CentroidCache {
    entries: BTreeMap<u32, CachedCentroids>,
    total_bytes: u64,
}

thread_local! {
    static CENTROID_CACHE: RefCell<CentroidCache> = RefCell::new(CentroidCache::default());
}

/// Accounted heap bytes of a decoded centroid set: the `f32` payloads plus the per-`Vec` headers.
fn centroid_bytes(centroids: &[Vec<f32>]) -> u64 {
    let payload: u64 = centroids
        .iter()
        .map(|c| (c.len() * size_of::<f32>()) as u64)
        .sum();
    payload + centroids.len() as u64 * size_of::<Vec<f32>>() as u64
}

/// Returns a warmed centroid set for `(index_id, version, nlist, dims)`, or `None` on miss. The only
/// path a `#[query]` touches; it never mutates the cache.
pub(super) fn lookup(index_id: u32, version: u64, nlist: u32, dims: u16) -> Option<Vec<Vec<f32>>> {
    CENTROID_CACHE.with_borrow(|cache| {
        let entry = cache.entries.get(&index_id)?;
        (entry.version == version && entry.nlist == nlist && entry.dims == dims)
            .then(|| entry.centroids.clone())
    })
}

/// Drops any cached entry for `index_id` (called on rebuild publish; safe if absent).
pub(super) fn invalidate(index_id: u32) {
    CENTROID_CACHE.with_borrow_mut(|cache| {
        if let Some(entry) = cache.entries.remove(&index_id) {
            cache.total_bytes -= entry.bytes;
        }
    });
}

/// Drops every cached entry (admin clear; also called on init/upgrade reset).
pub(super) fn clear_all() {
    CENTROID_CACHE.with_borrow_mut(|cache| {
        cache.entries.clear();
        cache.total_bytes = 0;
    });
}

/// Verifies that the coordinated reset can acquire the heap cache before any stable write.
#[cfg(any(test, feature = "canbench"))]
pub(super) fn preflight_clear() -> Result<(), ()> {
    CENTROID_CACHE.with(|cache| cache.try_borrow_mut().map(|_| ()).map_err(|_| ()))
}

/// Inserts (replacing any existing entry for `index_id`), evicting other entries lowest-`index_id`
/// first until the budget fits. A set larger than the whole budget is not cached. Returns whether
/// the entry is now resident.
fn insert(index_id: u32, cached: CachedCentroids) -> bool {
    CENTROID_CACHE.with_borrow_mut(|cache| {
        if let Some(old) = cache.entries.remove(&index_id) {
            cache.total_bytes -= old.bytes;
        }
        if cached.bytes > MAX_CENTROID_CACHE_BYTES {
            return false;
        }
        while cache.total_bytes + cached.bytes > MAX_CENTROID_CACHE_BYTES {
            // Evict another resident index (deterministic: lowest id first).
            let Some((&victim, _)) = cache.entries.iter().next() else {
                break;
            };
            let evicted = cache.entries.remove(&victim).expect("victim present");
            cache.total_bytes -= evicted.bytes;
        }
        cache.total_bytes += cached.bytes;
        cache.entries.insert(index_id, cached);
        true
    })
}

/// Current cache facts (entries / bytes / cap). Per-query hit/miss is intentionally not tracked
/// (queries cannot commit counters on IC).
fn status() -> VectorCentroidCacheStatus {
    CENTROID_CACHE.with_borrow(|cache| VectorCentroidCacheStatus {
        entries: cache.entries.len() as u64,
        bytes: cache.total_bytes,
        max_bytes: MAX_CENTROID_CACHE_BYTES,
    })
}

impl VectorCanisterStore {
    /// Warms the heap centroid cache for `index_id` from its active centroid set (ADR 0031 Slice 9).
    /// Router-guarded `#[update]` (a query cannot persist the warmed entry). Only indexes with a ready
    /// `nlist > 1` centroid set are cached; a degenerate (`nlist <= 1`) or untrained index instead
    /// drops any stale entry. Returns the post-warmup cache status.
    pub fn admin_vector_centroid_cache_warmup(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<VectorCentroidCacheStatus, VectorCanisterError> {
        self.assert_router_caller(caller)?;
        let def = definition_store::get(index_id)
            .map_err(super::legacy_definition_store_error)?
            .ok_or(VectorCanisterError::UnknownIndex)?;
        match read_centroids_at(index_id, def.active_index_version, def.nlist, def.dims) {
            Some(centroids) if def.nlist > 1 => {
                let bytes = centroid_bytes(&centroids);
                insert(
                    index_id,
                    CachedCentroids {
                        version: def.active_index_version,
                        nlist: def.nlist,
                        dims: def.dims,
                        centroids,
                        bytes,
                    },
                );
            }
            // No ready centroid set (or degenerate index): ensure no stale entry lingers.
            _ => invalidate(index_id),
        }
        Ok(status())
    }

    /// Clears the entire heap centroid cache (ADR 0031 Slice 9). Router-guarded `#[update]`.
    pub fn admin_vector_centroid_cache_clear(
        &self,
        caller: Principal,
    ) -> Result<VectorCentroidCacheStatus, VectorCanisterError> {
        self.assert_router_caller(caller)?;
        clear_all();
        Ok(status())
    }

    /// Reports the heap centroid cache status (ADR 0031 Slice 9). Router-guarded `#[query]`.
    pub fn admin_vector_centroid_cache_status(
        &self,
        caller: Principal,
    ) -> Result<VectorCentroidCacheStatus, VectorCanisterError> {
        self.assert_router_caller(caller)?;
        Ok(status())
    }
}
