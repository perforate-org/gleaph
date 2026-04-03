//! Local stable-storage traits for the rewrite.
//!
//! The rewrite keeps its own storage boundaries instead of depending directly on
//! `ic-stable-structures`. The traits in this module intentionally mirror only
//! the subset of behavior the rewrite currently needs.

use std::borrow::Cow;
use std::cell::RefCell;

/// States whether one encoded type is size-bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bound {
    Unbounded,
    Bounded { max_size: u32, is_fixed_size: bool },
}

/// Serialization boundary for stable-memory records.
pub trait Storable {
    fn to_bytes(&self) -> Cow<'_, [u8]>;
    fn into_bytes(self) -> Vec<u8>
    where
        Self: Sized;
    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self;
    const BOUND: Bound;
}

/// Minimal stable-memory abstraction used by the rewrite.
pub trait Memory {
    /// Returns current memory size in 64KiB wasm pages.
    fn size(&self) -> u64;

    /// Grows memory by `pages` 64KiB pages.
    ///
    /// Returns the old page count on success or `-1` on failure.
    fn grow(&self, pages: u64) -> i64;

    /// Reads bytes from one absolute offset.
    fn read(&self, offset: u64, buf: &mut [u8]);

    /// Writes bytes to one absolute offset.
    fn write(&self, offset: u64, src: &[u8]);
}

/// Simple in-memory `Memory` implementation for native tests and local runtime wiring.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VecMemory {
    bytes: RefCell<Vec<u8>>,
}

impl VecMemory {
    /// Creates one empty in-memory stable-memory image.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconstructs one in-memory stable-memory image from raw bytes.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self {
            bytes: RefCell::new(bytes),
        }
    }

    /// Returns a snapshot of the underlying bytes.
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes.borrow().clone()
    }
}

impl Memory for VecMemory {
    fn size(&self) -> u64 {
        let len = self.bytes.borrow().len() as u64;
        len.div_ceil(65_536)
    }

    fn grow(&self, pages: u64) -> i64 {
        let old = self.size();
        let Some(new_len) = old
            .checked_add(pages)
            .and_then(|pages| pages.checked_mul(65_536))
        else {
            return -1;
        };
        let Ok(new_len) = usize::try_from(new_len) else {
            return -1;
        };
        self.bytes.borrow_mut().resize(new_len, 0);
        old as i64
    }

    fn read(&self, offset: u64, buf: &mut [u8]) {
        let start = usize::try_from(offset).expect("offset should fit usize");
        let end = start + buf.len();
        buf.copy_from_slice(&self.bytes.borrow()[start..end]);
    }

    fn write(&self, offset: u64, src: &[u8]) {
        let start = usize::try_from(offset).expect("offset should fit usize");
        let end = start + src.len();
        let mut bytes = self.bytes.borrow_mut();
        if end > bytes.len() {
            bytes.resize(end, 0);
        }
        bytes[start..end].copy_from_slice(src);
    }
}
