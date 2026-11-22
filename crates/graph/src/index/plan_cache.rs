//! Bounded heap cache for decoded wire plan bundles.
//!
//! Router-issued plan blobs are repeated many times during bulk seeding (the same GQL template is
//! executed with different parameters). Decoding them on every operation burns a large,
//! predictable amount of instructions, so decoded bundles stay in canister heap memory.
//!
//! Heap `thread_local!` state survives every message until the next upgrade, and each distinct
//! ad-hoc query or policy-lowered plan arrives as a distinct blob, so the cache is bounded by
//! [`PLAN_BUNDLE_CACHE_LIMITS`] and evicts the least recently used bundle
//! ([`gleaph_graph_kernel::heap_cache`]). It never survives an upgrade, and the worst failure mode
//! is a cache miss.
//!
//! Keying by the full blob bytes is exact and collision-free; the entry weight is the blob length.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::rc::Rc;

use gleaph_gql_planner::{
    PhysicalPlan,
    wire::{PlanBundleError, decode_plan_bundle},
};
use gleaph_graph_kernel::heap_cache::{CacheLimits, HeapLruCache};

/// Bounds of the decoded plan-bundle cache: 64 bundles and 4 MiB of blob bytes.
pub(crate) const PLAN_BUNDLE_CACHE_LIMITS: CacheLimits =
    CacheLimits::new(NonZeroUsize::new(64).unwrap(), 4 * 1024 * 1024);

#[expect(
    clippy::ptr_arg,
    reason = "the HeapLruCache weight function takes `&K` for a `Vec<u8>` key or value"
)]
fn blob_weight(blob: &Vec<u8>, _: &CachedBundle) -> usize {
    blob.len()
}

thread_local! {
    static CACHE: RefCell<HeapLruCache<Vec<u8>, CachedBundle>> =
        RefCell::new(HeapLruCache::new(PLAN_BUNDLE_CACHE_LIMITS, blob_weight));
}

// Thread-local instruction-log counters used to report cache hit/miss counts
// under the `batch-instr-log` feature. Lives here so the cache can stay a
// pure `index` module without depending on `canister::instr_log`.
thread_local! {
    static HIT_COUNT: RefCell<u64> = const { RefCell::new(0) };
    static MISS_COUNT: RefCell<u64> = const { RefCell::new(0) };
}

#[derive(Clone)]
pub(crate) struct CachedBundle {
    pub requires_write_path: bool,
    pub plans: Rc<[PhysicalPlan]>,
}

/// Decode a plan bundle, returning the cached result when the same blob is still cached.
pub(crate) fn decode_plan_bundle_cached(bytes: &[u8]) -> Result<CachedBundle, PlanBundleError> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.get(bytes) {
            #[cfg(feature = "batch-instr-log")]
            HIT_COUNT.with(|c| *c.borrow_mut() += 1);
            return Ok(cached.clone());
        }
        #[cfg(feature = "batch-instr-log")]
        MISS_COUNT.with(|c| *c.borrow_mut() += 1);
        let (requires_write_path, plans) = decode_plan_bundle(bytes)?;
        let plans: Rc<[PhysicalPlan]> = plans.into();
        let cached = CachedBundle {
            requires_write_path,
            plans: plans.clone(),
        };
        let _ = cache.insert(bytes.to_vec(), cached.clone());
        Ok(cached)
    })
}

/// Clear the cache. Useful in tests where deterministic per-call behavior is
/// desired, or after large bulk operations to avoid pinning heap across
/// unrelated calls.
pub(crate) fn clear_plan_bundle_cache() {
    CACHE.with(|cache| cache.borrow_mut().clear());
}

/// Number of distinct bundles currently cached.
pub(crate) fn plan_bundle_cache_len() -> usize {
    CACHE.with(|cache| cache.borrow().len())
}

/// Reset and return hit/miss counters.
#[cfg(feature = "batch-instr-log")]
pub(crate) fn take_hit_miss_counts() -> (u64, u64) {
    HIT_COUNT.with(|h| {
        let mut h = h.borrow_mut();
        let hits = *h;
        *h = 0;
        MISS_COUNT.with(|m| {
            let mut m = m.borrow_mut();
            let misses = *m;
            *m = 0;
            (hits, misses)
        })
    })
}

/// Reset and return hit/miss counters (no-op outside `batch-instr-log`).
#[cfg(not(feature = "batch-instr-log"))]
pub(crate) fn take_hit_miss_counts() -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_planner::wire::encode_statement_plans;

    /// A distinct valid blob per `count`: a bundle of `count` empty plans.
    fn blob(count: usize) -> Vec<u8> {
        encode_statement_plans(&vec![PhysicalPlan::default(); count], false).expect("encode")
    }

    #[test]
    fn distinct_blobs_stay_within_the_entry_limit_and_keep_the_hot_bundle() {
        clear_plan_bundle_cache();
        let hot = blob(0);
        decode_plan_bundle_cached(&hot).expect("decode hot");
        for count in 1..=PLAN_BUNDLE_CACHE_LIMITS.max_entries() + 8 {
            decode_plan_bundle_cached(&blob(count)).expect("decode");
            decode_plan_bundle_cached(&hot).expect("touch hot");
        }
        assert_eq!(
            plan_bundle_cache_len(),
            PLAN_BUNDLE_CACHE_LIMITS.max_entries()
        );
        assert!(
            CACHE.with(|cache| cache.borrow().contains_key(hot.as_slice())),
            "a bundle used on every operation must survive eviction"
        );
        clear_plan_bundle_cache();
    }
}
