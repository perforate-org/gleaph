//! Stable LARA free span index ordered by span length.
//!
//! This is the low-update-cost production candidate for free span reuse:
//! `(len, start_slot) -> ()`. It supports best-fit allocation without scanning,
//! but intentionally does not coalesce on release.

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
