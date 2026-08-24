//! Expanded-unit fixture: the D1 closing counterpart of the FTS5 arm's ingested bodies.
//!
//! **Expansion rule (identical to `text-index-fts5-arm`'s insertion rule).** Each
//! document's term sequence is mapped through [`corpus::expand_bigrams`] on the
//! vocabulary string: a CJK token becomes its overlapping character bigrams as separate
//! indexable units, an ASCII token passes through whole. The union of produced units
//! (expansions of vocabulary slots sampled by at least one document) is renumbered into a
//! dense unit-id space — slots ascending, units in production order, one id per distinct
//! string — and documents become unit-id streams over that space. Applying the same rule
//! to both arms keeps the FTS5 `unicode61` index and the custom inverted index
//! unit-identical, so ingest cost and storage bytes compare like for like.
//!
//! Heap-only: nothing here touches stable memory. The `bench_build_segment_m2000` bench
//! measures the physical inversion of these streams; this module only prepares them and
//! provides the storage-parity accounting over real encoder outputs.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Fixed corpus seed; identical to the custom arm's `CORPUS_SEED` and the FTS5 arm's
/// fixture seed, so equal-size corpora are byte-identical across arms.
const CORPUS_SEED: u64 = 2026_0823;

/// Number of documents; mirrors the FTS5 arm's `CORPUS_DOCS` (M=2000).
const CORPUS_DOCS: u32 = 2000;

/// Mean tokens per document; matches both arms.
const CORPUS_AVG_LEN: u32 = 24;

/// Vocabulary size upper bound; matches both arms.
const CORPUS_VOCAB: u32 = 2048;

/// Zipf skew exponent; matches both arms.
const CORPUS_ZIPF_S: f64 = 1.0;

/// The bigram-expanded M=2000 corpus.
pub(crate) struct ExpandedCorpus {
    /// Unit strings indexed by dense unit id.
    pub units: Vec<String>,
    /// Per-document unit-id sequences in document order.
    pub docs: Vec<Vec<u32>>,
}

/// Builds the expanded fixture described in the module docs.
fn build() -> ExpandedCorpus {
    let corpus = crate::corpus::generate(crate::corpus::CorpusConfig {
        seed: CORPUS_SEED,
        docs: CORPUS_DOCS,
        avg_len: CORPUS_AVG_LEN,
        vocab_size: CORPUS_VOCAB,
        zipf_s: CORPUS_ZIPF_S,
    });
    // Vocabulary slots sampled by at least one document: only their expansions are ever
    // produced, so only they contribute to the union.
    let mut sampled = vec![false; corpus.vocab.len()];
    for doc in &corpus.docs {
        for &term in doc {
            sampled[term as usize] = true;
        }
    }
    // Dense renumbered unit-id space over the union of produced units. Map lookups only,
    // never iteration, so ordering stays fully determined by slot and production order.
    let mut unit_ids: HashMap<String, u32> = HashMap::new();
    let mut term_units: Vec<Vec<u32>> = vec![Vec::new(); corpus.vocab.len()];
    let mut units: Vec<String> = Vec::new();
    for (slot, term) in corpus.vocab.iter().enumerate() {
        if !sampled[slot] {
            continue;
        }
        for unit in crate::corpus::expand_bigrams(term) {
            let next = units.len() as u32;
            let id = match unit_ids.get(unit.as_str()) {
                Some(&id) => id,
                None => {
                    unit_ids.insert(unit.clone(), next);
                    units.push(unit);
                    next
                }
            };
            term_units[slot].push(id);
        }
    }
    let docs = corpus
        .docs
        .iter()
        .map(|doc| {
            doc.iter()
                .flat_map(|&term| term_units[term as usize].iter().copied())
                .collect::<Vec<u32>>()
        })
        .collect();
    ExpandedCorpus { units, docs }
}

/// The expanded fixture, built once per process.
pub(crate) fn expanded_fixture() -> &'static ExpandedCorpus {
    static EXPANDED: OnceLock<ExpandedCorpus> = OnceLock::new();
    EXPANDED.get_or_init(build)
}

/// One inverted posting list: ascending docids with per-doc occurrence counts.
pub(crate) struct InvertedList {
    /// Sorted distinct docids containing the unit (dedupe within doc).
    pub docids: Vec<u32>,
    /// Occurrence counts aligned with [`InvertedList::docids`].
    pub tfs: Vec<u32>,
}

/// Physical inversion of the expanded streams: documents ascending, per-unit sorted
/// docid lists, repeated occurrences collapsed into within-doc tf counts.
pub(crate) fn invert(docs: &[Vec<u32>], unit_count: usize) -> Vec<InvertedList> {
    let mut lists: Vec<InvertedList> = (0..unit_count)
        .map(|_| InvertedList {
            docids: Vec::new(),
            tfs: Vec::new(),
        })
        .collect();
    let mut sorted: Vec<u32> = Vec::new();
    for (docid, stream) in docs.iter().enumerate() {
        sorted.clear();
        sorted.extend_from_slice(stream);
        sorted.sort_unstable();
        let mut start = 0usize;
        while start < sorted.len() {
            let unit = sorted[start];
            let mut end = start + 1;
            while end < sorted.len() && sorted[end] == unit {
                end += 1;
            }
            let list = &mut lists[unit as usize];
            list.docids.push(docid as u32);
            list.tfs.push((end - start) as u32);
            start = end;
        }
    }
    lists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blockmax::LOGICAL_BLOCK_SIZE;
    use crate::enc::{PostingReader, VarintReader, encode_varint};

    /// Logical DB image recorded by the FTS5 arm's `bench_fts5_ingest_m`:
    /// 23 × 16 KiB pages of the contentless FTS5 index over this same corpus rule.
    const FTS5_IMAGE_BYTES: usize = 376_832;

    /// Length-prefixed canonical form of the expanded fixture so equal expansions always
    /// compare equal byte-wise.
    fn canonical(expanded: &ExpandedCorpus) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(expanded.units.len() as u32).to_le_bytes());
        for unit in &expanded.units {
            bytes.extend_from_slice(&(unit.len() as u32).to_le_bytes());
            bytes.extend_from_slice(unit.as_bytes());
        }
        bytes.extend_from_slice(&(expanded.docs.len() as u32).to_le_bytes());
        for doc in &expanded.docs {
            bytes.extend_from_slice(&(doc.len() as u32).to_le_bytes());
            for id in doc {
                bytes.extend_from_slice(&id.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn expanded_fixture_is_deterministic_and_dense() {
        let a = build();
        let b = build();
        assert_eq!(canonical(&a), canonical(&b), "rebuild must be identical");
        assert_eq!(a.docs.len(), CORPUS_DOCS as usize);

        // Dense id space: every id in 0..units.len() occurs in some document.
        let mut seen = vec![false; a.units.len()];
        let mut max_id = 0usize;
        for doc in &a.docs {
            assert!(!doc.is_empty());
            for &id in doc {
                seen[id as usize] = true;
                max_id = max_id.max(id as usize);
            }
        }
        assert!(seen.iter().all(|&present| present), "ids must be dense");
        assert_eq!(max_id, a.units.len() - 1);

        // The expansion rule must actually fire: the mixed-script vocabulary yields more
        // units than sampled vocabulary slots (CJK bigrams split into extra units).
        assert!(
            a.units.len() > CORPUS_VOCAB as usize,
            "expected bigram expansion to enlarge the unit space, got {}",
            a.units.len()
        );
    }

    #[test]
    fn inversion_conserves_occurrences_and_stays_sorted_distinct() {
        let fx = expanded_fixture();
        let inverted = invert(&fx.docs, fx.units.len());
        assert_eq!(inverted.len(), fx.units.len());
        let streamed_total: usize = fx.docs.iter().map(Vec::len).sum();
        let inverted_total: usize = inverted
            .iter()
            .map(|list| list.tfs.iter().map(|&tf| tf as usize).sum::<usize>())
            .sum();
        assert_eq!(
            inverted_total, streamed_total,
            "inversion must conserve every expanded token instance"
        );
        for (unit_id, list) in inverted.iter().enumerate() {
            assert!(
                !list.docids.is_empty(),
                "unit {unit_id}: a produced unit cannot be absent"
            );
            assert_eq!(list.docids.len(), list.tfs.len());
            assert!(list.docids.windows(2).all(|w| w[0] < w[1]));
            assert!(list.tfs.iter().all(|&tf| tf >= 1));
        }
    }

    /// Pushes `value` as LEB128 u32 — same wire form as the set-mode varint deltas.
    fn push_leb128(out: &mut Vec<u8>, mut value: u32) {
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

    /// LEB128 byte width of `value`; the analytic cross-check for both modes.
    fn leb128_len(value: u32) -> usize {
        (32 - value.leading_zeros()).div_ceil(7).max(1) as usize
    }

    /// Freq-mode payload for one list: per posting, the docid delta as LEB128 (first
    /// delta = absolute docid, mirroring `encode_varint`) followed by the tf capped at
    /// 255 as one raw byte. This format exists only as a parity accounting definition;
    /// no production kernel implements it yet.
    fn encode_freq_mode(docids: &[u32], tfs: &[u32]) -> Vec<u8> {
        assert_eq!(docids.len(), tfs.len());
        assert!(!docids.is_empty());
        let mut out = Vec::new();
        let mut prev = 0u32;
        for (i, &docid) in docids.iter().enumerate() {
            let delta = if i == 0 { docid } else { docid - prev };
            push_leb128(&mut out, delta);
            out.push(tfs[i].min(u32::from(u8::MAX)) as u8);
            prev = docid;
        }
        out
    }

    /// Reads one LEB128 u32 starting at `*pos`, advancing it past the bytes.
    fn read_leb128_at(bytes: &[u8], pos: &mut usize) -> u32 {
        let mut result = 0u32;
        let mut shift = 0u32;
        loop {
            let byte = bytes[*pos];
            *pos += 1;
            result |= u32::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return result;
            }
            shift += 7;
        }
    }

    /// Decodes [`encode_freq_mode`] output back to (docids, tfs) for round-trip gates.
    fn decode_freq_mode(bytes: &[u8]) -> (Vec<u32>, Vec<u32>) {
        let mut pos = 0usize;
        let mut docids = Vec::new();
        let mut tfs = Vec::new();
        let mut prev = 0u32;
        while pos < bytes.len() {
            let delta = read_leb128_at(bytes, &mut pos);
            let docid = if docids.is_empty() {
                delta
            } else {
                prev + delta
            };
            prev = docid;
            docids.push(docid);
            let tf = bytes[pos];
            pos += 1;
            tfs.push(u32::from(tf));
        }
        (docids, tfs)
    }

    #[test]
    fn freq_mode_round_trips_caps_and_matches_width_formula() {
        let cases: [(&str, Vec<u32>, Vec<u32>); 4] = [
            ("singleton", vec![7], vec![1]),
            ("dense", (0..300).collect(), vec![255; 300]),
            (
                "gaps",
                vec![0, 130, 90_000, 3_000_000_003],
                vec![1, 2, 300, 4],
            ),
            ("cap_boundary", vec![1, 2, 3], vec![254, 255, 256]),
        ];
        for (name, docids, tfs) in cases {
            let bytes = encode_freq_mode(&docids, &tfs);
            let (got_docs, got_tfs) = decode_freq_mode(&bytes);
            assert_eq!(got_docs, docids, "{name} docids");
            let want_tfs: Vec<u32> = tfs.iter().map(|&tf| tf.min(u32::from(u8::MAX))).collect();
            assert_eq!(got_tfs, want_tfs, "{name} tfs (256 must cap to 255)");
            let analytic: usize = docids
                .iter()
                .enumerate()
                .map(|(i, &d)| {
                    let delta = if i == 0 { d } else { d - docids[i - 1] };
                    leb128_len(delta) + 1
                })
                .sum();
            assert_eq!(bytes.len(), analytic, "{name} width formula");
        }
    }

    #[test]
    fn storage_parity_full_corpus() {
        let fx = expanded_fixture();
        let inverted = invert(&fx.docs, fx.units.len());
        let unit_count = fx.units.len();

        let mut postings_set = 0usize; // set-mode entries: distinct (unit, docid)
        let mut postings_tf = 0usize; // freq-mode entries: every occurrence
        let mut set_bytes = 0usize; // Σ real set-mode encoder outputs
        let mut freq_bytes = 0usize; // Σ real freq-mode writer outputs
        for list in &inverted {
            postings_set += list.docids.len();
            postings_tf += list.tfs.iter().map(|&tf| tf as usize).sum::<usize>();

            // Set mode: real encoder output, gated by a full round-trip before sizing.
            let encoded = encode_varint(&list.docids);
            let mut reader = VarintReader::new(&encoded);
            for &docid in &list.docids {
                assert_eq!(reader.next(), Some(docid), "set round trip");
            }
            assert!(reader.next().is_none(), "set round trip exhausted");
            set_bytes += encoded.len();

            // Freq mode: real writer output, gated by decode equality.
            let encoded_freq = encode_freq_mode(&list.docids, &list.tfs);
            let (decoded_docs, decoded_tfs) = decode_freq_mode(&encoded_freq);
            let capped: Vec<u32> = list
                .tfs
                .iter()
                .map(|&tf| tf.min(u32::from(u8::MAX)))
                .collect();
            assert_eq!(decoded_docs, list.docids, "freq round trip docids");
            assert_eq!(decoded_tfs, capped, "freq round trip tfs");
            freq_bytes += encoded_freq.len();
        }

        // Flat-layout dictionary assumption: unit strings verbatim plus 8 B key +
        // 8 B value per entry; no B-tree node or page overhead is modeled.
        let dict_bytes: usize =
            fx.units.iter().map(|unit| unit.len()).sum::<usize>() + 8 * unit_count;
        // One u32 block-max entry per 128-docid logical block per unit.
        let blockmax_bytes: usize = inverted
            .iter()
            .map(|list| {
                list.docids
                    .len()
                    .div_ceil(usize::try_from(LOGICAL_BLOCK_SIZE).expect("small constant"))
                    * 4
            })
            .sum();

        let total_set = set_bytes + dict_bytes + blockmax_bytes;
        let total_freq = freq_bytes + dict_bytes + blockmax_bytes;

        println!("PARITY units={unit_count} postings_set={postings_set} postings_tf={postings_tf}");
        println!(
            "PARITY set_bytes={set_bytes} freq_bytes={freq_bytes} dict_bytes={dict_bytes} blockmax_bytes={blockmax_bytes}"
        );
        println!("PARITY total_set={total_set} total_freq={total_freq}");

        // Parity invariant: totals recomputed two ways agree. Analytic widths from the
        // delta sequence must reproduce both encoder-output sums exactly. Set mode uses
        // the real encoder output, which includes a 4-byte count header per list; freq
        // mode follows the brief's bare interleaved layout (delta varint + tf byte, no
        // header) — the 4·units byte difference is part of each mode's definition.
        let analytic_set: usize = inverted
            .iter()
            .map(|list| {
                4 // varint count header
                    + list
                        .docids
                        .iter()
                        .enumerate()
                        .map(|(i, &d)| {
                            let delta = if i == 0 { d } else { d - list.docids[i - 1] };
                            leb128_len(delta)
                        })
                        .sum::<usize>()
            })
            .sum();
        assert_eq!(
            analytic_set, set_bytes,
            "set bytes: encoder vs analytic widths"
        );
        let analytic_freq: usize = inverted
            .iter()
            .map(|list| {
                list.tfs.len() // +1 tf byte per posting
                    + list
                        .docids
                        .iter()
                        .enumerate()
                        .map(|(i, &d)| {
                            let delta = if i == 0 { d } else { d - list.docids[i - 1] };
                            leb128_len(delta)
                        })
                        .sum::<usize>()
            })
            .sum();
        assert_eq!(
            analytic_freq, freq_bytes,
            "freq bytes: writer vs analytic widths"
        );

        let set_pct = total_set as f64 / FTS5_IMAGE_BYTES as f64 * 100.0;
        let freq_pct = total_freq as f64 / FTS5_IMAGE_BYTES as f64 * 100.0;
        println!(
            "PARITY vs_fts5_image fts5_bytes={FTS5_IMAGE_BYTES} set_pct={set_pct:.2} freq_pct={freq_pct:.2}"
        );

        assert!(total_set > 0);
        assert!(
            total_freq >= total_set,
            "tf bytes only ever add to the total"
        );
    }
}
