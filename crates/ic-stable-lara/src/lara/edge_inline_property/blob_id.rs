//! Stable identity for edge inline property bytes stored outside the 8-byte inline property bytes overflow log cell.

/// Opaque key for a edge inline property bytes tied to one inline property bytes overflow log slot.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeInlinePropertyBytesBlobId(u64);

impl EdgeInlinePropertyBytesBlobId {
    /// Builds the canonical id for `(leaf_segment, entry_idx)`.
    #[inline]
    pub const fn from_log_site(leaf: u32, entry_idx: u32) -> Self {
        debug_assert!(entry_idx < 256);
        Self(((leaf as u64) << 8) | (entry_idx as u64))
    }

    /// Returns the owning edge segment.
    #[inline]
    pub const fn leaf(self) -> u32 {
        (self.0 >> 8) as u32
    }

    /// Returns the overflow-log entry index within the segment.
    #[inline]
    pub const fn entry_idx(self) -> u32 {
        (self.0 & 0xFF) as u32
    }

    /// Builds the canonical id for `(run, row)`: the site a values spill row's wide value lives
    /// under (ADR 0097 §5). A run's rows stay inside
    /// [`crate::labeled::spill_run_store::MAX_ROWS`], so the row fits the low 16 bits.
    #[inline]
    pub const fn from_run_row(run: u32, row: u32) -> Self {
        debug_assert!(row < 65536);
        Self(((run as u64) << 16) | (row as u64))
    }

    /// Returns the owning values run id.
    #[inline]
    pub const fn run(self) -> u32 {
        (self.0 >> 16) as u32
    }

    /// Returns the row index within the run.
    #[inline]
    pub const fn row(self) -> u32 {
        (self.0 & 0xFFFF) as u32
    }

    /// Returns the packed raw blob id.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Creates a blob id from its packed representation.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}
