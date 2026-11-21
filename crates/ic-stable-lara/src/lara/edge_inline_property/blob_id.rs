//! Stable identity for edge inline property bytes stored outside the 8-byte values spill cell.

/// Opaque key for edge inline property bytes tied to one values spill run row.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeInlinePropertyBytesBlobId(u64);

impl EdgeInlinePropertyBytesBlobId {
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
