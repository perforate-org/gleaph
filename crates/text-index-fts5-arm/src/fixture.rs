//! Deterministic FTS5-arm fixture derived from the shared corpus generator.
//!
//! **Document rule (tokenizer fairness).** Every vocabulary term is mapped through
//! [`ic_stable_text_postings::corpus::expand_bigrams`]: a CJK token becomes its overlapping
//! character bigrams joined by single spaces (`"あいうえ"` → `"あい いう うえ"`), and a
//! non-CJK token passes through unchanged. A document body is its token sequence expanded
//! term-by-term and joined by single spaces. Feeding FTS5 these pre-bigrammed bodies makes
//! its `unicode61` index contain exactly the same units as the custom arm's inverted index
//! (default `unicode61` treats a contiguous CJK run as one token — spike finding, so the
//! raw tokens would be unfair to both sides).
//!
//! Probe ranks sit on even vocabulary slots (ASCII words; the generator alternates
//! ASCII/Japanese by slot parity), which keeps probe `MATCH` semantics at the whole-token
//! level: for ASCII terms the FTS5 hit count is exactly the document frequency of the
//! corpus term, with no phrase-query machinery involved.

use ic_stable_text_postings::corpus::{Corpus, CorpusConfig, expand_bigrams, generate};

/// Fixed corpus seed behind every derived fixture; identical value to the custom arm's
/// `CORPUS_SEED`, so equal-size corpora are byte-identical across arms.
pub const CORPUS_SEED: u64 = 2026_0823;

/// Number of documents M ingested by every bench. Brief default 2000; may be adjusted only
/// within 1000..=4000 if instrumentation runtime forces it, and the actual M must be
/// reported alongside the numbers.
pub const CORPUS_DOCS: u32 = 2000;

/// Mean tokens per document; matches the custom arm.
pub const CORPUS_AVG_LEN: u32 = 24;

/// Vocabulary size upper bound; matches the custom arm.
pub const CORPUS_VOCAB: u32 = 2048;

/// Zipf skew exponent; matches the custom arm.
pub const CORPUS_ZIPF_S: f64 = 1.0;

/// Dense probe rank ≈ A band (rank 0 is the heaviest Zipf term).
const PROBE_RANK_DENSE: usize = 0;

/// Mid probe rank ≈ B/C band. Even slot (ASCII) neighbor of the custom arm's rank 83.
const PROBE_RANK_MID: usize = 84;

/// Tail probe rank ≈ D band; same rank as the custom arm's small list D.
const PROBE_RANK_TAIL: usize = 1000;

/// The three probe terms, dense → tail: one per df band used by the verifier.
pub const PROBE_RANKS: [usize; 3] = [PROBE_RANK_DENSE, PROBE_RANK_MID, PROBE_RANK_TAIL];

/// The four custom-arm benchmark lists (A/B/C/D ranks) whose varint bytes form the honest
/// storage counterpart for the ingest stable-memory number.
pub const POSTING_RANKS: [usize; 4] = [0, 53, 83, 1000];

/// Generates the shared corpus fixture.
pub fn corpus() -> Corpus {
    generate(CorpusConfig {
        seed: CORPUS_SEED,
        docs: CORPUS_DOCS,
        avg_len: CORPUS_AVG_LEN,
        vocab_size: CORPUS_VOCAB,
        zipf_s: CORPUS_ZIPF_S,
    })
}

/// Per-vocabulary-term index units after the bigram rule (see module docs).
pub fn vocab_units(corpus: &Corpus) -> Vec<Vec<String>> {
    corpus
        .vocab
        .iter()
        .map(|term| expand_bigrams(term))
        .collect()
}

/// Materializes every document body in rowid order (rowid = docid + 1) under the document
/// rule in the module docs. This is fixture setup work and must stay outside all measured
/// closures.
pub fn doc_bodies(corpus: &Corpus) -> Vec<String> {
    let units = vocab_units(corpus);
    corpus
        .docs
        .iter()
        .map(|doc| {
            doc.iter()
                .map(|&id| units[id as usize].join(" "))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Brute-force expected document frequency of a vocabulary rank: distinct docs containing
/// the term at least once.
pub fn expected_df(corpus: &Corpus, rank: usize) -> u32 {
    let id = rank as u32;
    corpus.docs.iter().filter(|doc| doc.contains(&id)).count() as u32
}

/// Sorted distinct posting list (docids) for a vocabulary rank: the exact bytes whose
/// varint encoding sizes the custom arm's storage counterpart.
pub fn posting_list(corpus: &Corpus, rank: usize) -> Vec<u32> {
    let id = rank as u32;
    let mut out: Vec<u32> = corpus
        .docs
        .iter()
        .enumerate()
        .filter(|(_, doc)| doc.contains(&id))
        .map(|(docid, _)| docid as u32)
        .collect();
    // Docs arrive ascending and contain each id at most once in the dedup sense below;
    // sort + dedup keeps the contract independent of that incidental ordering.
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_text_postings::enc::{PostingReader, VarintReader, encode_varint};

    #[test]
    fn fixture_stats_match_the_contract() {
        let corpus = corpus();
        assert_eq!(corpus.docs.len(), CORPUS_DOCS as usize);
        assert!(corpus.vocab.len() <= CORPUS_VOCAB as usize);
        // Both scripts must be present so the bigram rule actually matters here.
        assert!(corpus.vocab.iter().any(|t| expand_bigrams(t).len() > 1));
        assert!(
            corpus.vocab.iter().any(|t| expand_bigrams(t).len() == 1),
            "expected ASCII (single-unit) terms too"
        );
        let total: usize = corpus.docs.iter().map(Vec::len).sum();
        let target = CORPUS_DOCS as usize * CORPUS_AVG_LEN as usize;
        assert!(
            total.abs_diff(target) <= target * 15 / 100,
            "total tokens {total} outside ±15% of {target}"
        );
        for doc in &corpus.docs {
            assert!(!doc.is_empty());
        }
    }

    #[test]
    fn probe_rands_hit_the_three_bands_and_stay_whole_token() {
        let corpus = corpus();
        let dfs: Vec<u32> = PROBE_RANKS
            .iter()
            .map(|&r| expected_df(&corpus, r))
            .collect();
        println!("probe dfs (seed {CORPUS_SEED}, M={CORPUS_DOCS}): {dfs:?}");
        // Loose sanity bands; the values are deterministic for the fixed seed.
        assert!(dfs[0] > 1_500, "dense probe must approach full coverage");
        assert!((30..=200).contains(&dfs[1]), "mid probe out of band");
        assert!((2..=30).contains(&dfs[2]), "tail probe out of band");
        for &rank in &PROBE_RANKS {
            let units = expand_bigrams(&corpus.vocab[rank]);
            assert_eq!(units.len(), 1, "probe {rank} must stay a single token");
            assert_eq!(units[0], corpus.vocab[rank]);
        }
    }

    /// Storage counterpart for the FTS5 ingest SMI number: delta-varint-encodes the four
    /// custom-arm posting lists over this same fixture and asserts round-trip integrity
    /// before reporting the byte totals.
    #[test]
    fn storage_counterpart_varint_encoded_bytes() {
        let corpus = corpus();
        let mut total = 0usize;
        for &rank in &POSTING_RANKS {
            let list = posting_list(&corpus, rank);
            let encoded = encode_varint(&list);
            // Round-trip correctness gate before trusting any size number.
            let mut reader = VarintReader::new(&encoded);
            let mut walked = Vec::with_capacity(list.len());
            while let Some(docid) = reader.next() {
                walked.push(docid);
            }
            assert_eq!(walked, list, "rank {rank}: varint round-trip must hold");
            println!(
                "storage counterpart rank {rank}: df={} varint_bytes={}",
                list.len(),
                encoded.len()
            );
            total += encoded.len();
        }
        println!("TOTAL custom-arm encoded bytes (seed {CORPUS_SEED}, M={CORPUS_DOCS}): {total}");
        assert!(total > 0);
    }

    #[test]
    fn posting_lists_match_brute_force_dfs() {
        let corpus = corpus();
        for &rank in &POSTING_RANKS {
            assert_eq!(
                posting_list(&corpus, rank).len(),
                expected_df(&corpus, rank) as usize
            );
        }
    }
}
