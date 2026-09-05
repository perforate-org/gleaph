//! Double-Array Trie implementation for fast dictionary lookup
//!
//! Copyright 2026 COOLJAPAN OU (Team KitaSan)
//! Vendored (plan 0333) from github.com/cool-japan/mecrab @ 85444b5
//! (mecrab/src/dict/double_array_trie.rs), MIT OR Apache-2.0.
//!
//! Plan 0333 refactor: the raw `*const Unit` pointer over the mmap slice is replaced by
//! `Arc<dyn ByteImage>` + a byte offset; every unit read is an 8-byte `read_exact_at`.
//! This is the first recorded contiguous-slice assumption removal point.

use crate::byteimage::ByteImage;
use crate::error::{Error, Result};
use byteorder::{ByteOrder, LittleEndian};
use std::sync::Arc;

/// Result from a trie lookup containing value and matched length
#[derive(Debug, Clone, Copy, Default)]
pub struct DartsResult {
    /// The value stored in the trie (-1 if not found)
    pub value: i32,
    /// Length of the matched key in bytes
    pub length: usize,
}

/// Double-Array Trie unit: 8 bytes (base i32 + check u32), Darts-compatible.
#[derive(Debug, Clone, Copy)]
struct Unit {
    base: i32,
    check: u32,
}

/// Double-Array Trie for fast word lookup (Darts compatible)
impl std::fmt::Debug for DoubleArrayTrie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DoubleArrayTrie")
            .field("offset", &self.offset)
            .field("size", &self.size)
            .finish()
    }
}

pub struct DoubleArrayTrie {
    /// Byte image (upstream: mmap-backed `&[u8]` pointer)
    image: Arc<dyn ByteImage>,
    /// Byte offset of the unit array inside the image
    offset: u64,
    /// Number of units
    size: usize,
}

impl DoubleArrayTrie {
    /// Unit size in bytes (same as C++ Darts)
    pub const UNIT_SIZE: usize = 8;

    /// Create a trie over `size_in_bytes` units starting at `offset` in `image`.
    pub fn from_image(
        image: Arc<dyn ByteImage>,
        offset: u64,
        size_in_bytes: usize,
    ) -> Result<Self> {
        if (offset as usize + size_in_bytes) as u64 > image.len() {
            return Err(Error::CorruptedDictionary(format!(
                "Double-array data too small: expected {} bytes at offset {}, image {} bytes",
                size_in_bytes,
                offset,
                image.len()
            )));
        }
        Ok(Self {
            image,
            offset,
            size: size_in_bytes / Self::UNIT_SIZE,
        })
    }

    /// Read one unit at `index` (contiguous fast path when the backing image is
    /// heap-resident; otherwise an 8-byte `read_exact_at`).
    #[inline]
    fn get(&self, index: usize) -> Option<Unit> {
        if index >= self.size {
            return None;
        }
        if let Some(slice) = self.image.as_contiguous() {
            let off = self.offset as usize + index * Self::UNIT_SIZE;
            Some(Unit {
                base: LittleEndian::read_i32(&slice[off..off + 4]),
                check: LittleEndian::read_u32(&slice[off + 4..off + 8]),
            })
        } else {
            let mut buf = [0u8; 8];
            self.image
                .read_exact_at(self.offset + (index * Self::UNIT_SIZE) as u64, &mut buf);
            Some(Unit {
                base: LittleEndian::read_i32(&buf),
                check: LittleEndian::read_u32(&buf[4..]),
            })
        }
    }

    /// Perform exact match search (compatible with Darts::exactMatchSearch)
    pub fn exact_match_search(&self, key: &[u8]) -> DartsResult {
        let mut result = DartsResult {
            value: -1,
            length: 0,
        };

        let mut b = match self.get(0) {
            Some(unit) => unit.base,
            None => return result,
        };

        for &byte in key.iter() {
            let p = (b as usize).wrapping_add(byte as usize).wrapping_add(1);
            match self.get(p) {
                Some(unit) if unit.check == b as u32 => {
                    b = unit.base;
                }
                _ => return result,
            }
        }

        let p = b as usize;
        if let Some(unit) = self.get(p)
            && unit.check == b as u32
            && unit.base < 0
        {
            result.value = -unit.base - 1;
            result.length = key.len();
        }

        result
    }

    /// Callback common-prefix search (the hot path): every prefix match is emitted to
    /// `sink` in key order. Avoids the fixed result-buffer zeroing per call (the
    /// upstream `[DartsResult; 512]` stack array costs an 8 KB memset per lookup).
    /// Returns the number of matches.
    pub fn for_each_result(&self, key: &[u8], mut sink: impl FnMut(DartsResult)) -> usize {
        let mut num_results = 0;

        let mut b = match self.get(0) {
            Some(unit) => unit.base,
            None => return 0,
        };

        for (i, &byte) in key.iter().enumerate() {
            let p = b as usize;
            if let Some(unit) = self.get(p)
                && unit.check == b as u32
                && unit.base < 0
            {
                sink(DartsResult {
                    value: -unit.base - 1,
                    length: i,
                });
                num_results += 1;
            }

            let p = (b as usize).wrapping_add(byte as usize).wrapping_add(1);
            match self.get(p) {
                Some(unit) if unit.check == b as u32 => {
                    b = unit.base;
                }
                _ => return num_results,
            }
        }

        let p = b as usize;
        if let Some(unit) = self.get(p)
            && unit.check == b as u32
            && unit.base < 0
        {
            sink(DartsResult {
                value: -unit.base - 1,
                length: key.len(),
            });
            num_results += 1;
        }

        num_results
    }

    /// Perform common prefix search (compatible with Darts::commonPrefixSearch)
    pub fn common_prefix_search(&self, key: &[u8], results: &mut [DartsResult]) -> usize {
        let max_results = results.len();
        let mut num_results = 0;

        let mut b = match self.get(0) {
            Some(unit) => unit.base,
            None => return 0,
        };

        for (i, &byte) in key.iter().enumerate() {
            let p = b as usize;
            if let Some(unit) = self.get(p)
                && unit.check == b as u32
                && unit.base < 0
                && num_results < max_results
            {
                results[num_results] = DartsResult {
                    value: -unit.base - 1,
                    length: i,
                };
                num_results += 1;
            }

            let p = (b as usize).wrapping_add(byte as usize).wrapping_add(1);
            match self.get(p) {
                Some(unit) if unit.check == b as u32 => {
                    b = unit.base;
                }
                _ => return num_results,
            }
        }

        let p = b as usize;
        if let Some(unit) = self.get(p)
            && unit.check == b as u32
            && unit.base < 0
            && num_results < max_results
        {
            results[num_results] = DartsResult {
                value: -unit.base - 1,
                length: key.len(),
            };
            num_results += 1;
        }

        num_results
    }

    /// Get the size of the trie in units
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the total size in bytes
    #[inline]
    pub fn total_size(&self) -> usize {
        self.size * Self::UNIT_SIZE
    }
}
