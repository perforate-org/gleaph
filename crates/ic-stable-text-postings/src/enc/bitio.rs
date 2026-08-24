//! LSB-first bit packing over byte buffers, shared by the FOR, EF, and PEF codecs.
//!
//! All operations are plain integer arithmetic on little-endian bytes — deterministic
//! across platforms. Streams are zero-padded to whole bytes; readers never observe
//! padding because every consumer knows the exact element count from its header.

/// Accumulates values LSB-first and flushes whole bytes.
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    acc_bits: u32,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            acc_bits: 0,
        }
    }

    /// Appends the low `bits` bits of `value` (bits must be <= 32; higher value bits must
    /// be zero — every call site passes masked values).
    pub(crate) fn push(&mut self, value: u32, bits: u8) {
        debug_assert!(bits <= 32);
        debug_assert!(
            bits == 32 || (value >> bits) == 0,
            "value exceeds bit width"
        );
        self.acc |= u64::from(value) << self.acc_bits;
        self.acc_bits += u32::from(bits);
        while self.acc_bits >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.acc_bits -= 8;
        }
    }

    /// Flushes partial-byte padding (zero bits) and returns the stream.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        if self.acc_bits > 0 {
            self.out.push((self.acc & 0xFF) as u8);
        }
        self.out
    }
}

/// Reads values LSB-first from a byte slice at a bit-granular position.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    pub(crate) fn seek_bits(&mut self, bit_pos: usize) {
        self.bit_pos = bit_pos;
    }

    /// Reads `bits` bits (<= 32) LSB-first; panics past the end of the buffer.
    pub(crate) fn read(&mut self, bits: u8) -> u32 {
        debug_assert!(bits <= 32);
        let mut value: u64 = 0;
        for i in 0..u32::from(bits) as usize {
            let byte = *self
                .data
                .get(self.bit_pos >> 3)
                .unwrap_or_else(|| panic!("corrupt postings: bit stream truncated"));
            let bit = (byte >> (self.bit_pos & 7)) & 1;
            value |= u64::from(bit) << i;
            self.bit_pos += 1;
        }
        value as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_round_trip_preserves_values_and_positions() {
        let mut w = BitWriter::new();
        w.push(0b101, 3);
        w.push(0xFFFF_FFFF, 32);
        w.push(0, 5);
        w.push(0b11_0001, 6);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(3), 0b101);
        assert_eq!(r.read(32), 0xFFFF_FFFF);
        assert_eq!(r.read(5), 0);
        assert_eq!(r.read(6), 0b11_0001);
    }

    #[test]
    fn zero_width_reads_are_noops() {
        let mut r = BitReader::new(&[]);
        assert_eq!(r.read(0), 0);
    }

    #[test]
    #[should_panic(expected = "corrupt postings: bit stream truncated")]
    fn reading_past_end_panics() {
        let mut r = BitReader::new(&[0b0000_0001]);
        r.read(9);
    }
}
