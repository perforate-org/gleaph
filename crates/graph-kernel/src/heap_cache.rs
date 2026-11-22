//! Bounded least-recently-used heap cache for derived plans.
//!
//! The Router and Graph canisters keep derived plans in heap memory: the Graph's decoded plan
//! bundles, the Router's sorted prepared variants and its per-caller policy-lowered plans. Heap
//! `thread_local!` state on the Internet Computer survives every message until the next upgrade,
//! so each of these caches needs a bound or it grows for the canister's lifetime. This type is the
//! one place that bound and its eviction rule live.
//!
//! The cache enforces two limits at once: an entry count and a total weight. The owner supplies the
//! weight function at construction; the plan caches weigh an entry by its encoded plan bytes, which
//! is what varies by orders of magnitude between plans. An insertion that would exceed either limit
//! evicts least-recently-used entries until it fits. An entry heavier than the whole weight limit
//! is not cached at all, so the cache never holds more than its limits.
//!
//! Eviction scans the entries for the oldest use. That is linear in the entry count, which the
//! limit keeps small, and it avoids storing a second copy of every key in a recency index (Graph
//! keys are whole plan blobs); only the evicted key is cloned. Entries live in a `HashMap`: the
//! Graph hit path runs once per seeded operation, and hashing a blob once is cheaper there than
//! the several full-blob comparisons of an ordered-map descent (`bench_plan_bundle_cache_hit_*`).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::num::NonZeroUsize;

/// The two limits a [`HeapLruCache`] enforces together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheLimits {
    max_entries: NonZeroUsize,
    max_weight: usize,
}

impl CacheLimits {
    pub const fn new(max_entries: NonZeroUsize, max_weight: usize) -> Self {
        Self {
            max_entries,
            max_weight,
        }
    }

    pub const fn max_entries(self) -> usize {
        self.max_entries.get()
    }

    pub const fn max_weight(self) -> usize {
        self.max_weight
    }
}

/// Outcome of [`HeapLruCache::insert`].
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insertion {
    /// The entry is cached; `evicted` older entries were removed to make room.
    Cached { evicted: usize },
    /// The entry alone outweighs the weight limit and was not cached.
    TooHeavy,
}

struct Slot<V> {
    value: V,
    weight: usize,
    last_use: u64,
}

/// A map bounded by entry count and total weight that evicts the least recently used entry.
pub struct HeapLruCache<K, V> {
    limits: CacheLimits,
    weigh: fn(&K, &V) -> usize,
    entries: HashMap<K, Slot<V>>,
    total_weight: usize,
    clock: u64,
}

impl<K: Eq + Hash + Clone, V> HeapLruCache<K, V> {
    pub fn new(limits: CacheLimits, weigh: fn(&K, &V) -> usize) -> Self {
        Self {
            limits,
            weigh,
            entries: HashMap::new(),
            total_weight: 0,
            clock: 0,
        }
    }

    pub fn limits(&self) -> CacheLimits {
        self.limits
    }

    /// Return the cached value and mark it most recently used.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let tick = self.tick();
        let slot = self.entries.get_mut(key)?;
        slot.last_use = tick;
        Some(&slot.value)
    }

    /// Cache `value` under `key`, replacing any previous value, after evicting least recently
    /// used entries until both limits would hold.
    ///
    /// The common call caches a key that is absent, so the key is hashed once, by the final
    /// `HashMap::insert`; eviction therefore assumes the key is new. When it replaces a present
    /// key, that can evict one entry more than strictly needed, never fewer, so the limits hold
    /// either way.
    pub fn insert(&mut self, key: K, value: V) -> Insertion {
        let weight = (self.weigh)(&key, &value);
        if weight > self.limits.max_weight {
            if let Some(previous) = self.entries.remove(&key) {
                self.total_weight -= previous.weight;
            }
            return Insertion::TooHeavy;
        }
        let mut evicted = 0;
        while self.entries.len() >= self.limits.max_entries.get()
            || self.total_weight + weight > self.limits.max_weight
        {
            if !self.evict_least_recently_used() {
                break;
            }
            evicted += 1;
        }
        let last_use = self.tick();
        self.total_weight += weight;
        let slot = Slot {
            value,
            weight,
            last_use,
        };
        if let Some(previous) = self.entries.insert(key, slot) {
            self.total_weight -= previous.weight;
        }
        Insertion::Cached { evicted }
    }

    /// Keep only the entries whose key satisfies `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        let mut removed_weight = 0;
        self.entries.retain(|key, slot| {
            let kept = keep(key);
            if !kept {
                removed_weight += slot.weight;
            }
            kept
        });
        self.total_weight -= removed_weight;
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.total_weight = 0;
    }

    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.entries.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn total_weight(&self) -> usize {
        self.total_weight
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Remove the least recently used entry; `false` when the cache is empty.
    fn evict_least_recently_used(&mut self) -> bool {
        let Some(oldest) = self
            .entries
            .iter()
            .min_by_key(|(_, slot)| slot.last_use)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        if let Some(removed) = self.entries.remove(&oldest) {
            self.total_weight -= removed.weight;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits(entries: usize, weight: usize) -> CacheLimits {
        CacheLimits::new(NonZeroUsize::new(entries).expect("nonzero"), weight)
    }

    #[expect(
        clippy::ptr_arg,
        reason = "the HeapLruCache weight function takes `&K` for a `Vec<u8>` key or value"
    )]
    fn by_value_len(_: &u32, value: &Vec<u8>) -> usize {
        value.len()
    }

    fn cache(entries: usize, weight: usize) -> HeapLruCache<u32, Vec<u8>> {
        HeapLruCache::new(limits(entries, weight), by_value_len)
    }

    #[test]
    fn entry_limit_evicts_the_least_recently_used() {
        let mut cache = cache(2, 100);
        assert_eq!(
            cache.insert(1, vec![0; 1]),
            Insertion::Cached { evicted: 0 }
        );
        assert_eq!(
            cache.insert(2, vec![0; 1]),
            Insertion::Cached { evicted: 0 }
        );
        assert!(cache.get(&1).is_some(), "touch 1 so 2 becomes the oldest");
        assert_eq!(
            cache.insert(3, vec![0; 1]),
            Insertion::Cached { evicted: 1 }
        );
        assert!(cache.contains_key(&1));
        assert!(!cache.contains_key(&2));
        assert!(cache.contains_key(&3));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn weight_limit_evicts_until_the_new_entry_fits() {
        let mut cache = cache(10, 10);
        assert!(matches!(
            cache.insert(1, vec![0; 4]),
            Insertion::Cached { .. }
        ));
        assert!(matches!(
            cache.insert(2, vec![0; 4]),
            Insertion::Cached { .. }
        ));
        assert_eq!(
            cache.insert(3, vec![0; 8]),
            Insertion::Cached { evicted: 2 }
        );
        assert_eq!(cache.keys().copied().collect::<Vec<_>>(), vec![3]);
        assert_eq!(cache.total_weight(), 8);
    }

    #[test]
    fn entry_heavier_than_the_limit_is_not_cached_and_drops_its_old_value() {
        let mut cache = cache(10, 10);
        assert!(matches!(
            cache.insert(1, vec![0; 4]),
            Insertion::Cached { .. }
        ));
        assert!(matches!(
            cache.insert(2, vec![0; 4]),
            Insertion::Cached { .. }
        ));
        assert_eq!(cache.insert(1, vec![0; 11]), Insertion::TooHeavy);
        assert!(
            !cache.contains_key(&1),
            "a stale value must not survive a rejected replace"
        );
        assert!(
            cache.contains_key(&2),
            "a rejected entry evicts nothing else"
        );
        assert_eq!(cache.total_weight(), 4);
    }

    #[test]
    fn replacing_a_key_reweighs_it() {
        let mut cache = cache(10, 10);
        assert!(matches!(
            cache.insert(1, vec![0; 6]),
            Insertion::Cached { .. }
        ));
        assert_eq!(
            cache.insert(1, vec![0; 2]),
            Insertion::Cached { evicted: 0 }
        );
        assert_eq!(cache.total_weight(), 2);
        assert_eq!(cache.get(&1).map(Vec::len), Some(2));
    }

    #[test]
    fn retain_and_clear_keep_the_weight_total_exact() {
        let mut cache = cache(10, 100);
        for key in 0..5 {
            assert!(matches!(
                cache.insert(key, vec![0; 3]),
                Insertion::Cached { .. }
            ));
        }
        cache.retain(|key| key % 2 == 0);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.total_weight(), 9);
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.total_weight(), 0);
    }

    #[test]
    fn borrowed_lookup_works_for_byte_keys() {
        let mut cache: HeapLruCache<Vec<u8>, u8> =
            HeapLruCache::new(limits(4, 64), |key: &Vec<u8>, _: &u8| key.len());
        assert!(matches!(
            cache.insert(b"plan".to_vec(), 7),
            Insertion::Cached { .. }
        ));
        assert_eq!(cache.get(b"plan".as_slice()), Some(&7));
    }
}
