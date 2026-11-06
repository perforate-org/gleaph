//! Building a tree bucket's inline-property leaf blocks.
//!
//! A property leaf holds `property_slots_per_block(width)` values, and a value's place in the leaf is
//! `(slot % K) * width`. Those two facts used to be written three times in one function - the write offset, the block
//! boundary that seals a leaf, and the number of ids the seal produced - so a boundary that disagreed with the offset
//! silently wrapped the payload buffer: the block was published with a valid id and another row's bytes.
//!
//! This type owns all three. Its only write entry takes the **slot**; the window, the offset, and the seal boundary are
//! derived from it, and a slot that does not advance the window (or jumps past one) is an error rather than an
//! overwrite. The caller never sees a boundary, so it cannot seal one at the wrong place.

use crate::labeled::tree_csr::property_slots_per_block;

use super::error::LabeledOperationError;

/// See the module documentation.
pub(crate) struct PropertyLeafWriter {
    width: u16,
    slots_per_block: u32,
    /// The payload being filled. Its length is one LTB payload.
    payload: Vec<u8>,
    /// First slot of the window being filled, `None` before the first write.
    window_start: Option<u32>,
    /// Highest byte written in this window, so a seal publishes exactly what was written.
    written: usize,
    /// Sealed payloads, in seal order. The caller mints them; the order is the ids' order.
    sealed: Vec<Vec<u8>>,
}

impl PropertyLeafWriter {
    /// A writer for values of `width` bytes and LTB payloads of `payload_bytes`.
    pub(crate) fn new(width: u16, payload_bytes: usize) -> Result<Self, LabeledOperationError> {
        let slots_per_block = property_slots_per_block(width).ok_or(
            LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: width,
                edge_inline_property_width: 0,
            },
        )?;
        Ok(Self {
            width,
            slots_per_block,
            payload: vec![0u8; payload_bytes],
            window_start: None,
            written: 0,
            sealed: Vec::new(),
        })
    }

    /// Writes `bytes` as `slot`'s value, sealing the previous window when `slot` moves into a new one.
    ///
    /// `slot` must advance: within the current window it may not repeat, and it may only move into the **next**
    /// window - a jump would leave a leaf that no id accounts for, which is the failure this type exists to prevent.
    pub(crate) fn put(&mut self, slot: u32, bytes: &[u8]) -> Result<(), LabeledOperationError> {
        if bytes.len() != usize::from(self.width) {
            return Err(LabeledOperationError::InlinePropertyBytesWidthMismatch {
                bucket_width: self.width,
                edge_inline_property_width: u16::try_from(bytes.len()).unwrap_or(u16::MAX),
            });
        }
        let block = slot / self.slots_per_block;
        let window_start = block * self.slots_per_block;
        match self.window_start {
            None => self.window_start = Some(window_start),
            Some(current) if current == window_start => {}
            Some(current) if window_start == current.saturating_add(self.slots_per_block) => {
                self.seal()?;
                self.window_start = Some(window_start);
            }
            Some(current) => {
                return Err(LabeledOperationError::PropertyLeafWindowMismatch(
                    super::error::PropertyLeafWindowMismatch {
                        slot,
                        window_start: current,
                        slots_per_block: self.slots_per_block,
                    },
                ));
            }
        }
        let row = (slot % self.slots_per_block) as usize;
        let offset = row * usize::from(self.width);
        self.payload[offset..offset + usize::from(self.width)].copy_from_slice(bytes);
        self.written = self.written.max(offset + usize::from(self.width));
        Ok(())
    }

    /// Seals the last window and answers the payloads to mint, in order.
    pub(crate) fn finish(mut self) -> Result<Vec<Vec<u8>>, LabeledOperationError> {
        if self.window_start.is_some() && self.written > 0 {
            self.seal()?;
        }
        Ok(self.sealed)
    }

    fn seal(&mut self) -> Result<(), LabeledOperationError> {
        if self.written == 0 {
            return Ok(());
        }
        self.sealed.push(self.payload[..self.written].to_vec());
        self.payload.fill(0);
        self.written = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values land where the reader's own arithmetic looks for them: offset `(slot % K) * width` of leaf `slot / K`.
    #[test]
    fn values_land_at_the_position_the_reader_computes() {
        let width = 8u16;
        let mut writer = PropertyLeafWriter::new(width, 4096).expect("writer");
        for slot in 0..1_200u32 {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&slot.to_le_bytes());
            writer.put(slot, &bytes).expect("write the value");
        }
        let sealed = writer.finish().expect("finish");
        let slots_per_block = property_slots_per_block(width).expect("K");
        assert_eq!(
            sealed.len(),
            (1_200u32).div_ceil(slots_per_block) as usize,
            "one payload per leaf the writes touched"
        );
        for slot in 0..1_200u32 {
            let leaf = (slot / slots_per_block) as usize;
            let offset = (slot % slots_per_block) as usize * usize::from(width);
            assert_eq!(
                u32::from_le_bytes(
                    sealed[leaf][offset..offset + 4]
                        .try_into()
                        .expect("4 bytes")
                ),
                slot,
                "slot {slot} is at leaf {leaf}, offset {offset}"
            );
        }
    }

    /// A slot that would leave an unaccounted leaf is refused instead of overwriting a payload.
    #[test]
    fn a_window_jump_is_refused() {
        let mut writer = PropertyLeafWriter::new(8, 4096).expect("writer");
        writer.put(0, &[0u8; 8]).expect("first write");
        let slots_per_block = property_slots_per_block(8).expect("K");
        let jumped = slots_per_block * 3;
        assert!(matches!(
            writer.put(jumped, &[0u8; 8]),
            Err(LabeledOperationError::PropertyLeafWindowMismatch { .. })
        ));
    }
}
