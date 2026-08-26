//! Page byte geometry: where each table lives inside a page.
//!
//! A page is a single contiguous span:
//!
//! ```text
//! [PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
//! [code_bytes × capacity]
//! ```
//!
//! `vector_bytes` is padded to a 16-byte boundary so every row's bytes are 16-byte aligned for
//! SIMD scoring (the row stride is itself a multiple of 16). The trailing per-row `code_bytes`
//! table (two-tier precision code tier, Slice 6) exists only when the header's `code_stride` is
//! non-zero: each entry is an 8-byte-aligned `[code_aux 8B][codes …]` pair. Because `vector_bytes`
//! ends on a 16-byte boundary (16-aligned offset + capacity × 16-aligned stride), every code entry
//! is at least 8-byte aligned.

use std::ops::Range;

use crate::header::{HeaderError, PAGE_HEADER_SIZE, PageHeader};
use crate::run::RunEntry;

/// Byte geometry of one page, derived from its header with checked arithmetic.
///
/// The page length is `header + run_table + row_meta + vector_bytes + code_bytes`, with
/// `vector_bytes` aligned up to a 16-byte boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageLayout {
    header_len: usize,
    run_table_len: usize,
    meta_stride: usize,
    row_stride: usize,
    vector_bytes_offset: usize,
    code_stride: usize,
    capacity: u32,
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
        // The code table starts where `vector_bytes` ends — already a multiple of 16.
        let code_offset = checked_page_len(
            vector_bytes_offset,
            header.capacity as usize,
            header.row_stride as usize,
        )
        .ok_or(HeaderError::SpanOverflow)?;
        let page_len = checked_page_len(
            code_offset,
            header.capacity as usize,
            header.code_stride as usize,
        )
        .ok_or(HeaderError::SpanOverflow)?;
        Ok(Self {
            header_len: PAGE_HEADER_SIZE,
            run_table_len,
            meta_stride: header.meta_stride as usize,
            row_stride: header.row_stride as usize,
            vector_bytes_offset,
            code_stride: header.code_stride as usize,
            capacity: header.capacity,
            page_len,
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

    /// Byte range of one row's code segment (`[code_aux 8B][codes …]`). Empty when the page has no
    /// code table (`code_stride == 0`).
    pub fn code_range_at(&self, row: u32) -> Range<usize> {
        let start = self.vector_bytes_offset
            + self.row_stride * self.capacity() as usize
            + row as usize * self.code_stride;
        start..start + self.code_stride
    }

    /// Stored vector stride per row (the header's `row_stride`).
    pub fn vector_stride(&self) -> usize {
        self.row_stride
    }

    /// Per-row code-segment width (`0` when the page has no code table).
    pub fn code_stride(&self) -> usize {
        self.code_stride
    }

    /// Stored row-meta stride per row (4 | 8 | 12).
    pub fn meta_stride(&self) -> usize {
        self.meta_stride
    }

    /// Number of row slots (the header's `capacity`).
    fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Largest `capacity` whose `page_len <= max_page_bytes` for the given page geometry, or `None`
    /// when even a single-row page does not fit (or the geometry is invalid).
    ///
    /// This is the inverse of [`PageLayout::new`] used to size `slots_per_page` from a byte budget.
    /// It reuses the same checked geometry, so it can never disagree with the header/layout
    /// validation the storage layer runs at write time. `page_len` is strictly increasing in
    /// `capacity` (the 16-alignment pad changes by at most 15 bytes, smaller than one
    /// `meta_stride + row_stride`), so a binary search finds the exact maximum.
    pub fn max_capacity_for(
        max_page_bytes: usize,
        row_stride: u32,
        meta_stride: u32,
        run_capacity: u32,
        code_stride: u32,
    ) -> Option<u32> {
        let fits = |capacity: u32| -> bool {
            match PageHeader::with_code_stride(
                capacity,
                row_stride,
                meta_stride,
                run_capacity,
                code_stride,
            ) {
                Ok(header) => PageLayout::new(&header)
                    .map(|layout| layout.page_len() <= max_page_bytes)
                    .unwrap_or(false),
                // Invalid capacity (e.g. 0) or a span overflow: treat as "does not fit".
                Err(_) => false,
            }
        };
        if !fits(1) {
            return None;
        }
        // `page_len >= prefix + capacity * (meta_stride + row_stride)`, so any valid capacity is at
        // most `(max_page_bytes - prefix) / per_row`. Cap at `u32::MAX` before the cast.
        let per_row = meta_stride as usize + row_stride as usize + code_stride as usize;
        let prefix = PAGE_HEADER_SIZE + run_capacity as usize * RunEntry::SIZE;
        let hi = (max_page_bytes
            .saturating_sub(prefix)
            .checked_div(per_row)
            .unwrap_or(0))
        .min(u32::MAX as usize) as u32;
        let mut lo = 1u32;
        let mut hi = hi.max(1);
        let mut best = 1u32;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            if fits(mid) {
                best = mid;
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        Some(best)
    }
}

/// Checked vector-bytes span: `capacity × row_stride + offset`, `None` on overflow.
///
/// Valid `u32` geometry cannot overflow 64-bit `usize` (host or wasm64 canister), but the guard
/// is retained as the fail-closed boundary for any future 32-bit target and as defense-in-depth.
/// Kept as a pure helper so it is unit-testable on any host.
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

    /// `d = 1536` F32: pad stride = align16(4 × 1536) = 6144.
    const D1536_STRIDE: u32 = 6144;

    fn header(capacity: u32, meta_stride: u32, run_capacity: u32) -> PageHeader {
        PageHeader::new(capacity, D1536_STRIDE, meta_stride, run_capacity).expect("valid header")
    }

    #[test]
    fn layout_offsets_for_d1536() {
        let h = header(1024, 4, 8);
        let layout = PageLayout::new(&h).expect("layout");
        assert_eq!(layout.run_table_range(), 32..32 + 8 * 8);
        // row_meta runs to the 16-aligned vector start (96 + 4096 = 4192).
        assert_eq!(layout.row_meta_range(), 32 + 64..4192);
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
        // Tier-off pages have no code table.
        assert_eq!(layout.code_stride(), 0);
        assert!(layout.code_range_at(0).is_empty());
    }

    #[test]
    fn layout_code_table_follows_vector_bytes() {
        // RaBitQ v1 shape for d = 1536: aux 8 B + 24 whole words = 200 B per row.
        let code_stride = 8u32 + 24 * 8;
        let h =
            PageHeader::with_code_stride(4, D1536_STRIDE, 4, 2, code_stride).expect("valid header");
        let layout = PageLayout::new(&h).expect("layout");
        assert_eq!(layout.code_stride(), code_stride as usize);
        // The code table starts exactly at the end of the vector table and is entry-strided.
        let code_start = layout.vector_bytes_offset() + 4 * D1536_STRIDE as usize;
        assert_eq!(layout.code_range_at(0).start, code_start);
        assert_eq!(
            layout.code_range_at(1).start - layout.code_range_at(0).start,
            code_stride as usize
        );
        assert_eq!(layout.code_range_at(3).end, layout.page_len());
        assert_eq!(layout.page_len(), code_start + 4 * code_stride as usize);
        // Code entries stay 8-byte aligned (the vector table ends on a 16-byte boundary).
        assert_eq!(layout.code_range_at(0).start % 8, 0);
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
    fn max_capacity_for_sizes_slots_per_page() {
        // d = 1536 F32, meta 4, single shard: header 32 + run table 8 + pad.
        let c = PageLayout::max_capacity_for(65_536, D1536_STRIDE, 4, 1, 0).expect("fits");
        assert_eq!(c, 10);
        // The chosen capacity fits exactly; one more row overflows the budget.
        assert!(
            PageLayout::new(&header(c, 4, 1))
                .expect("layout")
                .page_len()
                <= 65_536
        );
        let next = PageLayout::new(&header(c + 1, 4, 1)).expect("layout");
        assert!(next.page_len() > 65_536);
        // A per-row code segment can shrink the capacity under a tight budget.
        let code_stride = 8u32 + 24 * 8;
        // 62,000 B admits ten plain rows (61,520 B) but only nine coded rows (63,520 B).
        let c_plain = PageLayout::max_capacity_for(62_000, D1536_STRIDE, 4, 1, 0).expect("fits");
        assert_eq!(c_plain, 10);
        let c_coded =
            PageLayout::max_capacity_for(62_000, D1536_STRIDE, 4, 1, code_stride).expect("fits");
        assert_eq!(c_coded, 9);
        let layout = PageHeader::with_code_stride(c_coded, D1536_STRIDE, 4, 1, code_stride)
            .and_then(|h| PageLayout::new(&h))
            .expect("layout");
        assert!(layout.page_len() <= 62_000);
    }

    #[test]
    fn max_capacity_for_matches_budget_boundary() {
        // d = 17 F32: pad stride 80. Budget 64 KiB yields capacity 779 (see the walk-down proof).
        let stride_80 = |capacity: u32| PageHeader::new(capacity, 80, 4, 1).expect("valid header");
        let c = PageLayout::max_capacity_for(65_536, 80, 4, 1, 0).expect("fits");
        assert!(c > 0);
        assert!(PageLayout::new(&stride_80(c)).expect("layout").page_len() <= 65_536);
        assert!(
            PageLayout::new(&stride_80(c + 1))
                .expect("layout")
                .page_len()
                > 65_536
        );
    }

    #[test]
    fn max_capacity_for_rejects_too_small_budget_and_bad_geometry() {
        assert_eq!(
            PageLayout::max_capacity_for(40, D1536_STRIDE, 4, 1, 0),
            None
        );
        assert_eq!(PageLayout::max_capacity_for(65_536, 0, 4, 1, 0), None);
        assert_eq!(
            PageLayout::max_capacity_for(65_536, D1536_STRIDE, 6, 1, 0),
            None
        );
    }

    #[test]
    fn checked_page_len_guards_overflow() {
        // Overflow path: capacity × row_stride wraps 64-bit `usize`.
        assert_eq!(checked_page_len(usize::MAX, 2, 2), None);
        assert_eq!(checked_page_len(100, usize::MAX, 2), None);
        // Non-overflow path.
        assert_eq!(checked_page_len(100, 10, 10), Some(200));
    }
}
