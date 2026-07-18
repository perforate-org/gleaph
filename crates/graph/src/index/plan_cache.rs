//! Ephemeral, in-memory cache for decoded wire plan bundles.
//!
//! Router-issued plan blobs are repeated many times during bulk seeding (the
//! same GQL template is executed with different parameters). Decoding them on
//! every operation burns a large, predictable amount of instructions. This cache
//! keeps decoded plans in canister heap memory for the current execution
//! boundary. It is intentionally ephemeral: it never survives canister upgrades,
//! message boundaries are isolated by thread-local storage, and the worst failure
//! mode is a cache miss (the same behavior as before).
//!
//! Keying by the full blob bytes is exact and collision-free. Hashing the bytes
//! before comparison would add cost for marginal benefit; small bundles dominate
//! here, so the key is stored as `Vec<u8>`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gleaph_gql_planner::{
    PhysicalPlan,
    wire::{PlanBundleError, decode_plan_bundle},
};

thread_local! {
    static CACHE: RefCell<HashMap<Vec<u8>, CachedBundle>> = RefCell::new(HashMap::new());
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

/// Decode a plan bundle, returning a cached result when the same blob has been
/// decoded already in the current message.
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
        cache.insert(bytes.to_vec(), cached.clone());
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) fn take_hit_miss_counts() -> (u64, u64) {
    (0, 0)
}
