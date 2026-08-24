//! Plain Elias-Fano postings for one list.
//!
//! Given `n` strictly increasing docids with maximum `last`, the low bits
//! `l = floor(log2(last / n))` (0 when `last < n`) are stored explicitly and the high
//! bits `v >> l` are stored implicitly as a unary bitvector: bit `h_i + i` is set for
//! every posting i. Byte layout:
//!
//! ```text
//! count:         u32 LE   // n
//! low_width:     u8       // l <= 31
//! last:          u32 LE   // universe bound; also the largest docid
//! high_words:    u32 LE   // W = number of u64 words in the unary bitvector
//! unary bits:    W × u64 LE
//! low bits:      ceil(n * l / 8) bytes, LSB-first, zero padded
//! ```
//!
//! Select implementation choice: **scalar word-wise popcount scan** (`u64::count_ones`
//! walking words until the target one is bracketed, then bit-by-bit within the word).
//! No two-level select index is built — PoC-scale minimalism.
//!
//! **Cursor invariant (incremental iteration).** The reader carries a monotone unary
//! scan position `scan_bit` plus the window `highs` of already-decoded high parts for
//! indices `[pos, scan_done)`; the invariant `highs.len() == scan_done - pos` always
//! holds. `peek`/`next` extend the window by exactly one incremental select step
//! (amortized O(1) words: the scan never revisits bits). Monotonic `advance(target)`
//! discards window heads whose high part is below `target >> l` (raw-scanning the unary
//! stream without buffering when the window empties), then walks the equal-high run
//! checking lows — never rescanning the structure from scratch and never calling
//! [`EfReader::value_at`]. Only a backward request (impossible under the forward-only
//! contract) would force a scan restart.
//!
//! Corruption policy: buffers are produced only by [`encode_elias_fano`]; any truncated
//! or malformed input panics with a `corrupt postings:` message at the first offending
//! read.

use super::PostingReader;
use super::bitio::{BitReader, BitWriter};
use super::frame::read_u32;
use super::varint::assert_strictly_increasing;

/// Encodes strictly increasing, non-empty docids as plain Elias-Fano.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing.
pub fn encode_elias_fano(docs: &[u32]) -> Vec<u8> {
    assert_strictly_increasing(docs);
    let n = docs.len();
    let last = docs[n - 1];
    // l = floor(log2(last / n)), defined as 0 when last < n (ilog2 rejects 0).
    let l = last
        .checked_div(n as u32)
        .and_then(|quotient| quotient.checked_ilog2())
        .unwrap_or(0)
        .min(31) as u8;
    let h_max = (last >> l) as usize;
    let word_count = (h_max + n) / 64 + 1;
    let mut words = vec![0u64; word_count];
    for (i, &value) in docs.iter().enumerate() {
        let bit = (value >> l) as usize + i;
        words[bit / 64] |= 1u64 << (bit % 64);
    }
    let mut lows = BitWriter::new();
    if l > 0 {
        let mask = u32::MAX >> (32 - l);
        for &value in docs {
            lows.push(value & mask, l);
        }
    }
    let low_bytes = lows.finish();
    let mut out = Vec::with_capacity(13 + words.len() * 8 + low_bytes.len());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.push(l);
    out.extend_from_slice(&last.to_le_bytes());
    out.extend_from_slice(&(word_count as u32).to_le_bytes());
    for word in words {
        out.extend_from_slice(&word.to_le_bytes());
    }
    out.extend_from_slice(&low_bytes);
    out
}

/// Cursor over a plain Elias-Fano posting list.
pub struct EfReader<'a> {
    data: &'a [u8],
    words_start: usize,
    word_count: usize,
    lows_start: usize,
    count: u32,
    l: u8,
    pos: u32,
    /// Decoded high parts for indices `[pos, scan_done)`; `len() == scan_done - pos`.
    highs: Vec<u32>,
    /// Number of unary indices whose high part has been selected into `highs`.
    scan_done: u32,
    /// Absolute bit position of the incremental unary scan.
    scan_bit: usize,
}

impl<'a> EfReader<'a> {
    /// Parses and validates the header; payload access is lazy.
    ///
    /// # Panics
    /// Panics when the header is truncated or inconsistent.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 13 {
            panic!("corrupt postings: ef header truncated");
        }
        let count = read_u32(data, 0);
        let l = data[4];
        let _last = read_u32(data, 5); // layout position reserved; not needed by reads
        let word_count = read_u32(data, 9) as usize;
        if l > 31 || count == 0 || data.len() < 13 + word_count * 8 {
            panic!("corrupt postings: ef header inconsistent");
        }
        Self {
            data,
            words_start: 13,
            word_count,
            lows_start: 13 + word_count * 8,
            count,
            l,
            pos: 0,
            highs: Vec::new(),
            scan_done: 0,
            scan_bit: 0,
        }
    }

    fn word(&self, wi: usize) -> u64 {
        let start = self.words_start + wi * 8;
        let bytes = self
            .data
            .get(start..start + 8)
            .unwrap_or_else(|| panic!("corrupt postings: ef unary stream truncated"));
        u64::from_le_bytes(bytes.try_into().expect("fixed-size word"))
    }

    fn read_low(&self, index: u32) -> u32 {
        if self.l == 0 {
            return 0;
        }
        let mut reader = BitReader::new(&self.data[self.lows_start..]);
        reader.seek_bits(index as usize * self.l as usize);
        reader.read(self.l)
    }

    /// Scans the unary stream for the next set bit; returns its absolute position and the
    /// high part it encodes for index `scan_done`. Bumps `scan_bit` past the bit.
    fn scan_one(&mut self) -> u32 {
        let mut w = self.scan_bit / 64;
        let mut bit = self.scan_bit % 64;
        loop {
            let word = self.word(w);
            let masked = if bit == 0 {
                word
            } else {
                word & !((1u64 << bit) - 1)
            };
            if masked != 0 {
                let tz = masked.trailing_zeros() as usize;
                let bit_pos = w * 64 + tz;
                let high = bit_pos as u32 - self.scan_done;
                self.scan_bit = bit_pos + 1;
                return high;
            }
            w += 1;
            bit = 0;
        }
    }

    /// Buffers the high part of `index` (and therefore everything before it).
    fn buffer_through(&mut self, index: u32) {
        while self.scan_done <= index {
            if self.scan_done >= self.count {
                panic!("corrupt postings: ef unary index out of range");
            }
            let high = self.scan_one();
            self.highs.push(high);
            self.scan_done += 1;
        }
    }

    /// Stateless random-access decode of index `index` (monotone in `index`). Probe-only:
    /// the incremental iteration and seek paths never call this.
    pub fn value_at(&self, index: u32) -> u32 {
        // The i-th set bit sits at unary position h_i + i, so h_i = position - i.
        let mut seen: u32 = 0;
        for wi in 0..self.word_count {
            let word = self.word(wi);
            let ones = word.count_ones();
            if seen + ones > index {
                let mut remaining = index - seen;
                let mut rest = word;
                loop {
                    let tz = rest.trailing_zeros();
                    if tz == 64 {
                        panic!("corrupt postings: ef unary index out of range");
                    }
                    if remaining == 0 {
                        let bit_pos = wi * 64 + tz as usize;
                        let high = bit_pos as u32 - index;
                        return (high << self.l) | self.read_low(index);
                    }
                    rest &= rest - 1;
                    remaining -= 1;
                }
            }
            seen += ones;
        }
        panic!("corrupt postings: ef unary index out of range");
    }
}

impl<'a> PostingReader for EfReader<'a> {
    fn len(&self) -> u32 {
        self.count
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        if self.pos >= self.count {
            return None;
        }
        self.buffer_through(self.pos);
        Some((self.highs[0] << self.l) | self.read_low(self.pos))
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            // Consume the window head together with `pos` so the invariant
            // `highs.len() == scan_done - pos` survives every step.
            self.highs.remove(0);
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        if let Some(current) = self.peek()
            && current >= target
        {
            return Some(current);
        }
        if self.pos >= self.count {
            return None;
        }
        let h_threshold = target >> self.l;

        // Phase 1: discard everything whose high part cannot reach the threshold.
        // Buffered heads are dropped; once the window empties, the raw scan consumes
        // indices wholesale (they are all below target and never needed again).
        loop {
            if self.pos < self.scan_done {
                if self.highs[0] < h_threshold {
                    self.highs.remove(0);
                    self.pos += 1;
                    continue;
                }
                break;
            }
            if self.scan_done >= self.count {
                self.pos = self.count;
                return None;
            }
            let high = self.scan_one();
            self.scan_done += 1;
            if high < h_threshold {
                self.pos = self.scan_done;
                continue;
            }
            self.highs.push(high);
            break;
        }

        // Phase 2: window heads now have high >= h_threshold. Materialize candidates in
        // order until one clears `target` (equal-high entries qualify only when their
        // low bits reach the target's low part).
        loop {
            if self.pos == self.scan_done {
                if self.scan_done >= self.count {
                    self.pos = self.count;
                    return None;
                }
                let high = self.scan_one();
                self.scan_done += 1;
                self.highs.push(high);
            }
            let high = self.highs[0];
            let value = (high << self.l) | self.read_low(self.pos);
            if value >= target {
                return Some(value);
            }
            self.highs.remove(0);
            self.pos += 1;
        }
    }
}
