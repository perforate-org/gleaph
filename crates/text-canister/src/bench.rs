//! Whole-path term-search measurement for the Text Index canister (plan 0295
//! `query-path-and-bench`; plan 0296 adds the unscored matrix half).
//!
//! Run from `crates/text-canister`: `canbench query_term` (see `canbench.yml`).
//! The measured closure is exactly ONE full [`crate::state::TextStores::search`] call —
//! production analyzer → dictionary hash probe → postings slab fetch → DAAT driver with
//! inline skip data and lazy tf scoring → hit projection — making it the direct
//! counterpart of the FTS5 arm's whole-path number. Fixture construction (corpus
//! generation, analyzer-driven ingest through the durable pending log, bounded flushes)
//! and the brute-force correctness gate all happen outside `bench_fn`.
//!
//! Fair-pair matrix (plan 0296): [`bench_query_term_top100`] is the scored half,
//! `bench_query_term_top100_unscored` the unscored half (same fixture term; first-100
//! live docids without tf→part scoring) — together with the FTS5 arm's rowid lookup and
//! bm25-ranked top-100 these complete the 2×2 unscored/scored × custom/FTS5 comparison.
//!
//! Determinism: the corpus comes from the shared fixture family (fixed seed via
//! `ic_stable_text_postings::corpus`); the analyzer is the deterministic production
//! pipeline; no clocks, no hash iteration.

use std::hint::black_box;
use std::sync::OnceLock;

use canbench_rs::bench;
use ic_stable_text_postings::enc::{FreqVarintReader, PostingReader};

use crate::state::{Memory, TextStores, WEIGHT_BASE, with_stores};
use crate::{TextDoc, TextHit};

/// Fixture family seed (same lineage as the D1 comparison arms).
const CORPUS_SEED: u64 = 2026_0823;
/// Corpus size: large enough that the rank-0 Zipf term crosses df ≥ [`DENSE_DF_MIN`].
const CORPUS_DOCS: u32 = 10_000;
const CORPUS_AVG_LEN: u32 = 24;
const CORPUS_VOCAB: u32 = 2048;
const CORPUS_ZIPF_S: f64 = 1.0;

/// Top-k requested by the measured query (the workload's headline parameter).
const TOP_K: u32 = 100;

/// The dense fixture term must reach this document frequency.
const DENSE_DF_MIN: usize = 1_000;

struct Fixture {
    /// The dense query term (rank-0 vocabulary token), analyzed verbatim.
    query: String,
    /// Brute-force top-k truth over live stored postings.
    truth: Vec<TextHit>,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture)
}

/// Builds the fixture outside every measured closure: generate the corpus, render it to
/// text, run it through the production analyzer at enqueue time (the only analysis), apply
/// the whole pending log in bounded flush steps, then verify search against a naive
/// scorer over the same stored docs.
fn build_fixture() -> Fixture {
    let corpus = ic_stable_text_postings::corpus::generate(ic_stable_text_postings::CorpusConfig {
        seed: CORPUS_SEED,
        docs: CORPUS_DOCS,
        avg_len: CORPUS_AVG_LEN,
        vocab_size: CORPUS_VOCAB,
        zipf_s: CORPUS_ZIPF_S,
    });
    let dense_query = corpus.vocab[0].clone();
    let docs: Vec<TextDoc> = corpus
        .docs
        .iter()
        .enumerate()
        .map(|(i, doc)| TextDoc {
            key: i as u64 + 1,
            text: doc
                .iter()
                .map(|&token| corpus.vocab[token as usize].as_str())
                .collect::<Vec<_>>()
                .join(" "),
        })
        .collect();
    drop(corpus);

    with_stores(|stores| {
        // Bounded ingest batches; any preflight violation rejects the batch loudly.
        for batch in docs.chunks(crate::state::MAX_DOCS_PER_INGEST) {
            stores.enqueue_ingest(batch.to_vec()).expect("ingest batch");
        }
        loop {
            let report = stores.flush_step(u64::MAX);
            if report.done {
                break;
            }
        }

        let truth = brute_force_topk(stores, &dense_query, TOP_K);
        let hits = truth.len();
        assert!(
            hits == TOP_K as usize,
            "dense fixture must fill the top-{TOP_K} heap, got {hits}"
        );
        // The dense term's document frequency equals its full posting list length.
        let df = stored_postings(stores, &dense_query).len();
        assert!(df >= DENSE_DF_MIN, "dense df {df} below {DENSE_DF_MIN}");
        // End-to-end gate: the real search path must reproduce brute force exactly
        // before any measurement happens.
        assert_eq!(
            stores.search(&dense_query, TOP_K).expect("search"),
            truth,
            "search must equal brute force before measurement"
        );

        Fixture {
            query: dense_query,
            truth,
        }
    })
}

/// Naive scorer over stored state: decode the term's full posting list, keep live
/// docids scored `WEIGHT_BASE + tf`, order (score desc, docid asc), truncate to k, and
/// project keys — the identity-model oracle of the lazy driver.
fn brute_force_topk(stores: &mut TextStores<Memory>, term: &str, k: u32) -> Vec<TextHit> {
    let mut scored: Vec<(u32, u32)> = stored_postings(stores, term)
        .into_iter()
        .filter(|&(docid, _)| !stores.is_tombstoned(docid))
        .map(|(docid, tf)| (docid, WEIGHT_BASE + tf))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(k as usize);
    scored
        .into_iter()
        .map(|(docid, score)| TextHit {
            key: stores.key_of_docid(docid).expect("live docid has key"),
            docid,
            score,
        })
        .collect()
}

/// Decodes one term's full stored posting list as `(docid, tf)` pairs.
fn stored_postings(stores: &TextStores<Memory>, term: &str) -> Vec<(u32, u32)> {
    let Some(term_id) = stores.dict_term_id(term) else {
        return Vec::new();
    };
    let Some(blob) = stores.postings_blob(term_id) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut reader = FreqVarintReader::new(&blob);
    while let Some(docid) = reader.peek() {
        let tf = reader.freq().expect("interleaved tf");
        out.push((docid, tf));
        reader.next();
    }
    out
}

// -- bench ---------------------------------------------------------------------------------

/// Whole-path term-search: ONE full `search()` call over the production analyzer path —
/// analyzer → dict hash probe → postings slab fetch → DAAT/block-max driver (inline
/// bi-level skips, lazy tf scoring) → hit projection — on a corpus whose dense term has
/// df ≥ 1000 plus Zipf tail terms. Setup verifies the result against a brute-force
/// scorer over the same docs once before measuring.
///
/// **Predeclared comparison:** FTS5 arm whole-path = 261 K instructions; goal is
/// same-order-or-better. Informational gate, not pass/fail.
#[bench(raw)]
fn bench_query_term_top100() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    let query = black_box(f.query.clone());
    let truth = black_box(&f.truth);
    canbench_rs::bench_fn(move || {
        let hits = with_stores(|stores| stores.search(&query, TOP_K).expect("search"));
        black_box(hits);
        black_box(truth);
    })
}

/// Unscored half of the 2×2 fair-pair matrix (plan 0296): the first [`TOP_K`] live
/// docids of the SAME dense fixture term and corpus config — production analyzer →
/// dictionary probe → postings fetch → fused codec stepping with memoized tombstone
/// filtering (bulk dead-range jumps included) → key projection — but NO tf→part table
/// and no ranking driver. Workload counterpart of the FTS5 arm's unscored
/// `SELECT rowid ... LIMIT 100` rowid lookup; the scored sibling above completes the
/// matrix on this side.
///
/// Gate before measurement: the walk must reproduce first-100-live-docids truth
/// (stored postings, tombstones filtered, docid ascending) computed independently.
#[bench(raw)]
fn bench_query_term_top100_unscored() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    let expected: Vec<(u32, u64)> = with_stores(|stores| {
        stored_postings(stores, &f.query)
            .into_iter()
            .filter(|&(docid, _)| !stores.is_tombstoned(docid))
            .take(TOP_K as usize)
            .map(|(docid, _)| {
                (
                    docid,
                    stores.key_of_docid(docid).expect("live docid has key"),
                )
            })
            .collect()
    });
    assert_eq!(
        expected.len(),
        TOP_K as usize,
        "dense fixture must fill the unscored window"
    );
    let query = black_box(f.query.clone());
    // Correctness gate outside the measured closure.
    let probe = with_stores(|stores| stores.first_live_docids(&query, TOP_K));
    assert_eq!(
        probe, expected,
        "unscored walk must equal the first-100-live-docids truth"
    );
    let truth = black_box(expected);
    canbench_rs::bench_fn(move || {
        let hits = with_stores(|stores| stores.first_live_docids(&query, TOP_K));
        black_box(hits);
        black_box(truth);
    })
}
