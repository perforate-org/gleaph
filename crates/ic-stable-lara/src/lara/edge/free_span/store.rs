//! Stable LARA BTree-backed free span store ordered by span length.
//!
//! This is the low-update-cost production candidate for free span reuse:
//! `(len, start_slot) -> ()`. It supports best-fit allocation without scanning
//! and coalesces immediately-adjacent free spans on release.
//!
//! A free span is a retired physical edge range. It becomes reusable only after
//! relocation has published the new edge slots, vertex rows, segment span
//! metadata, and counts. Free spans must not overlap any vertex-owned span
//! `[base_slot_start, base_slot_start + capacity)`, and clean scans must never
//! read this index.
//!
//! Allocation is best-fit by length. When a free span is larger than requested,
//! the allocated prefix is returned and the remainder is inserted back into the
//! index. If no suitable span exists, callers grow the edge slab instead.

use std::{
    borrow::Cow,
    cell::RefCell,
    ops::Bound::{Included, Unbounded},
};

use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound as StorableBound};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpan {
    pub start_slot: u64,
    pub len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FreeSpanKey(u128);

impl FreeSpanKey {
    #[inline]
    pub fn new(len: u64, start_slot: u64) -> Self {
        Self((u128::from(len) << 64) | u128::from(start_slot))
    }

    #[inline]
    pub fn len(self) -> u64 {
        (self.0 >> 64) as u64
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn start_slot(self) -> u64 {
        (self.0 & u128::from(u64::MAX)) as u64
    }

    #[inline]
    pub fn span(self) -> FreeSpan {
        FreeSpan {
            start_slot: self.start_slot(),
            len: self.len(),
        }
    }
}

impl Storable for FreeSpanKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u128::from_bytes(bytes))
    }

    const BOUND: StorableBound = u128::BOUND;
}

pub struct FreeSpanStore<M: Memory> {
    by_len: RefCell<StableBTreeMap<FreeSpanKey, (), M>>,
}

impl<M: Memory> FreeSpanStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            by_len: RefCell::new(StableBTreeMap::init(memory)),
        }
    }

    pub fn into_memory(self) -> M {
        self.by_len.into_inner().into_memory()
    }

    pub fn len(&self) -> u64 {
        self.by_len.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_len.borrow().is_empty()
    }

    pub fn spans(&self) -> Vec<FreeSpan> {
        self.by_len
            .borrow()
            .range((Unbounded::<FreeSpanKey>, Unbounded::<FreeSpanKey>))
            .map(|entry| entry.key().span())
            .collect()
    }

    pub fn release(&self, span: FreeSpan) {
        if span.len == 0 {
            return;
        }
        let mut merged = span;
        let mut merge_prev = None;
        let mut merge_next = None;

        for existing in self.spans() {
            let existing_end = existing.start_slot.saturating_add(existing.len);
            let span_end = span.start_slot.saturating_add(span.len);
            debug_assert!(
                existing_end <= span.start_slot || span_end <= existing.start_slot,
                "free span {span:?} overlaps existing span {existing:?}"
            );
            if existing_end == span.start_slot {
                merged.start_slot = existing.start_slot;
                merged.len = merged.len.saturating_add(existing.len);
                merge_prev = Some(existing);
            } else if span_end == existing.start_slot {
                merged.len = merged.len.saturating_add(existing.len);
                merge_next = Some(existing);
            }
        }

        if let Some(prev) = merge_prev {
            self.remove_exact(prev);
        }
        if let Some(next) = merge_next {
            self.remove_exact(next);
        }
        self.insert_raw(merged);
    }

    pub(crate) fn remove_exact(&self, span: FreeSpan) -> bool {
        self.by_len
            .borrow_mut()
            .remove(&FreeSpanKey::new(span.len, span.start_slot))
            .is_some()
    }

    pub(crate) fn free_span_ending_at(&self, end_slot: u64) -> Option<FreeSpan> {
        self.spans()
            .into_iter()
            .find(|span| span.start_slot.saturating_add(span.len) == end_slot)
    }

    pub(crate) fn free_span_starting_at(&self, start_slot: u64) -> Option<FreeSpan> {
        self.spans()
            .into_iter()
            .find(|span| span.start_slot == start_slot)
    }

    pub(crate) fn replace_exact_pair_with(
        &self,
        first: FreeSpan,
        second: FreeSpan,
        replacement: FreeSpan,
    ) {
        let removed_first = self.remove_exact(first);
        debug_assert!(removed_first);
        let removed_second = self.remove_exact(second);
        debug_assert!(removed_second);
        self.insert_raw(replacement);
    }

    fn insert_raw(&self, span: FreeSpan) {
        if span.len == 0 {
            return;
        }
        let previous = self
            .by_len
            .borrow_mut()
            .insert(FreeSpanKey::new(span.len, span.start_slot), ());
        debug_assert!(previous.is_none());
    }

    pub fn release_span(&self, start_slot: u64, len: u64) {
        self.release(FreeSpan { start_slot, len });
    }

    pub fn take_best_fit(&self, min_len: u64) -> Option<FreeSpan> {
        let span = self.take_best_fit_whole(min_len)?;
        let remaining = span.len - min_len;
        if remaining > 0 {
            self.release(FreeSpan {
                start_slot: span.start_slot + min_len,
                len: remaining,
            });
        }
        Some(FreeSpan {
            start_slot: span.start_slot,
            len: min_len,
        })
    }

    pub fn take_best_fit_whole(&self, min_len: u64) -> Option<FreeSpan> {
        let span = self.peek_best_fit(min_len)?;
        self.by_len
            .borrow_mut()
            .remove(&FreeSpanKey::new(span.len, span.start_slot));
        Some(span)
    }

    pub fn peek_best_fit(&self, min_len: u64) -> Option<FreeSpan> {
        if min_len == 0 {
            return None;
        }
        let lower = FreeSpanKey::new(min_len, 0);
        let by_len = self.by_len.borrow();
        let entry = by_len.range((Included(lower), Unbounded)).next()?;
        Some(entry.key().span())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    #[test]
    fn free_span_store_take_best_fit_splits_remainder() {
        let store = FreeSpanStore::init(vector_memory());
        store.release_span(1000, 80);
        store.release_span(2000, 32);
        store.release_span(3000, 128);

        assert_eq!(
            store.take_best_fit(40),
            Some(FreeSpan {
                start_slot: 1000,
                len: 40
            })
        );
        assert_eq!(
            store.take_best_fit_whole(40),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
    }

    #[test]
    fn free_span_store_release_coalesces_adjacent_spans() {
        let store = FreeSpanStore::init(vector_memory());
        store.release_span(100, 20);
        store.release_span(140, 10);
        store.release_span(120, 20);

        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 100,
                len: 50
            }]
        );
    }

    #[test]
    fn free_span_store_release_coalesces_one_sided_neighbors() {
        let store = FreeSpanStore::init(vector_memory());
        store.release_span(100, 20);
        store.release_span(120, 10);
        store.release_span(80, 20);

        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 80,
                len: 50
            }]
        );
    }

    #[test]
    fn free_span_store_take_best_fit_remainder_coalesces_with_neighbor() {
        let store = FreeSpanStore::init(vector_memory());
        store.insert_raw(FreeSpan {
            start_slot: 100,
            len: 20,
        });
        store.insert_raw(FreeSpan {
            start_slot: 120,
            len: 30,
        });

        assert_eq!(
            store.take_best_fit(10),
            Some(FreeSpan {
                start_slot: 100,
                len: 10
            })
        );
        assert_eq!(
            store.spans(),
            vec![FreeSpan {
                start_slot: 110,
                len: 40
            }]
        );
    }
}
