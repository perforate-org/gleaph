//! Framing-of-Reference ("for") fixed-block postings.
//!
//! Docids are split into blocks of [`FOR_BLOCK_SIZE`]; each block stores its minimum
//! (`base`) plus every value minus that base packed at the block's bit width
//! `bits(max - base)`. Byte layout:
//!
//! ```text
//! count:       u32 LE
//! block_count: u32 LE                       // B = ceil(count / 128)
//! descriptors: B × (base: u32 LE, width: u8)
//! offsets:     (B + 1) × u32 LE             // payload byte offsets, offsets[0] == 0,
//!                                           // absolute from the end of the offsets array
//! payload:     per block, values LSB-first bit-packed into whole bytes (zero padding)
//! ```
//!
//! Per-block byte alignment gives O(1) block addressing: block b's payload starts at
//! `offsets[b]`. A width of 0 encodes a constant block and occupies zero payload bytes.
//!
//! Corruption policy: buffers are produced only by [`encode_frame_of_reference`]; any
//! truncated or malformed input panics with a `corrupt postings:` message at the first
//! offending read. `advance(target)` binary-searches the block base table, then scans.

use super::PostingReader;
use super::bitio::{BitReader, BitWriter};
use super::varint::assert_strictly_increasing;

/// Postings per physical block; also the logical block size used by `blockmax`.
pub const FOR_BLOCK_SIZE: usize = 128;

/// Encodes strictly increasing, non-empty docids as FOR fixed blocks.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing.
pub fn encode_frame_of_reference(docs: &[u32]) -> Vec<u8> {
    assert_strictly_increasing(docs);
    let block_count = docs.len().div_ceil(FOR_BLOCK_SIZE);
    let mut descriptors = Vec::with_capacity(block_count * 5);
    let mut offsets: Vec<u32> = Vec::with_capacity(block_count + 1);
    let mut payload = Vec::new();
    for block in docs.chunks(FOR_BLOCK_SIZE) {
        let base = block[0];
        let bits = block_bits(block);
        descriptors.extend_from_slice(&base.to_le_bytes());
        descriptors.push(bits);
        offsets.push(payload.len() as u32);
        let mut writer = BitWriter::new();
        for &value in block {
            writer.push(value - base, bits);
        }
        payload.extend(writer.finish());
    }
    offsets.push(payload.len() as u32);
    let mut out = Vec::with_capacity(8 + descriptors.len() + offsets.len() * 4 + payload.len());
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    out.extend_from_slice(&(block_count as u32).to_le_bytes());
    out.extend_from_slice(&descriptors);
    for offset in &offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&payload);
    out
}

fn block_bits(block: &[u32]) -> u8 {
    let span = block[block.len() - 1] - block[0];
    if span == 0 {
        0
    } else {
        32 - span.leading_zeros() as u8
    }
}

/// Cursor over a FOR-encoded posting list. `pos` is always the absolute index of the next
/// unconsumed posting, so seeks reposition by absolute index without incremental accounting.
pub struct ForReader<'a> {
    data: &'a [u8],
    header_end: usize,
    count: u32,
    bases: Vec<u32>,
    widths: Vec<u8>,
    offsets: Vec<u32>,
    pos: u32,
    cached_block: u32,
    cache: Vec<u32>,
}

impl<'a> ForReader<'a> {
    /// Parses and validates the descriptor tables eagerly; block payloads decode lazily.
    ///
    /// # Panics
    /// Panics when the header, descriptors, or offset table are truncated or inconsistent.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 8 {
            panic!("corrupt postings: for header truncated");
        }
        let count = read_u32(data, 0);
        let block_count = read_u32(data, 4);
        if block_count == 0 || (block_count as usize - 1) * FOR_BLOCK_SIZE >= count as usize {
            panic!("corrupt postings: for block count does not cover count");
        }
        let desc_len = block_count as usize * 5;
        let offs_start = 8 + desc_len;
        let offs_len = (block_count as usize + 1) * 4;
        let header_end = offs_start + offs_len;
        if data.len() < header_end {
            panic!("corrupt postings: for header truncated");
        }
        let mut bases = Vec::with_capacity(block_count as usize);
        let mut widths = Vec::with_capacity(block_count as usize);
        for b in 0..block_count as usize {
            bases.push(read_u32(data, 8 + b * 5));
            widths.push(data[8 + b * 5 + 4]);
        }
        let mut offsets = Vec::with_capacity(block_count as usize + 1);
        for b in 0..=block_count as usize {
            offsets.push(read_u32(data, offs_start + b * 4));
        }
        Self {
            data,
            header_end,
            count,
            bases,
            widths,
            offsets,
            pos: 0,
            cached_block: u32::MAX,
            cache: Vec::new(),
        }
    }

    fn ensure_block(&mut self, block: u32) {
        if self.cached_block == block {
            return;
        }
        let b = block as usize;
        let start = self.header_end + self.offsets[b] as usize;
        let end = self.header_end + self.offsets[b + 1] as usize;
        let slice = self
            .data
            .get(start..end)
            .unwrap_or_else(|| panic!("corrupt postings: for block payload truncated"));
        let elems = if b == self.bases.len() - 1 {
            self.count - (self.bases.len() as u32 - 1) * FOR_BLOCK_SIZE as u32
        } else {
            FOR_BLOCK_SIZE as u32
        };
        let mut reader = BitReader::new(slice);
        self.cache.clear();
        self.cache.reserve(elems as usize);
        let base = self.bases[b];
        for _ in 0..elems {
            // Stored values are relative to the block base; restore absolute docids.
            self.cache
                .push(base.wrapping_add(reader.read(self.widths[b])));
        }
        self.cached_block = block;
    }

    /// Absolute index and value of the first unconsumed posting >= target.
    fn locate(&mut self, target: u32) -> Option<(u32, u32)> {
        if let Some(current) = self.peek()
            && current >= target
        {
            return Some((self.pos, current));
        }
        if self.pos >= self.count {
            return None;
        }
        let frontier_block = self.pos / FOR_BLOCK_SIZE as u32;
        let mut block = self
            .bases
            .partition_point(|&base| base <= target)
            .saturating_sub(1)
            .max(frontier_block as usize);
        loop {
            let block_u32 = block as u32;
            self.ensure_block(block_u32);
            let start = if block_u32 == frontier_block {
                (self.pos % FOR_BLOCK_SIZE as u32) as usize
            } else {
                0
            };
            for i in start..self.cache.len() {
                if self.cache[i] >= target {
                    let abs = block_u32 * FOR_BLOCK_SIZE as u32 + i as u32;
                    return Some((abs, self.cache[i]));
                }
            }
            if block + 1 >= self.bases.len() {
                return None;
            }
            block += 1;
        }
    }
}

impl<'a> super::PostingReader for ForReader<'a> {
    fn len(&self) -> u32 {
        self.count
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        if self.pos >= self.count {
            return None;
        }
        let block = self.pos / FOR_BLOCK_SIZE as u32;
        self.ensure_block(block);
        Some(self.cache[(self.pos % FOR_BLOCK_SIZE as u32) as usize])
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        match self.locate(target) {
            Some((abs, value)) => {
                self.pos = abs;
                Some(value)
            }
            None => {
                self.pos = self.count;
                None
            }
        }
    }
}

pub(crate) fn read_u32(data: &[u8], offset: usize) -> u32 {
    let bytes = data
        .get(offset..offset + 4)
        .unwrap_or_else(|| panic!("corrupt postings: header field truncated"));
    u32::from_le_bytes(bytes.try_into().expect("fixed-size field"))
}
