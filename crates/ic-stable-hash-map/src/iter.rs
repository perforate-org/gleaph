//! Iterator over a [`StableHashMap`]'s entries, in slot order (unordered).

use crate::hash_map::StableHashMap;
use ic_stable_structures::{Memory, Storable};

/// Iterator over the occupied slots of a [`StableHashMap`], yielding `(key, value)`.
pub struct Iter<'a, K: Storable, V: Storable, M: Memory> {
    map: &'a StableHashMap<K, V, M>,
    next_idx: u64,
}

impl<'a, K: Storable + PartialEq, V: Storable, M: Memory> Iter<'a, K, V, M> {
    pub(crate) fn new(map: &'a StableHashMap<K, V, M>) -> Self {
        Self { map, next_idx: 0 }
    }
}

impl<'a, K: Storable + PartialEq, V: Storable, M: Memory> Iterator for Iter<'a, K, V, M> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        let capacity = self.map.capacity();
        while self.next_idx < capacity {
            let idx = self.next_idx;
            self.next_idx += 1;
            if self.map.is_occupied(idx) {
                return Some((self.map.read_key(idx), self.map.read_value(idx)));
            }
        }
        None
    }
}
