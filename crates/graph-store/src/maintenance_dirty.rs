//! Persistent **dirty ordinal intervals** for incremental graph maintenance.
//!
//! Intervals are half-open `[start, end)` on the forward-layout ordinal axis. The map is keyed by
//! a packed `(start, end)` pair so each interval is one [`StableBTreeMap`] entry; overlapping
//! intervals are merged on insert via local rescans (no full-graph rebuild).

use ic_stable_structures::storable::Bound;
use ic_stable_structures::{Memory, StableBTreeMap, Storable};
use std::borrow::Cow;
use std::ops::Bound as OpsBound;

use crate::adjacency::{GraphAdjacencyMemory, GraphStoreMemorySlots, PageRangeMemory, RcGraphMemory};

/// Stable map: one row per disjoint dirty interval `[start, end)`.
pub type GraphMaintenanceDirtyOrdinalMap<M> = StableBTreeMap<
    MaintenanceDirtyPackedInterval,
    (),
    GraphAdjacencyMemory<PageRangeMemory<RcGraphMemory<M>>>,
>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaintenanceDirtyPackedInterval(pub u128);

impl MaintenanceDirtyPackedInterval {
    #[inline]
    pub fn pack(start: u64, end: u64) -> Self {
        Self((u128::from(start) << 64) | u128::from(end))
    }

    #[inline]
    pub fn unpack(self) -> (u64, u64) {
        let start = (self.0 >> 64) as u64;
        let end = (self.0 & u128::from(u64::MAX)) as u64;
        (start, end)
    }
}

impl Storable for MaintenanceDirtyPackedInterval {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u128::from_bytes(bytes))
    }

    const BOUND: Bound = u128::BOUND;
}

/// Opens the maintenance-dirty map on fixed slot [`crate::adjacency::GRAPH_STORE_MEMORY_ID_MAINTENANCE_DIRTY_ORDINALS`].
pub fn open_maintenance_dirty_ordinal_map<M: Memory + Clone>(
    slots: &GraphStoreMemorySlots<PageRangeMemory<RcGraphMemory<M>>>,
) -> GraphMaintenanceDirtyOrdinalMap<M> {
    StableBTreeMap::init(slots.maintenance_dirty_ordinals())
}

/// Merges `[start, end)` into the map, absorbing any overlapping stored intervals.
pub fn merge_dirty_ordinal_interval<M: Memory + Clone>(
    map: &mut GraphMaintenanceDirtyOrdinalMap<M>,
    mut start: u64,
    mut end: u64,
) {
    if start >= end {
        return;
    }
    loop {
        let upper = MaintenanceDirtyPackedInterval::pack(end, 0);
        let mut removed: Vec<MaintenanceDirtyPackedInterval> = Vec::new();
        for entry in map.range((OpsBound::Unbounded, OpsBound::Excluded(upper))) {
            let k = *entry.key();
            let (a, b) = k.unpack();
            if b > start && a < end {
                removed.push(k);
                start = start.min(a);
                end = end.max(b);
            }
        }
        if removed.is_empty() {
            break;
        }
        for k in removed {
            map.remove(&k);
        }
    }
    map.insert(MaintenanceDirtyPackedInterval::pack(start, end), ());
}

/// Pops the smallest interval by `(start, end)` lexicographic order.
pub fn pop_first_dirty_interval<M: Memory + Clone>(
    map: &mut GraphMaintenanceDirtyOrdinalMap<M>,
) -> Option<(u64, u64)> {
    let (k, ()) = map.pop_first()?;
    Some(k.unpack())
}

/// Smallest dirty interval by key order, without removing it (peek).
pub fn peek_first_dirty_interval<M: Memory + Clone>(
    map: &GraphMaintenanceDirtyOrdinalMap<M>,
) -> Option<(u64, u64)> {
    map.first_key_value().map(|(k, ())| k.unpack())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adjacency::RcGraphMemory;
    use ic_stable_structures::VectorMemory;
    use std::rc::Rc;

    #[test]
    fn merge_intervals_coalesce_overlapping() {
        let slots = GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::new(
            VectorMemory::default(),
        )));
        let mut map = open_maintenance_dirty_ordinal_map(&slots);
        merge_dirty_ordinal_interval(&mut map, 1, 4);
        merge_dirty_ordinal_interval(&mut map, 3, 6);
        assert_eq!(map.len(), 1);
        let (s, e) = map.first_key_value().expect("one").0.unpack();
        assert_eq!((s, e), (1, 6));
    }

    #[test]
    fn pop_first_is_ordered_by_start_then_end() {
        let slots = GraphStoreMemorySlots::for_root_memory(RcGraphMemory(Rc::new(
            VectorMemory::default(),
        )));
        let mut map = open_maintenance_dirty_ordinal_map(&slots);
        merge_dirty_ordinal_interval(&mut map, 5, 7);
        merge_dirty_ordinal_interval(&mut map, 1, 2);
        assert_eq!(pop_first_dirty_interval(&mut map), Some((1, 2)));
        assert_eq!(pop_first_dirty_interval(&mut map), Some((5, 7)));
        assert_eq!(pop_first_dirty_interval(&mut map), None);
    }
}
