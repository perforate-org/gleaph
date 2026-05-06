//! Stable LARA dual-index free span store.
//!
//! This is the coalescing BTree-backed alternative to [`super::FreeSpanStore`].
//! It keeps two indexes for the same logical free spans:
//!
//! - `by_len`: `(len, start_slot) -> ()` for best-fit allocation.
//! - `by_start`: `start_slot -> len` for neighbor lookup and coalescing.
//!
//! Clean query scans must not read this store.

use std::{
    borrow::Cow,
    fmt,
    ops::Bound::{Excluded, Included, Unbounded},
};

use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound as StorableBound};

use super::FreeSpan;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LenStartKey(u128);

impl LenStartKey {
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

impl Storable for LenStartKey {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StartKey(u64);

impl StartKey {
    #[inline]
    pub fn new(start_slot: u64) -> Self {
        Self(start_slot)
    }

    #[inline]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl Storable for StartKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u64::from_bytes(bytes))
    }

    const BOUND: StorableBound = u64::BOUND;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpanLen(u64);

impl SpanLen {
    #[inline]
    pub fn new(len: u64) -> Self {
        Self(len)
    }

    #[inline]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl Storable for SpanLen {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        self.0.to_bytes()
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.into_bytes()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(u64::from_bytes(bytes))
    }

    const BOUND: StorableBound = u64::BOUND;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeSpanDualIndexError {
    EmptySpan,
    DuplicateStart {
        start_slot: u64,
    },
    OverlapPrevious {
        previous: FreeSpan,
        inserted: FreeSpan,
    },
    OverlapNext {
        inserted: FreeSpan,
        next: FreeSpan,
    },
    MissingByStart {
        span: FreeSpan,
    },
    MissingByLen {
        span: FreeSpan,
    },
}

impl fmt::Display for FreeSpanDualIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySpan => write!(f, "free span length must be greater than zero"),
            Self::DuplicateStart { start_slot } => {
                write!(f, "free span already exists at start slot {start_slot}")
            }
            Self::OverlapPrevious { previous, inserted } => write!(
                f,
                "free span {inserted:?} overlaps previous span {previous:?}"
            ),
            Self::OverlapNext { inserted, next } => {
                write!(f, "free span {inserted:?} overlaps next span {next:?}")
            }
            Self::MissingByStart { span } => {
                write!(f, "free span {span:?} missing from by-start index")
            }
            Self::MissingByLen { span } => {
                write!(f, "free span {span:?} missing from by-len index")
            }
        }
    }
}

impl std::error::Error for FreeSpanDualIndexError {}

pub struct FreeSpanDualIndexStore<ML: Memory, MS: Memory> {
    by_len: StableBTreeMap<LenStartKey, (), ML>,
    by_start: StableBTreeMap<StartKey, SpanLen, MS>,
}

impl<ML: Memory, MS: Memory> FreeSpanDualIndexStore<ML, MS> {
    pub fn init(by_len_memory: ML, by_start_memory: MS) -> Self {
        Self {
            by_len: StableBTreeMap::init(by_len_memory),
            by_start: StableBTreeMap::init(by_start_memory),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_start.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.by_start.len()
    }

    pub fn contains_start(&self, start_slot: u64) -> bool {
        self.by_start.contains_key(&StartKey::new(start_slot))
    }

    pub fn get_by_start(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .get(&StartKey::new(start_slot))
            .map(|len| FreeSpan {
                start_slot,
                len: len.get(),
            })
    }

    pub fn release_span(
        &mut self,
        start_slot: u64,
        len: u64,
    ) -> Result<(), FreeSpanDualIndexError> {
        let span = FreeSpan { start_slot, len };
        self.release(span)
    }

    pub fn release(&mut self, span: FreeSpan) -> Result<(), FreeSpanDualIndexError> {
        if span.len == 0 {
            return Err(FreeSpanDualIndexError::EmptySpan);
        }
        if self.contains_start(span.start_slot) {
            return Err(FreeSpanDualIndexError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }

        let prev = self.prev_span(span.start_slot);
        let next = self.next_span(span.start_slot);
        let mut merged = span;
        let mut merge_prev = None;
        let mut merge_next = None;

        if let Some(prev) = prev {
            let prev_end = prev.start_slot.saturating_add(prev.len);
            if prev_end > span.start_slot {
                return Err(FreeSpanDualIndexError::OverlapPrevious {
                    previous: prev,
                    inserted: span,
                });
            }
            if prev_end == span.start_slot {
                merged.start_slot = prev.start_slot;
                merged.len = merged.len.saturating_add(prev.len);
                merge_prev = Some(prev);
            }
        }

        if let Some(next) = next {
            let span_end = span.start_slot.saturating_add(span.len);
            if span_end > next.start_slot {
                return Err(FreeSpanDualIndexError::OverlapNext {
                    inserted: span,
                    next,
                });
            }
            if span_end == next.start_slot {
                merged.len = merged.len.saturating_add(next.len);
                merge_next = Some(next);
            }
        }

        if let Some(prev) = merge_prev {
            self.remove_raw(prev)?;
        }
        if let Some(next) = merge_next {
            self.remove_raw(next)?;
        }
        self.insert_raw(merged)
    }

    pub fn take_best_fit(
        &mut self,
        min_len: u64,
    ) -> Result<Option<FreeSpan>, FreeSpanDualIndexError> {
        let Some(span) = self.take_best_fit_whole(min_len)? else {
            return Ok(None);
        };

        let remaining = span.len - min_len;
        if remaining > 0 {
            self.insert_raw(FreeSpan {
                start_slot: span.start_slot + min_len,
                len: remaining,
            })?;
        }

        Ok(Some(FreeSpan {
            start_slot: span.start_slot,
            len: min_len,
        }))
    }

    pub fn take_best_fit_whole(
        &mut self,
        min_len: u64,
    ) -> Result<Option<FreeSpan>, FreeSpanDualIndexError> {
        if min_len == 0 {
            return Ok(None);
        }

        let lower = LenStartKey::new(min_len, 0);
        let Some(entry) = self.by_len.range((Included(lower), Unbounded)).next() else {
            return Ok(None);
        };
        let span = entry.key().span();
        self.remove_raw(span)?;
        Ok(Some(span))
    }

    fn insert_raw(&mut self, span: FreeSpan) -> Result<(), FreeSpanDualIndexError> {
        if span.len == 0 {
            return Err(FreeSpanDualIndexError::EmptySpan);
        }
        if self.contains_start(span.start_slot) {
            return Err(FreeSpanDualIndexError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }
        if self
            .by_start
            .insert(StartKey::new(span.start_slot), SpanLen::new(span.len))
            .is_some()
        {
            return Err(FreeSpanDualIndexError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }
        let prev = self
            .by_len
            .insert(LenStartKey::new(span.len, span.start_slot), ());
        debug_assert!(prev.is_none());
        Ok(())
    }

    fn remove_raw(&mut self, span: FreeSpan) -> Result<(), FreeSpanDualIndexError> {
        match self.by_start.remove(&StartKey::new(span.start_slot)) {
            Some(len) if len.get() == span.len => {}
            _ => return Err(FreeSpanDualIndexError::MissingByStart { span }),
        }
        if self
            .by_len
            .remove(&LenStartKey::new(span.len, span.start_slot))
            .is_none()
        {
            return Err(FreeSpanDualIndexError::MissingByLen { span });
        }
        Ok(())
    }

    fn prev_span(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .range((Unbounded, Excluded(StartKey::new(start_slot))))
            .next_back()
            .map(|entry| FreeSpan {
                start_slot: entry.key().get(),
                len: entry.value().get(),
            })
    }

    fn next_span(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .range((Excluded(StartKey::new(start_slot)), Unbounded))
            .next()
            .map(|entry| FreeSpan {
                start_slot: entry.key().get(),
                len: entry.value().get(),
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lara::edge::free_span::FreeSpan;
    use crate::test_support::vector_memory;

    #[test]
    fn free_span_dual_index_release_coalesces_neighbors() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());

        store.release_span(100, 20).unwrap();
        store.release_span(140, 10).unwrap();
        store.release_span(120, 20).unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get_by_start(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 50
            })
        );
    }

    #[test]
    fn free_span_dual_index_take_best_fit_splits_remainder() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(1000, 80).unwrap();
        store.release_span(2000, 32).unwrap();
        store.release_span(3000, 128).unwrap();

        assert_eq!(
            store.take_best_fit(40).unwrap(),
            Some(FreeSpan {
                start_slot: 1000,
                len: 40
            })
        );
        assert_eq!(
            store.get_by_start(1040),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
        assert_eq!(store.get_by_start(1000), None);
    }

    #[test]
    fn free_span_dual_index_rejects_overlap() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(100, 20).unwrap();

        let err = store.release_span(110, 20).unwrap_err();
        assert!(matches!(
            err,
            FreeSpanDualIndexError::OverlapPrevious { .. }
                | FreeSpanDualIndexError::OverlapNext { .. }
        ));
    }

    #[test]
    fn free_span_dual_index_rejects_duplicate_without_mutation() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(100, 20).unwrap();

        let err = store.release_span(100, 8).unwrap_err();
        assert!(matches!(
            err,
            FreeSpanDualIndexError::DuplicateStart { start_slot: 100 }
        ));
        assert_eq!(
            store.get_by_start(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 20
            })
        );
    }

    #[test]
    fn free_span_dual_index_reopens_both_indexes() {
        let by_len = vector_memory();
        let by_start = vector_memory();
        let mut store = FreeSpanDualIndexStore::init(by_len.clone(), by_start.clone());
        store.release_span(64, 8).unwrap();
        store.release_span(128, 32).unwrap();
        drop(store);

        let mut reopened = FreeSpanDualIndexStore::init(by_len, by_start);
        assert_eq!(
            reopened.take_best_fit(16).unwrap(),
            Some(FreeSpan {
                start_slot: 128,
                len: 16
            })
        );
        assert_eq!(
            reopened.get_by_start(144),
            Some(FreeSpan {
                start_slot: 144,
                len: 16
            })
        );
    }

    #[test]
    fn free_span_dual_index_inserts_by_len_in_release_builds() {
        let mut store = FreeSpanDualIndexStore::init(vector_memory(), vector_memory());
        store.release_span(4096, 128).unwrap();

        assert_eq!(
            store.take_best_fit_whole(96).unwrap(),
            Some(FreeSpan {
                start_slot: 4096,
                len: 128
            })
        );
        assert!(store.is_empty());
    }
}
