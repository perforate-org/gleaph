//! Posting-list encoding kernels.
//!
//! Every encoding follows one contract (see [`PostingReader`]):
//!
//! - input to an encoder is a **strictly increasing, non-empty** `&[u32]` docid list;
//! - encoded output is opaque bytes whose header self-describes the count, so
//!   [`PostingReader::len`] is O(1) without decoding;
//! - a reader yields ascending docids sequentially, can jump forward with
//!   [`PostingReader::advance(target)`] to the first docid >= target, and reports its
//!   absolute frontier via [`PostingReader::pos`].
//!
//! **Corruption policy** (one rule for the whole crate): encoded buffers are produced
//! only by this module's encoders; decoding truncated or malformed input panics loudly —
//! posting codecs use messages prefixed `corrupt postings:`, merge cursors use
//! `corrupt merge cursor:`. A mismatch means memory corruption or layout skew, which
//! must fail closed instead of yielding wrong postings. Stable-store headers (magic +
//! layout version) will wrap these byte layouts in a later slice; none are built here.
//!
//! All arithmetic is integer-only; iteration order is fully determined by the bytes.

mod bitio;
mod elias_fano;
mod frame;
mod freq_varint;
mod partitioned_ef;
mod skip;
mod varint;

pub use elias_fano::{EfReader, encode_elias_fano};
pub use frame::{FOR_BLOCK_SIZE, ForReader, encode_frame_of_reference};
pub use freq_varint::{FreqVarintReader, encode_freq_varint};
pub use partitioned_ef::{PEF_PARTITION_SIZE, PefReader, encode_partitioned_ef};
pub use varint::{VarintReader, encode_varint};

/// Cursor over one sorted posting list.
///
/// `advance(target)` only moves forward from the current frontier; a target at or below
/// the current docid returns the current docid unchanged. Exhaustion is sticky: readers
/// return `None` from every method once consumed past the end.
pub trait PostingReader {
    /// Total postings in the list, known from the header without decoding.
    fn len(&self) -> u32;
    /// Whether no unconsumed postings remain.
    fn is_empty(&self) -> bool {
        self.pos() == self.len()
    }
    /// Absolute index of the next unconsumed posting.
    fn pos(&self) -> u32;
    /// Next unconsumed docid without consuming it.
    fn peek(&mut self) -> Option<u32>;
    /// Consumes and returns the next unconsumed docid.
    fn next(&mut self) -> Option<u32>;
    /// First unconsumed docid >= `target`, consuming everything skipped; `None` when no
    /// remaining docid satisfies the bound.
    fn advance(&mut self, target: u32) -> Option<u32>;
    /// Term frequency carried by the next unconsumed posting, aligned with [`Self::peek`]
    /// (score it before consuming). Codecs that store no frequencies report exactly one
    /// occurrence per posting; `None` once exhausted.
    fn tf(&mut self) -> Option<u32> {
        self.peek().map(|_| 1)
    }
    /// Fused hot-loop step — consumes and returns `(docid, tf)` in ONE dispatch instead
    /// of the [`Self::next`] + [`Self::tf`] pair. Semantics are exactly the composition:
    /// tf-less codecs report one occurrence per posting; exhaustion is sticky. The
    /// default composes those two methods; codecs override it where a native fused decode
    /// avoids redundant cursor work (`FreqVarintReader`) or a wasted lookahead decode for
    /// a constant answer (`VarintReader`). Skipping stays on [`Self::advance`]; this is
    /// the posting-list driver's only per-candidate primitive.
    fn next_step(&mut self) -> Option<(u32, u32)> {
        let docid = self.next()?;
        Some((docid, self.tf().unwrap_or(1)))
    }
}

/// Reader over an unencoded strictly increasing slice — the oracle for tests and the
/// plain-run form accepted by the merge. Unlike the encodings, it may be empty.
pub struct PlainReader<'a> {
    docs: &'a [u32],
    pos: u32,
}

impl<'a> PlainReader<'a> {
    /// Wraps an unencoded strictly increasing slice (may be empty).
    pub fn new(docs: &'a [u32]) -> Self {
        Self { docs, pos: 0 }
    }
}

impl<'a> PostingReader for PlainReader<'a> {
    fn len(&self) -> u32 {
        self.docs.len() as u32
    }

    fn pos(&self) -> u32 {
        self.pos
    }

    fn peek(&mut self) -> Option<u32> {
        self.docs.get(self.pos as usize).copied()
    }

    fn next(&mut self) -> Option<u32> {
        let value = self.peek();
        if value.is_some() {
            self.pos += 1;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        while let Some(value) = self.peek() {
            if value >= target {
                return Some(value);
            }
            self.pos += 1;
        }
        None
    }
}

/// Exhaustive dispatch over every reader kind in this crate: lets one [`crate::merge::MergeState`]
/// mix encodings per run without trait objects in hot paths.
pub enum AnyPostingReader<'a> {
    /// Unencoded run.
    Plain(PlainReader<'a>),
    /// Delta + LEB128 varints.
    Varint(VarintReader<'a>),
    /// Framing-of-Reference fixed blocks.
    For(ForReader<'a>),
    /// Plain Elias-Fano.
    Ef(EfReader<'a>),
    /// Partitioned Elias-Fano.
    Pef(PefReader<'a>),
}

impl<'a> PostingReader for AnyPostingReader<'a> {
    fn len(&self) -> u32 {
        match self {
            Self::Plain(r) => r.len(),
            Self::Varint(r) => r.len(),
            Self::For(r) => r.len(),
            Self::Ef(r) => r.len(),
            Self::Pef(r) => r.len(),
        }
    }

    fn pos(&self) -> u32 {
        match self {
            Self::Plain(r) => r.pos(),
            Self::Varint(r) => r.pos(),
            Self::For(r) => r.pos(),
            Self::Ef(r) => r.pos(),
            Self::Pef(r) => r.pos(),
        }
    }

    fn peek(&mut self) -> Option<u32> {
        match self {
            Self::Plain(r) => r.peek(),
            Self::Varint(r) => r.peek(),
            Self::For(r) => r.peek(),
            Self::Ef(r) => r.peek(),
            Self::Pef(r) => r.peek(),
        }
    }

    fn next(&mut self) -> Option<u32> {
        match self {
            Self::Plain(r) => r.next(),
            Self::Varint(r) => r.next(),
            Self::For(r) => r.next(),
            Self::Ef(r) => r.next(),
            Self::Pef(r) => r.next(),
        }
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        match self {
            Self::Plain(r) => r.advance(target),
            Self::Varint(r) => r.advance(target),
            Self::For(r) => r.advance(target),
            Self::Ef(r) => r.advance(target),
            Self::Pef(r) => r.advance(target),
        }
    }

    fn tf(&mut self) -> Option<u32> {
        match self {
            Self::Plain(r) => r.tf(),
            Self::Varint(r) => r.tf(),
            Self::For(r) => r.tf(),
            Self::Ef(r) => r.tf(),
            Self::Pef(r) => r.tf(),
        }
    }

    fn next_step(&mut self) -> Option<(u32, u32)> {
        match self {
            Self::Plain(r) => r.next_step(),
            Self::Varint(r) => r.next_step(),
            Self::For(r) => r.next_step(),
            Self::Ef(r) => r.next_step(),
            Self::Pef(r) => r.next_step(),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Shared adversarial shapes, codec dispatch, and the derived Zipf fixture used by
    //! codec and merge tests.

    use super::*;

    /// One encoding: name, encoder fn, and reader kind for dispatch.
    pub(crate) enum Kind {
        Varint,
        For,
        Ef,
        Pef,
    }

    pub(crate) fn kinds() -> Vec<(&'static str, Kind)> {
        vec![
            ("varint", Kind::Varint),
            ("for", Kind::For),
            ("ef", Kind::Ef),
            ("pef", Kind::Pef),
        ]
    }

    pub(crate) fn encode(kind: &Kind, docs: &[u32]) -> Vec<u8> {
        match kind {
            Kind::Varint => encode_varint(docs),
            Kind::For => encode_frame_of_reference(docs),
            Kind::Ef => encode_elias_fano(docs),
            Kind::Pef => encode_partitioned_ef(docs),
        }
    }

    pub(crate) fn open<'a>(kind: &Kind, bytes: &'a [u8]) -> AnyPostingReader<'a> {
        match kind {
            Kind::Varint => AnyPostingReader::Varint(VarintReader::new(bytes)),
            Kind::For => AnyPostingReader::For(ForReader::new(bytes)),
            Kind::Ef => AnyPostingReader::Ef(EfReader::new(bytes)),
            Kind::Pef => AnyPostingReader::Pef(PefReader::new(bytes)),
        }
    }

    pub(crate) fn drain(reader: &mut impl PostingReader) -> Vec<u32> {
        std::iter::from_fn(|| reader.next()).collect()
    }

    /// Adversarial shapes covering the classes named by the plan: singleton, two
    /// elements, dense consecutive cluster, large gaps, mixed gaps + clusters, and
    /// full-range ids near `u32::MAX`.
    pub(crate) fn shapes() -> Vec<(&'static str, Vec<u32>)> {
        vec![
            ("singleton", vec![7]),
            ("two", vec![3, 9]),
            ("dense_cluster", (100..=355).collect()),
            // Zero-starting dense run: forces EF's low width to 0 (last < n).
            ("zero_start_dense", (0..=199).collect()),
            (
                "large_gaps",
                vec![0, 5_000, 90_000, 4_000_000, 2_000_000_000],
            ),
            (
                "mixed",
                vec![
                    1,
                    2,
                    3,
                    4,
                    5,
                    6,
                    7,
                    8,
                    130,
                    131,
                    132,
                    900_000,
                    900_001,
                    900_002,
                    1_500_000,
                    3_000_000_003,
                    3_000_000_010,
                    3_000_000_011,
                ],
            ),
            ("near_u32_max", vec![u32::MAX - 2, u32::MAX - 1, u32::MAX]),
        ]
    }

    /// ~100k-docid Zipf-shaped posting list: occurrences of the heaviest corpus term
    /// across documents (docids of documents containing term 0), strictly increasing by
    /// construction. Built once and cached.
    pub(crate) fn zipf_postings() -> &'static Vec<u32> {
        use std::sync::OnceLock;
        static ZIPF: OnceLock<Vec<u32>> = OnceLock::new();
        ZIPF.get_or_init(|| {
            let corpus = crate::corpus::generate(crate::corpus::CorpusConfig {
                seed: 2026_0823,
                docs: 320_000,
                avg_len: 3,
                vocab_size: 4096,
                zipf_s: 1.0,
            });
            let postings: Vec<u32> = corpus
                .docs
                .iter()
                .enumerate()
                .filter(|(_, doc)| doc.contains(&0))
                .map(|(docid, _)| docid as u32)
                .collect();
            assert!(
                (90_000..=130_000).contains(&postings.len()),
                "fixture drift: zipf postings length {}",
                postings.len()
            );
            drop(corpus);
            postings
        })
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn round_trip_matches_truth_on_all_shapes_and_encodings() {
        for (shape_name, docs) in shapes() {
            for (codec_name, kind) in kinds() {
                let bytes = encode(&kind, &docs);
                let mut reader = open(&kind, &bytes);
                assert_eq!(
                    reader.len(),
                    docs.len() as u32,
                    "{codec_name}/{shape_name} len"
                );
                assert_eq!(reader.pos(), 0);
                let seen = drain(&mut reader);
                assert_eq!(seen, docs, "{codec_name}/{shape_name} round trip");
                assert_eq!(reader.pos(), docs.len() as u32);
                assert!(
                    reader.peek().is_none(),
                    "{codec_name}/{shape_name} exhausted"
                );
            }
        }
    }

    #[test]
    fn zipf_shaped_list_round_trips_through_every_encoding() {
        let docs = zipf_postings();
        for (codec_name, kind) in kinds() {
            let bytes = encode(&kind, docs);
            let mut reader = open(&kind, &bytes);
            assert_eq!(reader.len(), docs.len() as u32, "{codec_name}");
            for (i, &doc) in docs.iter().enumerate() {
                assert_eq!(reader.peek(), Some(doc), "{codec_name} at {i}");
                assert_eq!(reader.pos(), i as u32, "{codec_name} pos at {i}");
                reader.next();
            }
            assert_eq!(reader.pos(), docs.len() as u32);
        }
    }

    #[test]
    fn advance_equals_binary_search_oracle_for_every_target_class() {
        // Target classes per shape: before-first, exactly-on-id, mid-gap/mid-block,
        // past-end. Oracle: first element >= target in the plain truth.
        let cases: Vec<(&'static str, Vec<u32>)> = vec![
            ("dense", (1000..=1127).collect()),
            (
                "mixed",
                vec![10, 11, 12, 500, 501, 700_000, 700_001, 700_002, 2_000_000],
            ),
            ("near_max", vec![u32::MAX - 40, u32::MAX - 3, u32::MAX]),
        ];
        for (shape_name, docs) in &cases {
            for (codec_name, kind) in kinds() {
                let bytes = encode(&kind, docs);
                let mut targets: Vec<u32> = vec![docs[0].saturating_sub(1)];
                targets.extend_from_slice(docs);
                for w in docs.windows(2) {
                    targets.push(w[0] + (w[1] - w[0]) / 2);
                }
                if docs[docs.len() - 1] != u32::MAX {
                    targets.push(docs[docs.len() - 1] + 1);
                }
                for &target in &targets {
                    let oracle = docs.iter().find(|&&d| d >= target).copied();
                    let mut reader = open(&kind, &bytes);
                    reader.advance(target);
                    assert_eq!(
                        reader.peek(),
                        oracle,
                        "{codec_name}/{shape_name} advance({target})"
                    );
                    assert_eq!(
                        reader.next(),
                        oracle,
                        "{codec_name}/{shape_name} next-after-advance({target})"
                    );
                }
            }
        }
    }

    #[test]
    fn plain_reader_contract_holds_including_empty_runs() {
        let mut empty = PlainReader::new(&[]);
        assert_eq!(empty.len(), 0);
        assert!(empty.peek().is_none());
        assert!(empty.next().is_none());
        assert!(empty.advance(42).is_none());

        let docs = [10u32, 20, 30];
        let mut plain = PlainReader::new(&docs);
        assert_eq!(plain.advance(15), Some(20)); // forward jump consumes 10
        assert_eq!(plain.advance(15), Some(20)); // target below frontier clamps to current
        assert_eq!(plain.advance(30), Some(30));
        assert!(plain.advance(u32::MAX - 1).is_none()); // past-end exhausts
        assert!(plain.advance(0).is_none()); // exhaustion is sticky
    }

    #[test]
    fn interleaved_next_and_advance_match_the_plain_oracle() {
        // Cursor-state transition coverage: forward-only advance between next() runs,
        // including zero-start (l=0), clusters, gaps, and exhaustion.
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
        for (name, kind) in kinds() {
            let bytes = encode(&kind, &docs);
            let mut reader = open(&kind, &bytes);
            let mut idx = 0usize;
            for &target in &targets {
                while idx < docs.len() && docs[idx] < target {
                    idx += 1;
                }
                assert_eq!(
                    reader.advance(target),
                    docs.get(idx).copied(),
                    "{name} advance({target})"
                );
                for _ in 0..2 {
                    let want = docs.get(idx).copied();
                    assert_eq!(reader.next(), want, "{name} next after {target}");
                    if want.is_some() {
                        idx += 1;
                    }
                }
            }
        }
    }

    #[test]
    fn truncated_headers_panic_at_construction() {
        let docs = vec![5, 6, 7, 800, 801, 802, 90_000];
        for (codec_name, kind) in kinds() {
            let bytes = encode(&kind, &docs);
            let head_cut = &bytes[..2];
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                open(&kind, head_cut);
            }));
            assert!(result.is_err(), "{codec_name} accepted a truncated header");
        }
    }

    #[test]
    fn truncated_bodies_panic_during_decoding() {
        // A long enough list that every header survives the half cut, so the panic must
        // come from the payload walk itself.
        let docs = &zipf_postings()[..4096];
        for (codec_name, kind) in kinds() {
            let full = encode(&kind, docs);
            let half = &full[..full.len() / 2];
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut reader = open(&kind, half);
                drain(&mut reader);
            }));
            assert!(result.is_err(), "{codec_name} decoded a truncated body");
        }
    }
}
