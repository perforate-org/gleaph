//! Fixed 8-byte inline payload cell for one payload overflow log entry.

pub const PAYLOAD_LOG_CELL_BYTES: usize = 8;
/// Max payload bytes stored inline in the log cell.
pub const MAX_PAYLOAD_LOG_INLINE_WIDTH: usize = PAYLOAD_LOG_CELL_BYTES;

/// Returns whether a bucket width stores its log-backed body in `payload_blobs`.
#[inline]
pub fn payload_log_uses_blob(width: u16) -> bool {
    usize::from(width) > MAX_PAYLOAD_LOG_INLINE_WIDTH
}

/// Inline payload bytes for one overflow-log entry (always 8 bytes on wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineValueLogCell([u8; PAYLOAD_LOG_CELL_BYTES]);

impl Default for InlineValueLogCell {
    fn default() -> Self {
        Self([0u8; PAYLOAD_LOG_CELL_BYTES])
    }
}

impl InlineValueLogCell {
    /// Empty payload-log cell.
    pub const EMPTY: Self = Self([0u8; PAYLOAD_LOG_CELL_BYTES]);

    /// Returns the on-wire cell bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8; PAYLOAD_LOG_CELL_BYTES] {
        &self.0
    }

    /// Creates a cell from on-wire bytes.
    #[inline]
    pub fn from_bytes(bytes: [u8; PAYLOAD_LOG_CELL_BYTES]) -> Self {
        Self(bytes)
    }

    /// Returns whether the cell is all zero bytes.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == [0u8; PAYLOAD_LOG_CELL_BYTES]
    }

    /// Inline cell for `1..=[`MAX_PAYLOAD_LOG_INLINE_WIDTH`] byte payloads (width from bucket on read).
    pub fn inline(width: u16, payload_bytes: &[u8]) -> Self {
        let w = usize::from(width);
        debug_assert!(w > 0 && w <= MAX_PAYLOAD_LOG_INLINE_WIDTH);
        debug_assert_eq!(payload_bytes.len(), w);
        let mut cell = [0u8; PAYLOAD_LOG_CELL_BYTES];
        cell[..w].copy_from_slice(payload_bytes);
        Self(cell)
    }

    /// Decodes an inline payload using the bucket-provided `width`.
    pub fn decode_inline(&self, width: u16, out: &mut [u8]) -> Option<usize> {
        let w = usize::from(width);
        if payload_log_uses_blob(width)
            || w == 0
            || w > MAX_PAYLOAD_LOG_INLINE_WIDTH
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
    fn payload_log_uses_blob_from_bucket_width() {
        assert!(!payload_log_uses_blob(8));
        assert!(payload_log_uses_blob(9));
    }

    #[test]
    fn inline_cell_round_trips_with_bucket_width() {
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let cell = InlineValueLogCell::inline(8, &payload);
        assert_eq!(cell.as_bytes()[0], 1);
        assert_eq!(cell.as_bytes()[1], 2);
        let mut out = [0u8; 8];
        assert_eq!(cell.decode_inline(8, &mut out), Some(8));
        assert_eq!(out, payload);
    }
}
