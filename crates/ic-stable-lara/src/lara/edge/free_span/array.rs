//! Stable LARA array-backed free span store.
//!
//! Free spans are update/maintenance metadata. Clean query scans must not read
//! this store.

use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::fmt;

use super::FreeSpan;

pub const MAGIC: [u8; 3] = *b"LFS";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 32;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 12;
const ENTRY_SIZE: u64 = 16;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
    stride: u32,
}

#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    StrideMismatch { expected: u32, actual: u32 },
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(fmt, "bad free span magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => {
                write!(fmt, "unsupported free span layout version {version}")
            }
            Self::StrideMismatch { expected, actual } => {
                write!(
                    fmt,
                    "free span stride mismatch: expected {expected}, got {actual}"
                )
            }
            Self::OutOfMemory => write!(fmt, "failed to allocate free span metadata"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Debug)]
pub struct FreeSpanArrayStore<M: Memory> {
    memory: M,
}

impl<M: Memory> FreeSpanArrayStore<M> {
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: ENTRY_SIZE as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self { memory })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory);
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride != ENTRY_SIZE as u32 {
            return Err(InitError::StrideMismatch {
                expected: ENTRY_SIZE as u32,
                actual: header.stride,
            });
        }
        Ok(Self { memory })
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: u64) -> FreeSpan {
        assert!(index < self.len());
        Self::read_entry(&self.memory, index)
    }

    pub fn push(&self, item: FreeSpan) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len.checked_add(1).expect("free span length overflow");
        let mut bytes = [0u8; ENTRY_SIZE as usize];
        bytes[0..8].copy_from_slice(&item.start_slot.to_le_bytes());
        bytes[8..16].copy_from_slice(&item.len.to_le_bytes());
        safe_write(&self.memory, Self::entry_offset(len), &bytes)?;
        self.set_len(new_len);
        Ok(())
    }

    pub fn pop(&self) -> Option<FreeSpan> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let last = len - 1;
        let item = Self::read_entry(&self.memory, last);
        self.set_len(last);
        Some(item)
    }

    /// Removes and returns the free span with the smallest `len` such that `len >= min_len`.
    ///
    /// Tie-break: lowest index in the backing store (stable ordering). Returns [`None`] when
    /// `min_len == 0` or no span is large enough. Uses swap-with-last removal (O(1) writes).
    pub fn take_best_fit(&self, min_len: u64) -> Result<Option<FreeSpan>, GrowFailed> {
        if min_len == 0 {
            return Ok(None);
        }
        let n = self.len();
        let mut best_i: Option<u64> = None;
        let mut best_len: u64 = 0;
        let mut i = 0u64;
        while i < n {
            let s = Self::read_entry(&self.memory, i);
            if s.len >= min_len {
                let take = match best_i {
                    None => true,
                    Some(bi) => s.len < best_len || (s.len == best_len && i < bi),
                };
                if take {
                    best_i = Some(i);
                    best_len = s.len;
                }
            }
            i += 1;
        }
        let Some(idx) = best_i else {
            return Ok(None);
        };
        self.swap_remove(idx).map(Some)
    }

    /// Inserts `span`, coalescing immediately-adjacent spans by scanning the whole array.
    ///
    /// This is primarily useful as a simple baseline for benchmarks and small free lists.
    /// It does not maintain any ordering.
    pub fn release_coalescing_linear(&self, span: FreeSpan) -> Result<(), GrowFailed> {
        if span.len == 0 {
            return Ok(());
        }

        let mut merged = span;
        let mut prev_i = None;
        let mut next_i = None;
        let n = self.len();
        let mut i = 0u64;
        while i < n {
            let s = Self::read_entry(&self.memory, i);
            if s.start_slot.saturating_add(s.len) == span.start_slot {
                prev_i = Some(i);
                merged.start_slot = s.start_slot;
                merged.len = merged.len.saturating_add(s.len);
            } else if span.start_slot.saturating_add(span.len) == s.start_slot {
                next_i = Some(i);
                merged.len = merged.len.saturating_add(s.len);
            }
            i += 1;
        }

        match (prev_i, next_i) {
            (Some(a), Some(b)) if a > b => {
                self.swap_remove(a)?;
                self.swap_remove(b)?;
            }
            (Some(a), Some(b)) => {
                self.swap_remove(b)?;
                self.swap_remove(a)?;
            }
            (Some(i), None) | (None, Some(i)) => {
                self.swap_remove(i)?;
            }
            (None, None) => {}
        }

        self.push(merged)
    }

    fn swap_remove(&self, index: u64) -> Result<FreeSpan, GrowFailed> {
        let len = self.len();
        assert!(index < len);
        let last = len - 1;
        let removed = Self::read_entry(&self.memory, index);
        if index != last {
            let tail = Self::read_entry(&self.memory, last);
            Self::write_entry(&self.memory, index, tail)?;
        }
        self.set_len(last);
        Ok(removed)
    }

    fn write_entry(memory: &M, index: u64, item: FreeSpan) -> Result<(), GrowFailed> {
        let mut bytes = [0u8; ENTRY_SIZE as usize];
        bytes[0..8].copy_from_slice(&item.start_slot.to_le_bytes());
        bytes[8..16].copy_from_slice(&item.len.to_le_bytes());
        safe_write(memory, Self::entry_offset(index), &bytes)
    }

    fn set_len(&self, new_len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), new_len);
    }

    #[inline]
    fn entry_offset(index: u64) -> u64 {
        DATA_OFFSET + ENTRY_SIZE * index
    }

    fn read_entry(memory: &M, index: u64) -> FreeSpan {
        let offset = Self::entry_offset(index);
        FreeSpan {
            start_slot: read_u64(memory, Address::from(offset)),
            len: read_u64(memory, Address::from(offset + 8)),
        }
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version; 1]);
        write_u64(memory, Address::from(LEN_OFFSET), header.len);
        crate::write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        HeaderV1 {
            magic,
            version: version[0],
            len: read_u64(memory, Address::from(LEN_OFFSET)),
            stride: crate::read_u32(memory, Address::from(STRIDE_OFFSET)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::lara::edge::free_span::FreeSpan;
    use crate::test_support::vector_memory;

    #[test]
    fn free_span_array_store_take_best_fit_prefers_smallest_len() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory).unwrap();
        store
            .push(FreeSpan {
                start_slot: 0,
                len: 100,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 1000,
                len: 50,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 2000,
                len: 80,
            })
            .unwrap();
        let got = store.take_best_fit(45).unwrap().unwrap();
        assert_eq!(
            got,
            FreeSpan {
                start_slot: 1000,
                len: 50
            }
        );
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn free_span_array_store_release_coalescing_linear_merges_neighbors() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory).unwrap();
        store
            .push(FreeSpan {
                start_slot: 100,
                len: 20,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 140,
                len: 10,
            })
            .unwrap();

        store
            .release_coalescing_linear(FreeSpan {
                start_slot: 120,
                len: 20,
            })
            .unwrap();

        assert_eq!(store.len(), 1);
        assert_eq!(
            store.get(0),
            FreeSpan {
                start_slot: 100,
                len: 50,
            }
        );
    }

    #[test]
    fn free_span_array_store_reopens_and_pops_lifo() {
        let memory = vector_memory();
        let store = FreeSpanArrayStore::new(memory.clone()).unwrap();
        store
            .push(FreeSpan {
                start_slot: 16,
                len: 4,
            })
            .unwrap();
        store
            .push(FreeSpan {
                start_slot: 64,
                len: 12,
            })
            .unwrap();

        let reopened = FreeSpanArrayStore::init(memory).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(
            reopened.pop(),
            Some(FreeSpan {
                start_slot: 64,
                len: 12,
            })
        );
        assert_eq!(
            reopened.pop(),
            Some(FreeSpan {
                start_slot: 16,
                len: 4,
            })
        );
        assert_eq!(reopened.pop(), None);
    }
}
