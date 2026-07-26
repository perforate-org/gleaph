//! Fixed 8-byte inline property bytes cell for one inline property bytes overflow log entry.

pub const INLINE_PROPERTY_BYTES_LOG_CELL_BYTES: usize = 8;
/// Max inline property bytes stored inline in the log cell.
pub const MAX_INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH: usize = INLINE_PROPERTY_BYTES_LOG_CELL_BYTES;

/// Returns whether a bucket width stores its log-backed body in `inline_property_bytes_blobs`.
#[inline]
pub fn inline_property_bytes_log_uses_blob(width: u16) -> bool {
    usize::from(width) > MAX_INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH
}

/// Inline property bytes for one overflow-log entry (always 8 bytes on wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlinePropertyBytesLogCell([u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES]);

impl Default for InlinePropertyBytesLogCell {
    fn default() -> Self {
        Self([0u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES])
    }
}

impl InlinePropertyBytesLogCell {
    /// Empty inline-property-bytes-log cell.
    pub const EMPTY: Self = Self([0u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES]);

    /// Returns the on-wire cell bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES] {
        &self.0
    }

    /// Creates a cell from on-wire bytes.
    #[inline]
    pub fn from_bytes(bytes: [u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES]) -> Self {
        Self(bytes)
    }

    /// Returns whether the cell is all zero bytes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == [0u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES]
    }

    /// Inline cell for `1..=[`MAX_INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH`] byte inline property bytes (width from bucket on read).
    pub fn inline(width: u16, inline_property_bytes: &[u8]) -> Self {
        let w = usize::from(width);
        debug_assert!(w > 0 && w <= MAX_INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH);
        debug_assert_eq!(inline_property_bytes.len(), w);
        let mut cell = [0u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES];
        cell[..w].copy_from_slice(inline_property_bytes);
        Self(cell)
    }

    /// Decodes an inline property bytes cell using the bucket-provided `width`.
    pub fn decode_inline(&self, width: u16, out: &mut [u8]) -> Option<usize> {
        let w = usize::from(width);
        if inline_property_bytes_log_uses_blob(width)
            || w == 0
            || w > MAX_INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH
            || out.len() < w
        {
            return None;
        }
        out[..w].copy_from_slice(&self.0[..w]);
        Some(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_property_bytes_log_uses_blob_from_bucket_width() {
        assert!(!inline_property_bytes_log_uses_blob(8));
        assert!(inline_property_bytes_log_uses_blob(9));
    }

    #[test]
    fn inline_cell_round_trips_with_bucket_width() {
        let inline_property_bytes = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let cell = InlinePropertyBytesLogCell::inline(8, &inline_property_bytes);
        assert_eq!(cell.as_bytes()[0], 1);
        assert_eq!(cell.as_bytes()[1], 2);
        let mut out = [0u8; 8];
        assert_eq!(cell.decode_inline(8, &mut out), Some(8));
        assert_eq!(out, inline_property_bytes);
    }
}
