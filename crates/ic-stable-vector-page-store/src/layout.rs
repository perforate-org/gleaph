//! Page byte geometry: where each table lives inside a page.
//!
//! A page is a single contiguous span:
//!
//! ```text
//! [PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
//! ```
//!
//! `vector_bytes` is padded to a 16-byte boundary so every row's bytes are 16-byte aligned for
//! SIMD scoring (the row stride is itself a multiple of 16).

use std::ops::Range;

use crate::header::{HeaderError, PAGE_HEADER_SIZE, PageHeader};
use crate::run::RunEntry;

/// Byte geometry of one page, derived from its header with checked arithmetic.
///
/// The page length is `header + run_table + row_meta + vector_bytes`, with `vector_bytes`
/// aligned up to a 16-byte boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageLayout {
    header_len: usize,
    run_table_len: usize,
    meta_stride: usize,
    row_stride: usize,
    vector_bytes_offset: usize,
    page_len: usize,
}

impl PageLayout {
    /// Computes the page geometry from a validated header, failing closed on overflow.
    pub fn new(header: &PageHeader) -> Result<Self, HeaderError> {
        let run_table_len = (header.run_capacity as usize)
            .checked_mul(RunEntry::SIZE)
            .ok_or(HeaderError::SpanOverflow)?;
        let row_meta_len = (header.capacity as usize)
            .checked_mul(header.meta_stride as usize)
            .ok_or(HeaderError::SpanOverflow)?;
        let row_meta_end = PAGE_HEADER_SIZE
            .checked_add(run_table_len)
            .and_then(|v| v.checked_add(row_meta_len))
            .ok_or(HeaderError::SpanOverflow)?;
        let vector_bytes_offset = row_meta_end.next_multiple_of(16);
        Ok(Self {
            header_len: PAGE_HEADER_SIZE,
            run_table_len,
            meta_stride: header.meta_stride as usize,
            row_stride: header.row_stride as usize,
            vector_bytes_offset,
            page_len: checked_page_len(
                vector_bytes_offset,
                header.capacity as usize,
                header.row_stride as usize,
            )
            .ok_or(HeaderError::SpanOverflow)?,
        })
    }

    /// Total on-disk page length in bytes.
    pub fn page_len(&self) -> usize {
        self.page_len
    }

    /// Byte offset of the `vector_bytes` table (always a multiple of 16).
    pub fn vector_bytes_offset(&self) -> usize {
        self.vector_bytes_offset
    }

    /// Byte range of the run table.
    pub fn run_table_range(&self) -> Range<usize> {
        self.header_len..self.header_len + self.run_table_len
    }

    /// Byte range of the row-meta table.
    pub fn row_meta_range(&self) -> Range<usize> {
        self.run_table_range().end..self.vector_bytes_offset
    }

    /// Byte range of one row's meta entry.
    pub fn row_meta_range_at(&self, row: u32) -> Range<usize> {
        let start = self.run_table_range().end + row as usize * self.meta_stride;
        start..start + self.meta_stride
    }

    /// Byte range of one row's vector bytes.
    pub fn vector_range_at(&self, row: u32) -> Range<usize> {
        let start = self.vector_bytes_offset + row as usize * self.row_stride;
        start..start + self.row_stride
    }

    /// Stored vector stride per row (the header's `row_stride`).
    pub fn vector_stride(&self) -> usize {
        self.row_stride
    }

    /// Stored row-meta stride per row (4 | 8 | 12).
    pub fn meta_stride(&self) -> usize {
        self.meta_stride
    }
}

/// Checked vector-bytes span: `capacity × row_stride + offset`, `None` on overflow.
///
/// On 64-bit hosts valid `u32` geometry can never overflow `usize`, but the canister runs on
/// wasm32 where `usize` is 32 bits and `capacity × row_stride` (up to ~2^64) can wrap; this guard
/// is the fail-closed boundary for that target. Kept as a pure helper so it is unit-testable on
/// any host.
fn checked_page_len(
    vector_bytes_offset: usize,
    capacity: usize,
    row_stride: usize,
) -> Option<usize> {
    let vector_bytes_len = capacity.checked_mul(row_stride)?;
    vector_bytes_offset.checked_add(vector_bytes_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::PageHeader;

    /// `d = 1536` F32: pad stride = ceil(1536 / 4) * 16 = 6144.
    const D1536_STRIDE: u32 = 6144;

    fn header(capacity: u32, meta_stride: u32, run_capacity: u32) -> PageHeader {
        PageHeader::new(capacity, D1536_STRIDE, meta_stride, run_capacity).expect("valid header")
    }

    #[test]
    fn layout_offsets_for_d1536() {
        let h = header(1024, 4, 8);
        let layout = PageLayout::new(&h).expect("layout");
        assert_eq!(layout.run_table_range(), 28..28 + 8 * 8);
        // row_meta runs to the 16-aligned vector start (92 + 4096 = 4188, padded to 4192).
        assert_eq!(layout.row_meta_range(), 28 + 64..4192);
        // vector_bytes is 16-byte aligned and rows are stride-separated.
        assert_eq!(layout.vector_bytes_offset() % 16, 0);
        assert_eq!(
            layout.vector_range_at(0),
            layout.vector_bytes_offset()..layout.vector_bytes_offset() + 6144
        );
        assert_eq!(
            layout.vector_range_at(1).start - layout.vector_range_at(0).start,
            D1536_STRIDE as usize
        );
        assert_eq!(
            layout.page_len(),
            layout.vector_bytes_offset() + 1024 * D1536_STRIDE as usize
        );
    }

    #[test]
    fn layout_row_meta_offsets_follow_stride() {
        let h = header(64, 8, 4);
        let layout = PageLayout::new(&h).expect("layout");
        assert_eq!(
            layout.row_meta_range_at(0),
            layout.row_meta_range().start..layout.row_meta_range().start + 8
        );
        assert_eq!(
            layout.row_meta_range_at(1).start - layout.row_meta_range_at(0).start,
            8
        );
        assert_eq!(layout.meta_stride(), 8);
        assert_eq!(layout.vector_stride(), D1536_STRIDE as usize);
    }

    #[test]
    fn checked_page_len_guards_32_bit_overflow() {
        // Overflow path (the wasm32 32-bit `usize` case): capacity × row_stride wraps.
        assert_eq!(checked_page_len(usize::MAX, 2, 2), None);
        assert_eq!(checked_page_len(100, usize::MAX, 2), None);
        // Non-overflow path.
        assert_eq!(checked_page_len(100, 10, 10), Some(200));
    }
}
