//! Minimal Partitioned Elias-Fano ("pef") postings.
//!
//! Fixed-size partitions of [`PEF_PARTITION_SIZE`] docids, encoded as two levels:
//!
//! 1. a plain EF list ([`super::elias_fano`]) over the per-partition upper bounds
//!    (each partition's last docid); and
//! 2. one plain EF blob per partition, universe-bounded by that partition's upper bound.
//!
//! No optimality DP: partition boundaries are fixed so the two-level structure is fully
//! determined by the input. Byte layout:
//!
//! ```text
//! count:        u32 LE   // n
//! part_size:    u32 LE   // == PEF_PARTITION_SIZE
//! meta_len:     u32 LE   // byte length of the bounds EF blob
//! meta bytes:   meta_len // EF encoding of the B strictly increasing upper bounds
//! offsets:      (B + 1) × u32 LE   // partition blob offsets from the end of `offsets`
//! partitions:   concatenated per-partition EF blobs
//! ```
//!
//! Partition lookup for `advance(target)` binary-searches the stateless
//! [`EfReader::value_at`] probes over the bounds list; the first bound >= target is the
//! first partition containing a posting >= target (bounds are each partition's maximum).
//! The active partition's reader is cached and rebuilt on crossing.
//!
//! Corruption policy: buffers are produced only by [`encode_partitioned_ef`]; any
//! truncated or malformed input panics with a `corrupt postings:` message at the first
//! offending read.

use super::PostingReader;
use super::elias_fano::{EfReader, encode_elias_fano};
use super::frame::read_u32;
use super::varint::assert_strictly_increasing;

/// Docids per partition.
pub const PEF_PARTITION_SIZE: u32 = 64;

/// Encodes strictly increasing, non-empty docids as minimal Partitioned Elias-Fano.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing.
pub fn encode_partitioned_ef(docs: &[u32]) -> Vec<u8> {
    assert_strictly_increasing(docs);
    let bounds: Vec<u32> = docs
        .chunks(PEF_PARTITION_SIZE as usize)
        .map(|part| part[part.len() - 1])
        .collect();
    let meta = encode_elias_fano(&bounds);
    let mut partitions = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(bounds.len() + 1);
    for part in docs.chunks(PEF_PARTITION_SIZE as usize) {
        offsets.push(partitions.len() as u32);
        partitions.extend(encode_elias_fano(part));
    }
    offsets.push(partitions.len() as u32);
    let mut out = Vec::with_capacity(12 + meta.len() + offsets.len() * 4 + partitions.len());
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    out.extend_from_slice(&PEF_PARTITION_SIZE.to_le_bytes());
    out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta);
    for offset in &offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&partitions);
    out
}

/// Cursor over a minimal Partitioned Elias-Fano posting list.
pub struct PefReader<'a> {
    data: &'a [u8],
    parts_start: usize,
    part_offsets: Vec<u32>,
    count: u32,
    part_size: u32,
    /// Bounds-list reader whose cursor monotonically rules out partitions below the
    /// target (order-safe: the structural path only runs for targets beyond the
    /// frontier); `meta.pos()` is always the first partition not yet ruled out.
    meta: EfReader<'a>,
    pos: u32,
    active: Option<(u32, EfReader<'a>)>,
}

impl<'a> PefReader<'a> {
    /// Parses and validates the header, bounds reader, and offset table.
    ///
    /// # Panics
    /// Panics when the header or offsets are truncated or inconsistent with `count`.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 12 {
            panic!("corrupt postings: pef header truncated");
        }
        let count = read_u32(data, 0);
        let part_size = read_u32(data, 4);
        let meta_len = read_u32(data, 8) as usize;
        if part_size != PEF_PARTITION_SIZE || count == 0 || data.len() < 12 + meta_len {
            panic!("corrupt postings: pef header inconsistent");
        }
        let meta = EfReader::new(&data[12..12 + meta_len]);
        if meta.len() != count.div_ceil(part_size) {
            panic!("corrupt postings: pef partition count mismatch");
        }
        let offs_start = 12 + meta_len;
        let offs_len = (meta.len() as usize + 1) * 4;
        let parts_start = offs_start + offs_len;
        if data.len() < parts_start {
            panic!("corrupt postings: pef offsets truncated");
        }
        let mut part_offsets = Vec::with_capacity(meta.len() as usize + 1);
        for b in 0..=meta.len() as usize {
            part_offsets.push(read_u32(data, offs_start + b * 4));
        }
        Self {
            data,
            parts_start,
            part_offsets,
            count,
            part_size,
            meta,
            pos: 0,
            active: None,
        }
    }

    fn partition_reader(&self, part: u32) -> EfReader<'a> {
        let b = part as usize;
        let start = self.parts_start + self.part_offsets[b] as usize;
        let end = self.parts_start + self.part_offsets[b + 1] as usize;
        let slice = self
            .data
            .get(start..end)
            .unwrap_or_else(|| panic!("corrupt postings: pef partition truncated"));
        EfReader::new(slice)
    }

    /// Reader for the partition owning absolute index `pos`, fast-forwarded to it.
    fn active_for(&mut self, part: u32) -> EfReader<'a> {
        let mut reader = self.partition_reader(part);
        let within = self.pos % self.part_size;
        for _ in 0..within {
            reader.next();
        }
        reader
    }
}

impl<'a> super::PostingReader for PefReader<'a> {
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
        let part = self.pos / self.part_size;
        let fresh = self.active.as_ref().is_none_or(|&(p, _)| p != part);
        if fresh {
            let reader = self.active_for(part);
            self.active = Some((part, reader));
        }
        let (_, reader) = self.active.as_mut().expect("partition installed above");
        reader.peek()
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            // Keep the active partition reader paired with `pos`.
            let (_, reader) = self
                .active
                .as_mut()
                .expect("peek installed partition above");
            reader.next();
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        if let Some(current) = self.peek()
            && current >= target
        {
            return Some(current);
        }
        if self.pos >= self.count {
            return None;
        }
        // Monotone meta lookup: rule out leading partitions whose upper bound is below
        // the target (each ruled-out bound is consumed; the qualifying bound stays at
        // the window head so later targets in the same partition still see it). The
        // structural path only runs for targets beyond the previous frontier, so this
        // is order-safe and keeps the whole advance incremental.
        loop {
            let Some(bound) = self.meta.peek() else {
                self.pos = self.count;
                return None;
            };
            if bound >= target {
                break;
            }
            self.meta.next();
        }
        let part = self.meta.pos().max(self.pos / self.part_size);
        let mut reader = self.partition_reader(part);
        match reader.advance(target) {
            None => {
                self.pos = self.count;
                None
            }
            Some(value) => {
                // EfReader::advance leaves its cursor ON the found value, whose relative
                // index is exactly reader.pos().
                self.pos = part * self.part_size + reader.pos();
                self.active = Some((part, reader));
                Some(value)
            }
        }
    }
}
