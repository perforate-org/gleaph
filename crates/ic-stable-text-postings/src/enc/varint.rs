//! Delta + LEB128 varint postings — the uncompressed-size baseline encoding, upgraded
//! with an inline bi-level skip trailer ([`super::skip`], plan 0295): level 0 every
//! 128-posting logical block (block-end docid + absolute start offset), level 1 every
//! 32 blocks; `advance(target)` binary-searches level 1 → level 0, then linear-decodes
//! inside the landing block. Sequential reads never touch the trailer.
//!
//! Byte layout:
//!
//! ```text
//! count:        u32 LE   // number of docids
//! first_docid:  LEB128 u32
//! gaps:         LEB128 u32 × (count - 1)  // strictly positive
//! skip trailer: see [`super::skip`] (plain mode: docid + offset entries)
//! ```
//!
//! Corruption policy: buffers are produced only by [`encode_varint`]; any truncated or
//! malformed input panics with a `corrupt postings:` message at the first offending
//! read — including a malformed skip trailer.

use super::skip::{PLAIN_ENTRY_BYTES, SkipBuilder, SkipIndex};

/// Encodes strictly increasing, non-empty docids as delta + LEB128 varints.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing.
pub fn encode_varint(docs: &[u32]) -> Vec<u8> {
    assert_strictly_increasing(docs);
    let mut out = Vec::with_capacity(docs.len() * 5 + 4);
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    let mut prev: u32 = 0;
    let mut skips = SkipBuilder::new();
    for (i, &doc) in docs.iter().enumerate() {
        skips.posting(i as u32, out.len(), doc);
        let delta = if i == 0 { doc } else { doc - prev };
        write_u32(&mut out, delta);
        prev = doc;
    }
    skips.finish(&mut out, false);
    out
}

/// Cursor over a delta + LEB128 varint posting list.
pub struct VarintReader<'a> {
    data: &'a [u8],
    count: u32,
    /// Bytes consumed from the delta stream (after the header).
    byte_pos: usize,
    /// Absolute end of the delta payload (the skip trailer starts here).
    payload_end: usize,
    pos: u32,
    /// Last decoded absolute docid (accumulated), valid once pos >= 1.
    prev: u32,
    current: Option<u32>,
    skip: SkipIndex,
}

impl<'a> VarintReader<'a> {
    /// Parses the header and skip trailer; payload decoding is lazy.
    ///
    /// # Panics
    /// Panics when the header itself is truncated.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 4 {
            panic!("corrupt postings: varint header truncated");
        }
        let count = u32::from_le_bytes(data[..4].try_into().expect("fixed-size header"));
        let (skip, payload_end) = SkipIndex::parse(data, count, PLAIN_ENTRY_BYTES, false, "varint");
        Self {
            data,
            count,
            byte_pos: 4,
            payload_end,
            pos: 0,
            prev: 0,
            current: None,
            skip,
        }
    }

    fn decode_delta(&mut self) -> u32 {
        let mut result: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            if self.byte_pos >= self.payload_end {
                panic!("corrupt postings: varint stream truncated");
            }
            let Some(&byte) = self.data.get(self.byte_pos) else {
                panic!("corrupt postings: varint stream truncated");
            };
            self.byte_pos += 1;
            if shift == 28 && byte > 0x0F {
                panic!("corrupt postings: varint exceeds 32 bits");
            }
            result |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return result;
            }
            shift += 7;
            if shift >= 32 {
                panic!("corrupt postings: varint exceeds 32 bits");
            }
        }
    }

    fn fill_current(&mut self) {
        if self.current.is_none() && self.pos < self.count {
            let delta = self.decode_delta();
            let value = if self.pos == 0 {
                delta
            } else {
                match self.prev.checked_add(delta) {
                    Some(v) => v,
                    None => panic!("corrupt postings: varint delta overflow"),
                }
            };
            self.prev = value;
            self.current = Some(value);
        }
    }

    fn jump_toward(&mut self, target: u32) {
        if self.skip.is_empty() {
            return;
        }
        if let Some((offset, pos_before, prev)) =
            self.skip.jump_landing(self.data, target, self.byte_pos)
        {
            self.byte_pos = offset;
            self.pos = pos_before;
            self.prev = prev;
            self.current = None;
        }
    }
}

impl<'a> super::PostingReader for VarintReader<'a> {
    fn len(&self) -> u32 {
        self.count
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        self.fill_current();
        self.current
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            self.current = None;
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        match self.peek() {
            None => return None,
            Some(v) if v >= target => return Some(v),
            Some(_) => self.jump_toward(target),
        }
        loop {
            match self.peek() {
                None => return None,
                Some(v) if v >= target => return Some(v),
                Some(_) => {
                    self.next();
                }
            }
        }
    }

    /// Fused step with the tf-less constant: returns tf 1 without decoding past the
    /// consumed posting (the trait default would peek the NEXT posting just to learn a
    /// constant answer).
    fn next_step(&mut self) -> Option<(u32, u32)> {
        let docid = self.next()?;
        Some((docid, 1))
    }
}

/// Shared encoder-side precondition for every posting codec in this crate.
pub(crate) fn assert_strictly_increasing(docs: &[u32]) {
    assert!(!docs.is_empty(), "postings must be non-empty");
    assert!(
        docs.windows(2).all(|w| w[0] < w[1]),
        "postings must be strictly increasing"
    );
}

/// Appends one LEB128-encoded u32 — the shared wire primitive for delta-based codecs
/// (used directly by the freq-varint encoder).
pub(crate) fn write_u32(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let group = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(group);
            return;
        }
        out.push(group | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::PostingReader;

    /// Dense run spanning several level-1 strides: advance hits every target class
    /// (before-first, block-aligned edges, stride edges, misaligned, exact-last,
    /// past-end) against the plain oracle, with position bookkeeping verified.
    #[test]
    fn advance_matches_oracle_across_skip_levels_and_target_classes() {
        let docs: Vec<u32> = (0..5000u32).collect();
        let bytes = encode_varint(&docs);
        let targets = [
            0u32, // before-first clamp
            126,
            127, // inside first block / its final docid
            128,
            129,      // block-aligned edge + first of next block
            4094,     // last posting of the first level-1 stride
            4095,     // first posting of the second stride
            4096,     // level-0 edge inside the second stride
            4103,     // misaligned inside that block
            4999,     // exact last posting
            5000,     // just past end
            u32::MAX, // far past end
        ];
        let mut reader = VarintReader::new(&bytes);
        for &target in &targets {
            let idx = docs.partition_point(|&d| d < target);
            let want = docs.get(idx).copied();
            assert_eq!(reader.advance(target), want, "advance({target})");
            if want.is_some() {
                assert_eq!(reader.pos(), idx as u32, "pos after advance({target})");
                assert_eq!(reader.peek(), want);
            }
        }

        // Gapped shape: single gaps wider than whole blocks force multi-block jumps.
        let gapped: Vec<u32> = (0..200u32).map(|i| i * 300).collect();
        let bytes = encode_varint(&gapped);
        let mut reader = VarintReader::new(&bytes);
        for &target in &[0u32, 150, 299, 300, 301, 15_000, 59_700, 59_701] {
            let want = gapped.iter().find(|&&d| d >= target).copied();
            assert_eq!(reader.advance(target), want, "gapped advance({target})");
        }
    }

    /// The fused step reports the tf-less constant 1 and consumes exactly one posting,
    /// interleaved with advance jumps exactly like the driver uses it.
    #[test]
    fn fused_next_step_reports_constant_tf_and_consumes_one_posting() {
        let docs: Vec<u32> = (0..300u32).map(|i| i * 7 + 3).collect();
        let bytes = encode_varint(&docs);
        let mut reader = VarintReader::new(&bytes);
        let mut idx = 0usize;
        for target in [0u32, 50, 140, 141, 1000, 2094] {
            while idx < docs.len() && docs[idx] < target {
                assert_eq!(
                    reader.next_step(),
                    Some((docs[idx], 1)),
                    "fused step at {} before {target}",
                    docs[idx]
                );
                idx += 1;
            }
            assert_eq!(reader.advance(target), docs.get(idx).copied());
            if let Some(&doc) = docs.get(idx) {
                assert_eq!(reader.next_step(), Some((doc, 1)), "step at {doc}");
                idx += 1;
            }
        }
        assert!(reader.next_step().is_none(), "exhaustion is sticky");
    }

    /// Sequential decode must be byte-identical to the input even when the skip trailer
    /// is populated (a 1000-posting list spans several blocks and one full stride? —
    /// 8 level-0 blocks; level-1 entries exist too since ceil(8/32)=1).
    #[test]
    fn sequential_round_trip_unchanged_when_skip_trailer_populated() {
        let docs: Vec<u32> = (0..1000u32).map(|i| i * 3 + 11).collect();
        let bytes = encode_varint(&docs);
        let mut reader = VarintReader::new(&bytes);
        let drained: Vec<u32> = std::iter::from_fn(|| reader.next()).collect();
        assert_eq!(drained, docs);
    }

    #[test]
    fn truncated_skip_trailer_panics_with_codec_policy_message() {
        let docs: Vec<u32> = (0..300u32).collect();
        let full = encode_varint(&docs);
        // Cutting into the trailing count word must fail closed at construction.
        let cut = &full[..full.len() - 2];
        let result = std::panic::catch_unwind(|| VarintReader::new(cut));
        let payload = match result {
            Err(payload) => payload,
            Ok(_) => panic!("must reject a truncated trailer"),
        };
        let text = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        assert!(text.starts_with("corrupt postings:"), "{text}");
    }
}
