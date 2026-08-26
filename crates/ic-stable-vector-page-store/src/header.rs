//! Slab and page headers: fixed-width on-disk structures with fail-closed validation.
//!
//! Format lineage restarts at version 1 (ADR 0064). Headers use a 3-byte magic plus a binary `u8`
//! version byte. The discarded ASCII-magic format (`VSL1` / `VPG1`, 4th byte `0x31`) is rejected
//! fail-closed because its version byte (`0x31`) no longer matches the binary version (`0x01`).

use crate::layout::PageLayout;

/// 3-byte magic of the slab header.
pub const MAGIC_SLAB: [u8; 3] = *b"VSL";
/// 3-byte magic of the page header.
pub const MAGIC_PAGE: [u8; 3] = *b"VPG";
/// Binary format version byte (lineage restarts at 1; old ASCII `'1'` = `0x31` is rejected).
pub const FORMAT_VERSION: u8 = 1;

/// Maximum number of runs per page: `run_capacity = min(owned_shards, MAX_RUNS)`.
pub const MAX_RUNS: u32 = 64;
/// Smallest allowed row-meta stride: vertex payload only, no aux bytes.
pub const MIN_META_STRIDE: u32 = 4;
/// Largest allowed row-meta stride: vertex payload plus 8 aux bytes.
pub const MAX_META_STRIDE: u32 = 12;

/// On-disk size of [`SlabHeader`] (`3 + 1 + 8 + 4 + 16`).
pub const SLAB_HEADER_SIZE: usize = 32;
/// On-disk size of [`PageHeader`] (`3 + 1 + 7 × 4`). The retired 28-byte layout (six `u32`
/// fields) predates the per-page `code_stride` segment and is rejected fail-closed on reopen
/// (reinstall required).
pub const PAGE_HEADER_SIZE: usize = 32;

/// Smallest allowed per-row code-segment width: off (`no code table`) or an 8-byte-aligned
/// `[code_aux 8B][codes …]` pair (`RaBitQ v1`: aux 8 B + whole 64-bit words).
pub const MIN_CODE_STRIDE: u32 = 0;
const CODE_STRIDE_ALIGN: u32 = 8;

/// Fail-closed header validation error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// Header magic does not match the expected 3-byte magic.
    BadMagic {
        /// Expected magic bytes.
        expected: [u8; 3],
        /// Magic bytes found on disk.
        actual: [u8; 3],
    },
    /// Format version byte is not [`FORMAT_VERSION`]; old ASCII-magic data is rejected here.
    UnsupportedVersion {
        /// Expected version byte.
        expected: u8,
        /// Version byte found on disk.
        actual: u8,
    },
    /// `meta_stride` is not one of 4 | 8 | 12.
    InvalidMetaStride(u32),
    /// `row_stride` is zero or not a multiple of 16 (the SIMD alignment contract).
    InvalidRowStride(u32),
    /// `capacity` is zero.
    ZeroCapacity,
    /// `run_capacity` is zero or exceeds [`MAX_RUNS`].
    InvalidRunCapacity(u32),
    /// Invalid `code_stride`: not off (`0`) and not a multiple of 8.
    InvalidCodeStride(u32),
    /// `run_count` exceeds `run_capacity`.
    RunCountExceedsCapacity {
        /// Stored run count.
        run_count: u32,
        /// Stored run capacity.
        run_capacity: u32,
    },
    /// The derived page span overflows checked arithmetic.
    SpanOverflow,
}

/// Fixed-width slab header at offset 0 of the row-slab region.
///
/// `occupied_tail` is the byte offset of the first unused byte; pages are appended there and the
/// header is rewritten last so a crash between the two leaves a valid, smaller slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlabHeader {
    /// Byte offset of the first unused byte in the slab (end of the last appended page).
    pub occupied_tail: u64,
    /// Format flags (reserved for future layout switches).
    pub flags: u32,
    /// Reserved bytes (zero on write).
    pub reserved: [u8; 16],
}

impl SlabHeader {
    /// Constructs a fresh header at the initial tail.
    pub const fn new(occupied_tail: u64, flags: u32) -> Self {
        Self {
            occupied_tail,
            flags,
            reserved: [0; 16],
        }
    }

    /// Encodes the header into its on-disk representation.
    pub fn to_bytes(&self) -> [u8; SLAB_HEADER_SIZE] {
        let mut out = [0u8; SLAB_HEADER_SIZE];
        out[0..3].copy_from_slice(&MAGIC_SLAB);
        out[3] = FORMAT_VERSION;
        out[4..12].copy_from_slice(&self.occupied_tail.to_le_bytes());
        out[12..16].copy_from_slice(&self.flags.to_le_bytes());
        out[16..32].copy_from_slice(&self.reserved);
        out
    }

    /// Decodes a header from its on-disk representation and validates it fail-closed.
    pub fn from_bytes(bytes: &[u8; SLAB_HEADER_SIZE]) -> Result<Self, HeaderError> {
        let actual_magic = [bytes[0], bytes[1], bytes[2]];
        if actual_magic != MAGIC_SLAB {
            return Err(HeaderError::BadMagic {
                expected: MAGIC_SLAB,
                actual: actual_magic,
            });
        }
        let version = bytes[3];
        if version != FORMAT_VERSION {
            return Err(HeaderError::UnsupportedVersion {
                expected: FORMAT_VERSION,
                actual: version,
            });
        }
        let mut occupied_tail = [0u8; 8];
        occupied_tail.copy_from_slice(&bytes[4..12]);
        let mut flags = [0u8; 4];
        flags.copy_from_slice(&bytes[12..16]);
        let mut reserved = [0u8; 16];
        reserved.copy_from_slice(&bytes[16..32]);
        Ok(Self {
            occupied_tail: u64::from_le_bytes(occupied_tail),
            flags: u32::from_le_bytes(flags),
            reserved,
        })
    }
}

/// Fixed-width page header at offset 0 of every page in the row slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageHeader {
    /// Number of row slots in the page.
    pub capacity: u32,
    /// Stored vector stride per row (`pad_stride_bytes`; 16-byte aligned for SIMD).
    pub row_stride: u32,
    /// Stored row-meta stride: 4 | 8 | 12 (`4 + aux_bytes`).
    pub meta_stride: u32,
    /// Maximum number of runs (`min(owned_shards, MAX_RUNS)`).
    pub run_capacity: u32,
    /// Number of runs currently recorded.
    pub run_count: u32,
    /// Per-row code-segment width (the two-tier precision code tier; Slice 6). `0` = the page has
    /// no code table; otherwise an 8-byte-aligned `[code_aux 8B][codes …]` entry per row. The
    /// header keeps pages **self-describing**: a rebuild can switch the tier on for the next
    /// generation, so teardown-residual pages of other generations may legitimately carry a
    /// different `code_stride` than the current definition.
    pub code_stride: u32,
}

impl PageHeader {
    /// Constructs a header and validates it; fails closed on any invalid geometry. The page has
    /// no code segment (`code_stride = 0`). Tier-on geometry goes through
    /// [`Self::with_code_stride`].
    pub fn new(
        capacity: u32,
        row_stride: u32,
        meta_stride: u32,
        run_capacity: u32,
    ) -> Result<Self, HeaderError> {
        Self::with_code_stride(capacity, row_stride, meta_stride, run_capacity, 0)
    }

    /// Constructs a header with an explicit per-row code-segment width and validates it.
    pub fn with_code_stride(
        capacity: u32,
        row_stride: u32,
        meta_stride: u32,
        run_capacity: u32,
        code_stride: u32,
    ) -> Result<Self, HeaderError> {
        let header = Self {
            capacity,
            row_stride,
            meta_stride,
            run_capacity,
            run_count: 0,
            code_stride,
        };
        header.validate()?;
        Ok(header)
    }

    /// Sets the run count, failing closed when it would exceed `run_capacity`.
    pub fn set_run_count(&mut self, run_count: u32) -> Result<(), HeaderError> {
        if run_count > self.run_capacity {
            return Err(HeaderError::RunCountExceedsCapacity {
                run_count,
                run_capacity: self.run_capacity,
            });
        }
        self.run_count = run_count;
        Ok(())
    }

    /// Validates every invariant: magic/version (encoded by `to_bytes`), meta/row strides,
    /// capacity, run bounds, and span overflow.
    pub fn validate(&self) -> Result<(), HeaderError> {
        if self.capacity == 0 {
            return Err(HeaderError::ZeroCapacity);
        }
        if !matches!(self.meta_stride, 4 | 8 | 12) {
            return Err(HeaderError::InvalidMetaStride(self.meta_stride));
        }
        if self.row_stride == 0 || !self.row_stride.is_multiple_of(16) {
            return Err(HeaderError::InvalidRowStride(self.row_stride));
        }
        if self.run_capacity == 0 || self.run_capacity > MAX_RUNS {
            return Err(HeaderError::InvalidRunCapacity(self.run_capacity));
        }
        if self.code_stride != MIN_CODE_STRIDE
            && !self.code_stride.is_multiple_of(CODE_STRIDE_ALIGN)
        {
            return Err(HeaderError::InvalidCodeStride(self.code_stride));
        }
        if self.run_count > self.run_capacity {
            return Err(HeaderError::RunCountExceedsCapacity {
                run_count: self.run_count,
                run_capacity: self.run_capacity,
            });
        }
        // The derived span must fit in usize/u64 checked arithmetic; `PageLayout::new` re-checks
        // the multiplication overflow and is the single place that computes spans.
        let _ = PageLayout::new(self)?;
        Ok(())
    }

    /// Encodes the header into its on-disk representation.
    pub fn to_bytes(&self) -> [u8; PAGE_HEADER_SIZE] {
        let mut out = [0u8; PAGE_HEADER_SIZE];
        out[0..3].copy_from_slice(&MAGIC_PAGE);
        out[3] = FORMAT_VERSION;
        out[4..8].copy_from_slice(&self.capacity.to_le_bytes());
        out[8..12].copy_from_slice(&self.row_stride.to_le_bytes());
        out[12..16].copy_from_slice(&self.meta_stride.to_le_bytes());
        out[16..20].copy_from_slice(&self.run_capacity.to_le_bytes());
        out[20..24].copy_from_slice(&self.run_count.to_le_bytes());
        // Slice 6 (two-tier precision): `code_stride` extends the retired 28-byte layout; the
        // retired form is rejected fail-closed on decode (its trailing bytes cannot validate).
        out[24..28].copy_from_slice(&self.code_stride.to_le_bytes());
        out
    }

    /// Decodes a header from its on-disk representation and validates it fail-closed.
    pub fn from_bytes(bytes: &[u8; PAGE_HEADER_SIZE]) -> Result<Self, HeaderError> {
        let actual_magic = [bytes[0], bytes[1], bytes[2]];
        if actual_magic != MAGIC_PAGE {
            return Err(HeaderError::BadMagic {
                expected: MAGIC_PAGE,
                actual: actual_magic,
            });
        }
        let version = bytes[3];
        if version != FORMAT_VERSION {
            return Err(HeaderError::UnsupportedVersion {
                expected: FORMAT_VERSION,
                actual: version,
            });
        }
        let read_u32 = |range: std::ops::Range<usize>| {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(&bytes[range]);
            u32::from_le_bytes(buf)
        };
        let header = Self {
            capacity: read_u32(4..8),
            row_stride: read_u32(8..12),
            meta_stride: read_u32(12..16),
            run_capacity: read_u32(16..20),
            run_count: read_u32(20..24),
            code_stride: read_u32(24..28),
        };
        header.validate()?;
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> PageHeader {
        PageHeader::new(1024, 6144, 4, 8).expect("valid header")
    }

    #[test]
    fn slab_header_roundtrips() {
        let original = SlabHeader::new(4096, 0);
        let bytes = original.to_bytes();
        assert_eq!(SlabHeader::from_bytes(&bytes).expect("decode"), original);
    }

    #[test]
    fn slab_header_bad_magic_rejected() {
        let mut bytes = SlabHeader::new(0, 0).to_bytes();
        bytes[0] = b'X';
        let err = SlabHeader::from_bytes(&bytes).expect_err("bad magic must fail");
        assert_eq!(
            err,
            HeaderError::BadMagic {
                expected: MAGIC_SLAB,
                actual: *b"XSL",
            }
        );
    }

    #[test]
    fn discarded_ascii_magic_version_rejected_fail_closed() {
        // Old format: `VSL` + ASCII '1' (0x31) + 64-bit tail. The version byte no longer matches
        // the binary version 1 (0x01), so old-format data must be rejected, never reinterpreted.
        let mut bytes = SlabHeader::new(0, 0).to_bytes();
        bytes[3] = b'1'; // 0x31
        let err = SlabHeader::from_bytes(&bytes).expect_err("old ASCII magic must fail");
        assert_eq!(
            err,
            HeaderError::UnsupportedVersion {
                expected: FORMAT_VERSION,
                actual: 0x31,
            }
        );
    }

    #[test]
    fn page_header_roundtrips_and_validate() {
        let original = header();
        let decoded = PageHeader::from_bytes(&original.to_bytes()).expect("decode valid header");
        assert_eq!(decoded, original);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn page_header_code_stride_roundtrips_and_validates() {
        // Tier-off pages encode code_stride = 0 and stay byte-stable in the first 28 bytes.
        let off = header();
        assert_eq!(off.code_stride, 0);
        // Tier-on pages carry the per-row `[code_aux 8B][codes …]` width (8-byte aligned).
        let on = PageHeader::with_code_stride(1024, 6144, 4, 8, 8 + 24 * 8).expect("valid header");
        assert_eq!(PageHeader::from_bytes(&on.to_bytes()).expect("decode"), on);
        assert_eq!(
            PageHeader::with_code_stride(1024, 6144, 4, 8, 7).expect_err("unaligned code stride"),
            HeaderError::InvalidCodeStride(7)
        );
    }

    #[test]
    fn page_header_rejects_bad_magic() {
        let mut bytes = header().to_bytes();
        bytes[0] = b'X';
        let err = PageHeader::from_bytes(&bytes).expect_err("bad magic must fail");
        assert_eq!(
            err,
            HeaderError::BadMagic {
                expected: MAGIC_PAGE,
                actual: *b"XPG",
            }
        );
    }

    #[test]
    fn page_header_rejects_discarded_ascii_version() {
        let mut bytes = header().to_bytes();
        bytes[3] = b'1';
        let err = PageHeader::from_bytes(&bytes).expect_err("old ASCII magic must fail");
        assert_eq!(
            err,
            HeaderError::UnsupportedVersion {
                expected: FORMAT_VERSION,
                actual: 0x31,
            }
        );
    }

    #[test]
    fn page_header_rejects_invalid_meta_stride() {
        let err = PageHeader::new(8, 6144, 6, 4).expect_err("meta_stride 6 must fail");
        assert_eq!(err, HeaderError::InvalidMetaStride(6));
        // Boundary values are accepted.
        for stride in [MIN_META_STRIDE, 8, MAX_META_STRIDE] {
            PageHeader::new(8, 6144, stride, 4).expect("valid meta_stride");
        }
    }

    #[test]
    fn page_header_rejects_invalid_row_stride() {
        assert_eq!(
            PageHeader::new(8, 0, 4, 4).expect_err("zero row_stride"),
            HeaderError::InvalidRowStride(0)
        );
        assert_eq!(
            PageHeader::new(8, 17, 4, 4).expect_err("non-16-multiple row_stride"),
            HeaderError::InvalidRowStride(17)
        );
    }

    #[test]
    fn page_header_rejects_zero_capacity() {
        assert_eq!(
            PageHeader::new(0, 6144, 4, 4).expect_err("zero capacity"),
            HeaderError::ZeroCapacity
        );
    }

    #[test]
    fn page_header_rejects_invalid_run_capacity() {
        assert_eq!(
            PageHeader::new(8, 6144, 4, 0).expect_err("zero run_capacity"),
            HeaderError::InvalidRunCapacity(0)
        );
        assert_eq!(
            PageHeader::new(8, 6144, 4, MAX_RUNS + 1).expect_err("over-max run_capacity"),
            HeaderError::InvalidRunCapacity(MAX_RUNS + 1)
        );
    }

    #[test]
    fn page_header_set_run_count_is_bounded() {
        let mut h = header();
        h.set_run_count(8).expect("within capacity");
        assert_eq!(h.run_count, 8);
        let err = h.set_run_count(9).expect_err("over capacity");
        assert_eq!(
            err,
            HeaderError::RunCountExceedsCapacity {
                run_count: 9,
                run_capacity: 8,
            }
        );
    }

    #[test]
    fn page_header_rejects_oversized_row_stride_before_span_math() {
        // `u32::MAX` fails the 16-byte alignment contract before any span math runs. Valid `u32`
        // geometry cannot overflow 64-bit `usize` (host or wasm64); the overflow guard in
        // `PageLayout::checked_page_len` is retained defensively and tested there.
        assert_eq!(
            PageHeader::new(u32::MAX, u32::MAX, 4, 4).expect_err("alignment violation"),
            HeaderError::InvalidRowStride(u32::MAX)
        );
    }
}
