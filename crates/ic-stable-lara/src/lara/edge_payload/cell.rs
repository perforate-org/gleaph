//! Tagged 9-byte payload for one payload overflow log entry.

pub const PAYLOAD_LOG_CELL_BYTES: usize = 9;
/// Max payload bytes stored inline (byte 0 is the tag).
pub const MAX_PAYLOAD_LOG_INLINE_WIDTH: usize = PAYLOAD_LOG_CELL_BYTES - 1;
const TAG_INLINE: u8 = 0;
const TAG_BLOB: u8 = 1;

/// Tagged payload overflow log cell (always 9 bytes on wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PayloadLogCell([u8; PAYLOAD_LOG_CELL_BYTES]);

impl Default for PayloadLogCell {
    fn default() -> Self {
        Self([0u8; PAYLOAD_LOG_CELL_BYTES])
    }
}

impl PayloadLogCell {
    pub const EMPTY: Self = Self([0u8; PAYLOAD_LOG_CELL_BYTES]);

    #[inline]
    pub fn as_bytes(&self) -> &[u8; PAYLOAD_LOG_CELL_BYTES] {
        &self.0
    }

    #[inline]
    pub fn from_bytes(bytes: [u8; PAYLOAD_LOG_CELL_BYTES]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == [0u8; PAYLOAD_LOG_CELL_BYTES]
    }

    #[inline]
    pub fn tag(&self) -> u8 {
        self.0[0]
    }

    /// Inline cell for `1..=[`MAX_PAYLOAD_LOG_INLINE_WIDTH`] byte payloads (width from bucket on read).
    pub fn inline(width: u16, payload_bytes: &[u8]) -> Self {
        let w = usize::from(width);
        debug_assert!(w > 0 && w <= MAX_PAYLOAD_LOG_INLINE_WIDTH);
        debug_assert_eq!(payload_bytes.len(), w);
        let mut cell = [0u8; PAYLOAD_LOG_CELL_BYTES];
        cell[0] = TAG_INLINE;
        cell[1..1 + w].copy_from_slice(payload_bytes);
        Self(cell)
    }

    /// Blob tag only; bytes live in [`super::blob_id::EdgePayloadBlobId`] map keyed by log site.
    pub fn blob(width: u16) -> Self {
        let mut cell = [0u8; PAYLOAD_LOG_CELL_BYTES];
        cell[0] = TAG_BLOB;
        cell[1..3].copy_from_slice(&width.to_le_bytes());
        Self(cell)
    }

    #[inline]
    pub fn is_inline(&self) -> bool {
        self.0[0] == TAG_INLINE
    }

    #[inline]
    pub fn is_blob(&self) -> bool {
        self.0[0] == TAG_BLOB
    }

    /// Physical width recorded in blob cells (`None` for inline; use bucket width on read).
    #[inline]
    pub fn stored_width(&self) -> Option<u16> {
        if self.is_blob() {
            Some(u16::from_le_bytes([self.0[1], self.0[2]]))
        } else {
            None
        }
    }

    pub fn decode_inline(&self, width: u16, out: &mut [u8]) -> Option<usize> {
        if !self.is_inline() {
            return None;
        }
        let w = usize::from(width);
        if w == 0 || w > MAX_PAYLOAD_LOG_INLINE_WIDTH || out.len() < w {
            return None;
        }
        out[..w].copy_from_slice(&self.0[1..1 + w]);
        Some(w)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_cell_stores_u16_width() {
        let cell = PayloadLogCell::blob(300);
        assert_eq!(cell.stored_width(), Some(300));
    }

    #[test]
    fn inline_cell_round_trips_with_bucket_width() {
        let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let cell = PayloadLogCell::inline(8, &payload);
        let mut out = [0u8; 8];
        assert_eq!(cell.decode_inline(8, &mut out), Some(8));
        assert_eq!(out, payload);
    }
}
