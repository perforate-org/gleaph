//! Interleaved delta-varint postings with per-posting term frequencies — the tf-carrying
//! counterpart of [`super::VarintReader`], byte layout sized by the slice-6 storage-parity
//! accounting and promoted to a production query-executor mechanism: the text index's
//! search path reads these lists through [`crate::topk`], with every score contribution
//! supplied by callers at query time (plan 0295: as a tf→part lookup table, so the driver
//! computes contributions inline from [`super::PostingReader::tf`]).
//!
//! **Boundary.** Physical facts only: term frequencies are opaque integers; no scoring
//! formula and no analyzer lives in this crate.
//!
//! Byte layout (no magic or version yet; stable-store headers embed those later):
//!
//! ```text
//! count:        u32 LE   // number of docids
//! per posting:  docid delta as LEB128 u32 (first posting: absolute docid)
//!               tf as one raw u8  // encoder caps values > 255 at 255
//! skip trailer: see [`super::skip`] (freq mode: entries add the block posting count)
//! ```
//!
//! The skip trailer's freq-mode entries carry each block's posting count so a jump can
//! restore both the absolute position and the delta base with tf alignment intact.
//!
//! Corruption policy (crate-wide rule): buffers are produced only by [`encode_freq_varint`];
//! truncated or malformed input panics with a `corrupt postings:` message at the first
//! offending read.

use super::skip::{FREQ_ENTRY_BYTES, SkipBuilder, SkipIndex};

/// Encodes strictly increasing, non-empty docids with aligned per-doc term frequencies.
///
/// Frequencies above 255 are capped at 255 (the format stores one raw u8); the caller's
/// slice is not modified.
///
/// # Panics
/// Panics when `docs` is empty or not strictly increasing, or when `tfs.len() != docs.len()`.
pub fn encode_freq_varint(docs: &[u32], tfs: &[u32]) -> Vec<u8> {
    super::varint::assert_strictly_increasing(docs);
    assert!(docs.len() == tfs.len(), "tf list must align with postings");
    let mut out = Vec::with_capacity(docs.len() * 6 + 4);
    out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
    let mut prev: u32 = 0;
    let mut skips = SkipBuilder::new();
    for (i, &doc) in docs.iter().enumerate() {
        skips.posting(i as u32, out.len(), doc);
        let delta = if i == 0 { doc } else { doc - prev };
        super::varint::write_u32(&mut out, delta);
        out.push(tfs[i].min(u32::from(u8::MAX)) as u8);
        prev = doc;
    }
    skips.finish(&mut out, true);
    out
}

/// Cursor over an interleaved delta-varint + u8 tf posting list.
///
/// The docid surface is exactly [`super::PostingReader`]; [`FreqVarintReader::freq`]
/// extends it with the term frequency of the next unconsumed posting (aligned with
/// [`PostingReader::peek`], so score it before consuming).
pub struct FreqVarintReader<'a> {
    data: &'a [u8],
    count: u32,
    /// Bytes consumed from the interleaved stream (after the header).
    byte_pos: usize,
    /// Absolute end of the interleaved payload (the skip trailer starts here).
    payload_end: usize,
    pos: u32,
    /// Last decoded absolute docid (accumulated), valid once pos >= 1.
    prev: u32,
    current: Option<(u32, u32)>,
    skip: SkipIndex,
}

impl<'a> FreqVarintReader<'a> {
    /// Parses the header and skip trailer; payload decoding is lazy.
    ///
    /// # Panics
    /// Panics when the header itself is truncated.
    pub fn new(data: &'a [u8]) -> Self {
        if data.len() < 4 {
            panic!("corrupt postings: freq varint header truncated");
        }
        let count = u32::from_le_bytes(data[..4].try_into().expect("fixed-size header"));
        let (skip, payload_end) =
            SkipIndex::parse(data, count, FREQ_ENTRY_BYTES, true, "freq varint");
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

    fn decode_step(&mut self) -> (u32, u32) {
        let delta = self.decode_delta();
        let value = if self.pos == 0 {
            delta
        } else {
            match self.prev.checked_add(delta) {
                Some(v) => v,
                None => panic!("corrupt postings: freq varint delta overflow"),
            }
        };
        let Some(&tf_byte) = self.data.get(self.byte_pos) else {
            panic!("corrupt postings: freq varint tf byte missing");
        };
        if self.byte_pos >= self.payload_end {
            panic!("corrupt postings: freq varint tf byte missing");
        }
        self.byte_pos += 1;
        self.prev = value;
        (value, u32::from(tf_byte))
    }

    fn decode_delta(&mut self) -> u32 {
        let mut result: u32 = 0;
        let mut shift: u32 = 0;
        loop {
            if self.byte_pos >= self.payload_end {
                panic!("corrupt postings: freq varint stream truncated");
            }
            let Some(&byte) = self.data.get(self.byte_pos) else {
                panic!("corrupt postings: freq varint stream truncated");
            };
            self.byte_pos += 1;
            if shift == 28 && byte > 0x0F {
                panic!("corrupt postings: freq varint exceeds 32 bits");
            }
            result |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return result;
            }
            shift += 7;
            if shift >= 32 {
                panic!("corrupt postings: freq varint exceeds 32 bits");
            }
        }
    }

    fn fill_current(&mut self) {
        if self.current.is_none() && self.pos < self.count {
            self.current = Some(self.decode_step());
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

    /// Term frequency of the next unconsumed posting without consuming it; `None` once
    /// exhausted. Values reflect the stored u8 (already capped at encode time).
    pub fn freq(&mut self) -> Option<u32> {
        self.fill_current();
        self.current.map(|(_, tf)| tf)
    }
}

impl<'a> super::PostingReader for FreqVarintReader<'a> {
    fn len(&self) -> u32 {
        self.count
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        self.fill_current();
        self.current.map(|(docid, _)| docid)
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

    fn tf(&mut self) -> Option<u32> {
        self.freq()
    }

    /// Native fused step: the cached `(docid, tf)` pair is consumed in one move — the
    /// next posting decodes lazily on the caller's following `peek`, so a full traversal
    /// pays exactly one decode per posting across `next_step` + frontier refreshes.
    fn next_step(&mut self) -> Option<(u32, u32)> {
        self.fill_current();
        let step = self.current?;
        self.current = None;
        self.pos += 1;
        Some(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enc::PostingReader;
    use crate::enc::test_support::zipf_postings;

    fn round_trip(docs: &[u32], tfs: &[u32]) {
        let bytes = encode_freq_varint(docs, tfs);
        let mut reader = FreqVarintReader::new(&bytes);
        assert_eq!(reader.len(), docs.len() as u32);
        for (i, (&doc, &tf)) in docs.iter().zip(tfs).enumerate() {
            assert_eq!(reader.peek(), Some(doc), "{i}: peek");
            assert_eq!(reader.freq(), Some(tf.min(u32::from(u8::MAX))), "{i}: freq");
            assert_eq!(reader.pos(), i as u32);
            assert_eq!(reader.next(), Some(doc), "{i}: next");
        }
        assert_eq!(reader.pos(), docs.len() as u32);
        assert!(reader.peek().is_none());
        assert!(reader.freq().is_none());
        assert!(reader.next().is_none());
    }

    #[test]
    fn round_trip_matches_plain_oracle_with_tf_equality() {
        round_trip(&[7], &[1]);
        round_trip(&(0..300).collect::<Vec<u32>>(), &vec![255u32; 300]);
        round_trip(
            &[0, 130, 90_000, 3_000_000_003],
            &[1, 2, 300, u32::from(u8::MAX)],
        );
        // Cap boundary: 255 survives, 256 clamps to 255.
        round_trip(&[1, 2, 3], &[254, 255, 256]);
    }

    #[test]
    fn zipf_shaped_list_round_trips_with_synthetic_tfs() {
        let docs = zipf_postings();
        let tfs: Vec<u32> = docs.iter().map(|&d| (d % 97) + 1).collect();
        round_trip(docs, &tfs);
    }

    #[test]
    fn advance_and_interleaved_next_match_the_plain_oracle() {
        // Same target-class pattern as the codec suite's interleave test: forward-only
        // advance between next() runs, including zero-start, clusters, gaps, exhaustion.
        // tf must be read while the posting is still the unconsumed frontier.
        let docs = vec![
            0u32,
            1,
            2,
            3,
            500,
            501,
            700_000,
            900_000,
            900_001,
            900_002,
            1_500_000,
            2_000_000_000,
        ];
        let tfs: Vec<u32> = docs.iter().map(|d| (*d % 13) + 1).collect();
        let targets: [u32; 12] = [
            0,
            2,
            3,
            4,
            501,
            502,
            700_000,
            800_000,
            900_002,
            1_600_000,
            2_000_000_001,
            2_100_000_000,
        ];
        let bytes = encode_freq_varint(&docs, &tfs);
        let mut reader = FreqVarintReader::new(&bytes);
        let mut idx = 0usize;
        for &target in &targets {
            while idx < docs.len() && docs[idx] < target {
                idx += 1;
            }
            let want = docs.get(idx).copied();
            assert_eq!(reader.advance(target), want, "advance({target})");
            if want.is_some() {
                assert_eq!(reader.peek(), want);
                assert_eq!(reader.freq(), Some(tfs[idx]), "tf at {target}");
            }
            assert_eq!(reader.next(), want, "next after {target}");
            if want.is_some() {
                idx += 1;
            }
        }
        assert!(reader.advance(0).is_none(), "exhaustion is sticky");
    }

    #[test]
    fn truncated_buffers_panic_with_codec_policy_message() {
        let docs = vec![5u32, 6, 7, 800, 801];
        let tfs = vec![1u32, 2, 1, 3, 1];
        let full = encode_freq_varint(&docs, &tfs);

        // Header cut fails at construction.
        let result = std::panic::catch_unwind(|| FreqVarintReader::new(&full[..2]));
        assert!(result.is_err(), "accepted a truncated header");

        // Body cut fails during the payload walk.
        let half = &full[..full.len() / 2];
        let result = std::panic::catch_unwind(|| {
            let mut reader = FreqVarintReader::new(half);
            std::iter::from_fn(|| reader.next()).for_each(|_| {});
        });
        assert!(result.is_err(), "decoded a truncated body");

        // Missing final tf byte fails even though all deltas decoded.
        let missing_tf = &full[..full.len() - 1];
        let result = std::panic::catch_unwind(|| {
            let mut reader = FreqVarintReader::new(missing_tf);
            std::iter::from_fn(|| reader.next()).for_each(|_| {});
        });
        assert!(result.is_err(), "accepted a missing tf byte");

        // Panic payloads carry the crate-wide corruption prefix.
        let payload = std::panic::catch_unwind(|| {
            let mut reader = FreqVarintReader::new(half);
            std::iter::from_fn(|| reader.next()).for_each(|_| {});
        })
        .expect_err("must panic");
        let text = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default();
        assert!(text.starts_with("corrupt postings:"), "{text}");
    }

    #[test]
    fn encoder_rejects_misshapen_input_like_the_plain_codec() {
        let result = std::panic::catch_unwind(|| encode_freq_varint(&[], &[]));
        assert!(result.is_err(), "empty input must be rejected");
        let result = std::panic::catch_unwind(|| encode_freq_varint(&[5, 5], &[1, 1]));
        assert!(result.is_err(), "non-increasing input must be rejected");
        let result = std::panic::catch_unwind(|| encode_freq_varint(&[5, 9], &[1]));
        assert!(result.is_err(), "misaligned tfs must be rejected");
    }

    /// Jumps across level-0 blocks and the level-1 stride must restore both the absolute
    /// position and the interleaved tf stream: every landing reports exactly the oracle
    /// docid and its stored tf.
    #[test]
    fn advance_with_jumps_preserves_tf_alignment_across_skip_levels() {
        let docs: Vec<u32> = (0..4500u32).collect();
        let tfs: Vec<u32> = docs.iter().map(|d| (d % 251) + 1).collect();
        let bytes = encode_freq_varint(&docs, &tfs);
        let mut reader = FreqVarintReader::new(&bytes);
        // Forward-only walk: block edges (level 0), a stride edge (level 1), a
        // misaligned landing inside a later block, and past-end exhaustion.
        for target in [0u32, 127, 128, 4094, 4096, 4200, 4499, 5000] {
            let idx = docs.partition_point(|&d| d < target);
            let want = docs.get(idx).copied();
            assert_eq!(reader.advance(target), want, "advance({target})");
            if want.is_some() {
                assert_eq!(reader.pos(), idx as u32, "pos after advance({target})");
                assert_eq!(
                    reader.freq(),
                    Some(tfs[idx]),
                    "tf aligned after jump to {target}"
                );
                assert_eq!(reader.next(), want, "next consumes the landing posting");
            } else {
                assert!(reader.freq().is_none(), "exhausted landing has no tf");
            }
        }
        assert!(reader.advance(0).is_none(), "exhaustion is sticky");
    }

    /// A populated trailer must not perturb sequential round trips.
    #[test]
    fn sequential_round_trip_unchanged_when_skip_trailer_populated() {
        let docs: Vec<u32> = (0..900u32).collect();
        let tfs: Vec<u32> = docs.iter().map(|d| (d % 200) + 3).collect();
        let bytes = encode_freq_varint(&docs, &tfs);
        let mut reader = FreqVarintReader::new(&bytes);
        for (i, &doc) in docs.iter().enumerate() {
            assert_eq!(reader.peek(), Some(doc), "{i}");
            assert_eq!(reader.freq(), Some(tfs[i]), "{i} tf");
            assert_eq!(reader.next(), Some(doc), "{i} next");
        }
        assert!(reader.next().is_none());
    }

    /// The fused step must equal the `(next, tf)` pair exactly — including across
    /// advance jumps (tf alignment restored from the skip trailer) — and leave the
    /// cursor in the same state as `next`.
    #[test]
    fn fused_next_step_matches_next_tf_pair_across_jumps() {
        let docs: Vec<u32> = (0..4500u32).map(|i| i * 2 + 1).collect();
        let tfs: Vec<u32> = docs.iter().map(|d| (d % 251) + 1).collect();
        let bytes = encode_freq_varint(&docs, &tfs);
        let mut reader = FreqVarintReader::new(&bytes);
        let mut idx = 0usize;
        for target in [0u32, 127, 128, 4094, 4096, 4200, 8999, 10_000] {
            while idx < docs.len() && docs[idx] < target {
                // Drain through the fused primitive itself.
                let want = (docs[idx], tfs[idx]);
                assert_eq!(
                    reader.next_step(),
                    Some(want),
                    "fused step at {} before {target}",
                    docs[idx]
                );
                idx += 1;
            }
            assert_eq!(
                reader.advance(target),
                docs.get(idx).copied(),
                "advance({target})"
            );
            if let Some(&doc) = docs.get(idx) {
                assert_eq!(reader.next_step(), Some((doc, tfs[idx])), "step at {doc}");
                idx += 1;
                // Mixed mode: plain next() after a fused step stays aligned.
                if let Some(&doc) = docs.get(idx) {
                    assert_eq!(reader.peek(), Some(doc));
                    assert_eq!(reader.freq(), Some(tfs[idx]));
                    assert_eq!(reader.next(), Some(doc));
                    idx += 1;
                }
            }
        }
        assert!(reader.next_step().is_none(), "exhaustion is sticky");
    }
}
