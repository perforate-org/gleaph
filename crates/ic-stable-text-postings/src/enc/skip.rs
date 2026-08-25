//! Shared bi-level inline skip index for the delta-varint posting codecs.
//!
//! Lucene912-style design (investigation D1 note ²): level 0 records one entry per
//! [`SKIP_BLOCK_POSTINGS`]-posting block; level 1 coalesces every [`SKIP_LEVEL1_STRIDE`]
//! level-0 blocks into one entry. Each entry carries the block-end docid, the absolute
//! byte offset where the covered span starts, and (freq mode) the number of postings in
//! the covered span so tf alignment survives jumps. `advance(target)` binary-searches
//! level 1 → level 0, then linear-decodes inside the landing block — Lucene evidence says
//! linear wins inside 128.
//!
//! Wire layout (appended after the delta/interleaved payload; offsets are ABSOLUTE byte
//! positions in the encoded buffer):
//!
//! ```text
//! payload:            as documented by each codec
//! level-0 entries:    skip_count × entry
//! level-1 entries:    ceil(skip_count / SKIP_LEVEL1_STRIDE) × entry
//! skip_count:         u32 LE   // level-0 blocks (= ceil(count / SKIP_BLOCK_POSTINGS));
//!                                // the FINAL four bytes of the buffer
//! ```
//!
//! The trailing position of `skip_count` makes the trailer self-describing: a reader
//! with only the byte slice can size the entry arrays from the buffer end.
//!
//! Entry encoding is fixed-width little-endian: `{end_docid u32, start_offset u32}` in
//! plain mode, `{end_docid u32, start_offset u32, posting_count u32}` in freq mode. A
//! level-1 entry describes its whole stride: `end_docid` of the LAST covered block, the
//! start offset of the FIRST covered block, and (freq mode) their summed posting count.
//!
//! Every encoded buffer carries this trailer, even when `skip_count` is 0 (lists shorter
//! than one block), so readers parse one uniform shape. Corruption policy follows the
//! crate-wide rule: truncated or inconsistent trailers panic with `corrupt postings:`
//! messages, and readers bound all payload decoding at the trailer start so skip bytes
//! can never be mistaken for postings. Blocks are positional and fixed-width, so every
//! block except possibly the last holds exactly [`SKIP_BLOCK_POSTINGS`] postings; the
//! stored count words make that invariant explicit and are what a jump landing walks to
//! restore the absolute posting position.

use crate::blockmax::LOGICAL_BLOCK_SIZE;

/// Postings per level-0 skip block (the logical block size, positional within the list).
pub(crate) const SKIP_BLOCK_POSTINGS: u32 = LOGICAL_BLOCK_SIZE;
/// Level-0 blocks coalesced into one level-1 entry (`32 × 128 = 4_096` postings).
pub(crate) const SKIP_LEVEL1_STRIDE: usize = 32;

/// Byte width of one skip entry in plain mode.
pub(crate) const PLAIN_ENTRY_BYTES: usize = 8;
/// Byte width of one skip entry in freq mode (extra posting-count word).
pub(crate) const FREQ_ENTRY_BYTES: usize = 12;

fn write_u32_le(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u32_le(data: &[u8], at: usize, what: &str) -> u32 {
    let bytes = data
        .get(at..at + 4)
        .unwrap_or_else(|| panic!("corrupt postings: {what} out of bounds"));
    u32::from_le_bytes(bytes.try_into().expect("fixed width"))
}

fn write_entry(out: &mut Vec<u8>, end: u32, offset: u32, count: Option<u32>) {
    write_u32_le(out, end);
    write_u32_le(out, offset);
    if let Some(count) = count {
        write_u32_le(out, count);
    }
}

/// Encoder-side accumulator for the trailer. Feed [`Self::posting`] once per encoded
/// posting in order, then [`Self::finish`] after the last one.
#[derive(Default)]
pub(crate) struct SkipBuilder {
    /// One closed level-0 block: `(end_docid, start_byte_offset, posting_count)`.
    blocks: Vec<(u32, u32, u32)>,
    /// Byte offset where the still-open block began (`None` before the first posting).
    open_offset: Option<usize>,
    open_count: u32,
    prev_docid: u32,
}

impl SkipBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records one posting: `pos` is its 0-based list index, `offset` its absolute byte
    /// position in the output buffer, `docid` its decoded value.
    pub(crate) fn posting(&mut self, pos: u32, offset: usize, docid: u32) {
        if pos.is_multiple_of(SKIP_BLOCK_POSTINGS) {
            self.close_block();
            self.open_offset = Some(offset);
        }
        self.open_count += 1;
        self.prev_docid = docid;
    }

    /// Closes the open block, if any, into `blocks`.
    fn close_block(&mut self) {
        if let Some(offset) = self.open_offset.take() {
            self.blocks
                .push((self.prev_docid, offset as u32, self.open_count));
            self.open_count = 0;
        }
    }

    /// Serializes the trailer onto `out` (which already holds the full payload).
    ///
    /// Emission order: level-0 entries, level-1 entries, then the trailing
    /// `skip_count` word — the reader walks backwards from the buffer end, so the count
    /// must be the final four bytes.
    pub(crate) fn finish(mut self, out: &mut Vec<u8>, freq_mode: bool) {
        self.close_block();
        // Level-0 entries carry their real closed-block posting counts (freq mode).
        for &(end, offset, count) in &self.blocks {
            write_entry(out, end, offset, freq_mode.then_some(count));
        }
        // Aligned stride windows: [0..32), [32..64), …, tail partial.
        for stride_start in (0..self.blocks.len()).step_by(SKIP_LEVEL1_STRIDE) {
            let stride_end = (stride_start + SKIP_LEVEL1_STRIDE).min(self.blocks.len());
            let summed: u32 = self.blocks[stride_start..stride_end]
                .iter()
                .map(|b| b.2)
                .sum();
            write_entry(
                out,
                self.blocks[stride_end - 1].0,
                self.blocks[stride_start].1,
                freq_mode.then_some(summed),
            );
        }
        write_u32_le(out, self.blocks.len() as u32);
    }
}

/// Parsed view over one buffer's trailer.
pub(crate) struct SkipIndex {
    /// Absolute offset where level-0 entries begin (= payload end).
    level0_offset: usize,
    /// Number of level-0 blocks.
    skip_count: usize,
    /// Number of level-1 stride entries.
    stride_count: usize,
    entry_bytes: usize,
    with_counts: bool,
}

impl SkipIndex {
    /// Parses the trailer from the tail of `data`.
    ///
    /// Returns the index plus the absolute offset where the payload ends (all posting
    /// decoding must stay below it).
    ///
    /// # Panics
    /// Panics with a `corrupt postings:` message when the trailer is truncated or
    /// inconsistent with `count`.
    pub(crate) fn parse(
        data: &[u8],
        count: u32,
        entry_bytes: usize,
        with_counts: bool,
        codec: &str,
    ) -> (Self, usize) {
        let total = data.len();
        if total < 4 + 4 + entry_bytes {
            panic!("corrupt postings: {codec} skip trailer truncated");
        }
        let raw = read_u32_le(data, total - 4, &format!("{codec} skip count"));
        let max_blocks = count.div_ceil(SKIP_BLOCK_POSTINGS) as usize;
        if raw as usize > max_blocks {
            panic!("corrupt postings: {codec} skip block count {raw} exceeds {max_blocks}");
        }
        let skip_count = raw as usize;
        let stride_count = skip_count.div_ceil(SKIP_LEVEL1_STRIDE);
        let l0_bytes = skip_count * entry_bytes;
        let l1_bytes = stride_count * entry_bytes;
        let Some(payload_end) = total.checked_sub(4 + l0_bytes + l1_bytes) else {
            panic!("corrupt postings: {codec} skip trailer larger than buffer");
        };
        if payload_end < 4 {
            panic!("corrupt postings: {codec} skip trailer overlaps payload");
        }
        (
            Self {
                level0_offset: payload_end,
                skip_count,
                stride_count,
                entry_bytes,
                with_counts,
            },
            payload_end,
        )
    }

    /// True when the buffer carries no skip blocks (shorter than one block).
    pub(crate) fn is_empty(&self) -> bool {
        self.skip_count == 0
    }

    /// Level-0 entry `index`: `(end_docid, start_offset, count?)`.
    fn block(&self, data: &[u8], index: usize, what: &str) -> (u32, u32, Option<u32>) {
        let base = self.level0_offset + index * self.entry_bytes;
        let end = read_u32_le(data, base, what);
        let offset = read_u32_le(data, base + 4, what);
        let count = self.with_counts.then(|| read_u32_le(data, base + 8, what));
        (end, offset, count)
    }

    /// Level-1 stride entry `index`: `(end_docid, start_offset, count?)`.
    fn stride(&self, data: &[u8], index: usize, what: &str) -> (u32, u32, Option<u32>) {
        let base =
            self.level0_offset + self.skip_count * self.entry_bytes + index * self.entry_bytes;
        let end = read_u32_le(data, base, what);
        let offset = read_u32_le(data, base + 4, what);
        let count = self.with_counts.then(|| read_u32_le(data, base + 8, what));
        (end, offset, count)
    }

    /// Computes the jump landing for `target`:
    /// `(absolute byte offset, restored posting position, restored accumulated docid)`
    /// of the first block whose end docid is `>= target`.
    ///
    /// Returns `None` when every block ends below `target` (the target lies past the
    /// list; the caller exhausts normally), or when the landing would not move the
    /// cursor forward (forward-only contract: the caller keeps linear decoding).
    pub(crate) fn jump_landing(
        &self,
        data: &[u8],
        target: u32,
        current_byte_pos: usize,
    ) -> Option<(usize, u32, u32)> {
        const WHAT: &str = "corrupt postings: skip entry";
        if self.skip_count == 0 || target == 0 {
            return None;
        }
        // Level 1: first stride whose end docid is >= target.
        let (mut lo, mut hi) = (0usize, self.stride_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.stride(data, mid, WHAT).0 < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        // Level 0: first below-target-free block within [stride_start .. stride_end),
        // i.e. the first block whose end docid is >= target. All blocks of earlier
        // strides are fully below target by the level-1 scan.
        let stride_start = lo * SKIP_LEVEL1_STRIDE;
        let (mut blo, mut bhi) = (
            stride_start,
            self.skip_count.min(stride_start + SKIP_LEVEL1_STRIDE),
        );
        while blo < bhi {
            let mid = (blo + bhi) / 2;
            if self.block(data, mid, WHAT).0 < target {
                blo = mid + 1;
            } else {
                bhi = mid;
            }
        }
        if blo >= self.skip_count {
            return None; // every block ends below target
        }
        let (_end, offset, _count) = self.block(data, blo, WHAT);
        let byte_offset = offset as usize;
        if byte_offset <= current_byte_pos {
            return None; // would not move forward; keep the linear walk
        }
        // Restore the absolute posting position: blocks are positional and fixed-width,
        // so exactly `blo × SKIP_BLOCK_POSTINGS` postings precede the landing block. In
        // freq mode the stored per-block counts recompute the same number from data
        // (validated under debug) rather than trusting the invariant blindly.
        let pos_before = blo as u32 * SKIP_BLOCK_POSTINGS;
        if self.with_counts {
            let landing_stride = blo / SKIP_LEVEL1_STRIDE;
            let mut counted = (landing_stride * SKIP_LEVEL1_STRIDE) as u32 * SKIP_BLOCK_POSTINGS;
            for index in landing_stride * SKIP_LEVEL1_STRIDE..blo {
                counted += self.block(data, index, WHAT).2.unwrap_or_default();
            }
            debug_assert_eq!(counted, pos_before);
        }
        // Accumulated docid of the posting before the landing block (its delta base).
        let prev = if blo == 0 {
            0
        } else {
            self.block(data, blo - 1, WHAT).0
        };
        Some((byte_offset, pos_before, prev))
    }
}
