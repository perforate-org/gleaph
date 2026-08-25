//! Transient heap centroid cache for `ivf_flat` search and mutation assignment (ADR 0031 Slice 9).
//!
//! Scoring a query against an index's `0..nlist` centroids and assigning an upsert to its nearest
//! partition both need the decoded top-level centroid set of one generation — the leaf set of a
//! flat index, the level-0 coarse set of a two-level one. Reading it from `IVF_CENTROIDS` (a
//! `StableBTreeMap`) and decoding `f32` bytes on every call is pure, repeatable work; this module
//! keeps the decoded set resident on the heap as a shared handle so warmed consumers skip both the
//! stable read and the decode without ever duplicating payload bytes.
//!
//! **Shared payloads.** Each entry stores its decoded set behind an [`Arc`] ([`CentroidSet`]).
//! [`lookup`] hands out clones of that handle (a pointer bump), and consumers score against the
//! borrowed payloads — serving a cache hit copies no centroid bytes.
//!
//! **IC query semantics.** `vector_search` is a `#[query]` and IC query execution is non-committing:
//! heap mutations made during a query are discarded when the call returns. The cache therefore has a
//! strict read/write split:
//!
//! - [`lookup`] is the only path a query touches. It *reads* an already-resident entry and never
//!   writes. A miss simply returns `None`; the caller performs a one-call stable read for this call.
//! - Population happens only where heap writes commit: [`warm_all`] after the post-upgrade store
//!   reopen, and lazily on update-path reads ([`read_active`]).
//!
//! **Version scope.** Only reads at a definition's *active* generation touch the cache
//! ([`read_active`], [`warm_all`]). Reads at any other version — e.g. the Training/Building shadow
//! target that a resume/retry can still overwrite — bypass the cache via direct stable reads, so no
//! stale entry can survive a centroid rewrite. A rebuild publish flips `active_index_version` and
//! calls [`invalidate`]; independently, a stale entry could never satisfy a [`lookup`] for the new
//! version because the key's version field would differ.
//!
//! **Level scope (Slice 5).** For a two-level (`levels = 2`) generation only the **coarse**
//! set becomes resident; fine child sets are always stable-read per use and never cached.
//!
//! **Freshness & bounds.** Each entry is keyed by `(index_id -> {version, nlist, dims})`; a lookup
//! for any other shape misses. Entries are byte-bounded by [`MAX_CENTROID_CACHE_BYTES`] with
//! lowest-`index_id`-first eviction. The cache is purely derived, so it is dropped on init/upgrade
//! and rebuilt by [`warm_all`].

use super::authorization::assert_router_caller;
use super::search::read_centroids_at;
use crate::facade::stable::{IVF_CENTROID_META, definition_store};
use crate::records::VectorIndexDef;
use candid::Principal;
use gleaph_graph_kernel::vector_index::{VectorCanisterError, VectorCentroidCacheStatus};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

/// Heap byte budget for the centroid cache. Generous enough to hold a few `MAX_NLIST`-sized centroid
/// sets at typical dims, while still bounding worst-case heap growth across many warmed indexes.
const MAX_CENTROID_CACHE_BYTES: u64 = 8 * 1024 * 1024;

/// Shared handle to one decoded centroid set. Cloning copies the `Arc` pointer only; consumers score
/// against the borrowed payloads, so resident centroid bytes are never duplicated.
pub(super) type CentroidSet = Arc<Vec<Vec<f32>>>;

/// One warmed centroid set, with the generation key it was read at and its accounted heap bytes.
struct CachedCentroids {
    version: u64,
    nlist: u32,
    dims: u16,
    centroids: CentroidSet,
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

/// Returns a shared handle to the warmed centroid set for `(index_id, version, nlist, dims)`, or
/// `None` on miss. The only path a `#[query]` touches; it never mutates the cache and copies no
/// payload bytes (the handle clone is an `Arc` pointer bump).
pub(super) fn lookup(index_id: u32, version: u64, nlist: u32, dims: u16) -> Option<CentroidSet> {
    CENTROID_CACHE.with_borrow(|cache| {
        let entry = cache.entries.get(&index_id)?;
        (entry.version == version && entry.nlist == nlist && entry.dims == dims)
            .then(|| Arc::clone(&entry.centroids))
    })
}

/// Update-path read of the definition's **active** centroid set. A resident entry is served by
/// handle; otherwise the set is read from stable memory once, wrapped in a fresh [`Arc`], and left
/// resident for subsequent calls (the enclosing `#[update]` commits the heap write). Degenerate
/// (`nlist <= 1`) indexes never populate.
///
/// A two-level generation caches its **level-0 coarse set only** (the key's `nlist` field carries
/// the coarse count); the per-subtree fine child sets are always read straight from stable memory
/// — an extension of the version-scope rule below, since no query or mutation needs a full fine
/// set resident to assign one row. Returns `None` when no complete set exists at the active
/// generation.
pub(super) fn read_active(index_id: u32, def: &VectorIndexDef) -> Option<CentroidSet> {
    if def.nlist <= 1 {
        return None;
    }
    if let Some(set) = lookup(index_id, def.active_index_version, def.nlist, def.dims) {
        return Some(set);
    }
    let set = Arc::new(if def.is_two_level() {
        super::search::read_coarse_centroids_at(
            index_id,
            def.active_index_version,
            def.nlist,
            def.dims,
        )?
    } else {
        read_centroids_at(index_id, def.active_index_version, def.nlist, def.dims)?
    });
    insert(
        index_id,
        CachedCentroids {
            version: def.active_index_version,
            nlist: def.nlist,
            dims: def.dims,
            bytes: centroid_bytes(&set),
            centroids: Arc::clone(&set),
        },
    );
    Some(set)
}

/// Drops any cached entry for `index_id` (called on rebuild publish; safe if absent).
pub(super) fn invalidate(index_id: u32) {
    CENTROID_CACHE.with_borrow_mut(|cache| {
        if let Some(entry) = cache.entries.remove(&index_id) {
            cache.total_bytes -= entry.bytes;
        }
    });
}

/// Drops every cached entry (called on init/upgrade reset and coordinated definition-domain reset).
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

/// Best-effort population of one index's entry from its current active centroid set. Only a ready
/// `nlist > 1` set becomes resident; an unknown index, a degenerate generation, or an incomplete
/// set instead drops any stale entry. Never errors: an unavailable definition region behaves like
/// an unknown index. Returns the post-warm cache status.
pub(crate) fn warm_index(index_id: u32) -> VectorCentroidCacheStatus {
    let warmed = definition_store::get(index_id)
        .ok()
        .flatten()
        .filter(|def| def.nlist > 1)
        .and_then(|def| read_active(index_id, &def));
    if warmed.is_none() {
        invalidate(index_id);
    }
    status()
}

/// Restores the resident cache after a store reopen (post-upgrade): warms every index whose
/// centroid metadata marks a ready set. Internal — no caller guard — and bounded by
/// [`MAX_CENTROID_CACHE_BYTES`] with the standard eviction policy.
pub(crate) fn warm_all() {
    let ready: Vec<u32> = IVF_CENTROID_META.with_borrow(|meta| {
        meta.iter()
            .filter(|entry| entry.value().centroid_ready)
            .map(|entry| *entry.key())
            .collect()
    });
    for index_id in ready {
        warm_index(index_id);
    }
}

/// Reports the heap centroid cache status (ADR 0031 Slice 9). Router-guarded `#[query]`.
pub(crate) fn admin_vector_centroid_cache_status(
    caller: Principal,
) -> Result<VectorCentroidCacheStatus, VectorCanisterError> {
    assert_router_caller(caller)?;
    Ok(status())
}
