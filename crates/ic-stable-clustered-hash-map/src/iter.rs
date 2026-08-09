//! Iterator over a [`StableClusteredHashMap`]'s entries, in slot order (unordered).

use crate::map::StableClusteredHashMap;
use ic_stable_structures::{Memory, Storable};

/// Iterator over the occupied slots of a [`StableClusteredHashMap`], yielding `(key, value)`.
pub struct Iter<'a, K: Storable, V: Storable, M: Memory> {
    map: &'a StableClusteredHashMap<K, V, M>,
    next_idx: u64,
}

impl<'a, K: Storable + PartialEq, V: Storable, M: Memory> Iter<'a, K, V, M> {
    pub(crate) fn new(map: &'a StableClusteredHashMap<K, V, M>) -> Self {
        Self { map, next_idx: 0 }
    }

    /// Resumes iteration from `slot` (inclusive) in slot order. Used to continue a bounded scan
    /// across steps. The caller must ensure `slot` is a valid slot index for the map's current
    /// capacity; a stale slot after a resize is handled by the caller restarting the scan.
    pub(crate) fn from_slot(map: &'a StableClusteredHashMap<K, V, M>, slot: u64) -> Self {
        Self {
            map,
            next_idx: slot,
        }
    }

    /// The slot index of the next entry to be examined (one past the last yielded slot). Used to
    /// persist a resumable scan cursor across steps.
    pub fn position(&self) -> u64 {
        self.next_idx
    }
}

impl<'a, K: Storable + PartialEq, V: Storable, M: Memory> Iterator for Iter<'a, K, V, M> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        let capacity = self.map.capacity();
        while self.next_idx < capacity {
            let idx = self.next_idx;
            self.next_idx += 1;
            if !self.map.is_empty_slot(idx) {
                return Some((self.map.read_key(idx), self.map.read_value(idx)));
            }
        }
        None
    }
}
