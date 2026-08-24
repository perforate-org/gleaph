//! Unit tests for stable-state lifecycle, search parity, and guard rails. Everything
//! runs on fresh in-memory regions (`VectorMemory`) — no PocketIC involvement.

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use ic_stable_structures::VectorMemory;
use ic_stable_text_postings::enc::{FreqVarintReader, PostingReader};

use super::*;
use crate::analyzer::analyze;

type TestStores = TextStores<VectorMemory>;
type TestMemories = TextMemories<VectorMemory>;

/// Twelve fresh, independent regions (stable structures never share memories).
fn fresh_regions() -> TestMemories {
    TestMemories {
        meta: VectorMemory::default(),
        segments: VectorMemory::default(),
        dict: VectorMemory::default(),
        postings: VectorMemory::default(),
        block_max: VectorMemory::default(),
        key_by_docid: VectorMemory::default(),
        docid_by_key: VectorMemory::default(),
        tombstones: VectorMemory::default(),
        stats: VectorMemory::default(),
        pending: VectorMemory::default(),
        merge_cursor: VectorMemory::default(),
        controller: VectorMemory::default(),
    }
}

/// Reopens a store over clones of previously used regions (shared backing bytes).
fn reopen(regions: &TestMemories) -> TestStores {
    let clone_all = || TestMemories {
        meta: regions.meta.clone(),
        segments: regions.segments.clone(),
        dict: regions.dict.clone(),
        postings: regions.postings.clone(),
        block_max: regions.block_max.clone(),
        key_by_docid: regions.key_by_docid.clone(),
        docid_by_key: regions.docid_by_key.clone(),
        tombstones: regions.tombstones.clone(),
        stats: regions.stats.clone(),
        pending: regions.pending.clone(),
        merge_cursor: regions.merge_cursor.clone(),
        controller: regions.controller.clone(),
    };
    TestStores::init(clone_all())
}

fn doc(key: u64, text: &str) -> TextDoc {
    TextDoc {
        key,
        text: text.to_string(),
    }
}

fn doc_keys(keys: &[u64]) -> Vec<u64> {
    keys.to_vec()
}

/// Applies the whole pending log in one call.
fn flush_all(stores: &mut TestStores) -> FlushReport {
    let mut report = stores.flush_step(u64::MAX);
    while !report.done {
        report = stores.flush_step(u64::MAX);
    }
    report
}

/// Naive live-doc scorer mirroring the v0 identity model (weight + tf per matched term),
/// ordered (score desc, docid asc) and truncated to k — the brute-force oracle.
fn brute_force(stores: &TestStores, query: &str, k: u32) -> Vec<TextHit> {
    let alive = |docid: u32| !stores.is_tombstoned(docid);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut scores: BTreeMap<u32, u32> = BTreeMap::new();
    for term in analyze(query) {
        if !seen.insert(term.clone()) {
            continue;
        }
        let Some(TermEntry { term_id, .. }) = stores.dict.get(&term) else {
            continue;
        };
        let Some(blob) = stores.postings.get(&term_id) else {
            continue;
        };
        let mut reader = FreqVarintReader::new(&blob);
        while let Some(docid) = reader.peek() {
            let tf = reader.freq().expect("interleaved tf");
            let consumed = reader.next().expect("just peeked");
            assert_eq!(docid, consumed);
            if alive(docid) {
                *scores.entry(docid).or_insert(0) += WEIGHT_BASE + tf;
            }
        }
    }
    let mut ranked: Vec<(u32, u32)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(k as usize);
    ranked
        .into_iter()
        .map(|(docid, score)| TextHit {
            key: stores.key_by_docid.get(&docid).expect("live docid has key"),
            docid,
            score,
        })
        .collect()
}

fn assert_search_parity(stores: &TestStores, query: &str, k: u32) {
    let got = stores.search(query, k).expect("search ok");
    let want = brute_force(stores, query, k);
    assert_eq!(got, want, "parity failed for {query:?} (k={k})");
}

/// Mixed-script corpus engineered for score ties, NFKC folding, CJK bigrams, and
/// duplicate-key updates. Fixture is built by the production analyzer only.
fn seed_corpus(stores: &mut TestStores) {
    stores
        .enqueue_ingest(vec![
            doc(101, "the red fox"),
            doc(102, "the blue fox"),
            doc(103, "red red fox"),
            doc(104, "fox"),
            doc(105, "東京都"),
            doc(106, "東京都"),
            doc(107, "ＦＵＬＬＴＥＸＴ fulltext"),
            doc(108, "東京 tower"),
            doc(109, "ｆｏｘ"),
            doc(110, "unrelated words entirely"),
        ])
        .expect("seed ingest");
    flush_all(stores);
}

#[test]
fn search_survives_whole_total_skip_when_heap_fills_below_block_max() {
    // Regression (plan 0294 slice 10): with UNSCALED block-max bounds, a heap filled by
    // ten score-2 hits made required=3 exceed total_bound=max-tf=2, and the whole-total
    // skip jumped every cursor past docid 128 — silently dropping higher-scored docs
    // inside the first logical block. The driver contract needs contribution-scaled
    // bounds (weight + max part), which `search` must supply.
    let mut stores = TextStores::init(fresh_regions());
    let mut docs = vec![doc(1, "東京")];
    for i in 1..=12 {
        docs.push(doc(i as u64 + 10, &format!("filler {i} 東京")));
    }
    // The tf-2 doc lands AFTER the heap has already filled with score-2 hits.
    docs.push(doc(999, "double 東京 東京 hit"));
    stores.enqueue_ingest(docs).expect("ingest");
    flush_all(&mut stores);

    let hits = stores.search("東京", 10).expect("search");
    assert_eq!(
        hits.first().expect("tf-2 doc must rank first").key,
        999,
        "whole-total skip must not drop the in-block high scorer"
    );
    assert_search_parity(&stores, "東京", 10);
}

#[test]
fn search_block_bounds_align_to_docid_blocks_on_sparse_lists() {
    // Regression (plan 0294 slice 10): tables built over POSITIONAL posting windows
    // misalign with the driver's docid-indexed bound lookups once posting lists are
    // sparse relative to the docid space — the lookup `docid / 128` can exceed the
    // table length and trap, or read a wrong block maximum. Docids 5 / 150 / 300 give
    // the rare term three postings across three docid blocks.
    let mut stores = TextStores::init(fresh_regions());
    let mut docs: Vec<TextDoc> = Vec::new();
    for i in 0..300u64 {
        let text = match i + 1 {
            5 => "rare token".to_string(),
            150 => "another rare".to_string(),
            300 => "third rare rare".to_string(),
            _ => format!("common filler {i}"),
        };
        docs.push(doc(i + 1, &text));
    }
    stores.enqueue_ingest(docs).expect("ingest");
    flush_all(&mut stores);

    let hits = stores.search("rare", 10).expect("sparse search");
    // Doc 300 carries tf 2 (score 3) and ranks first; the two tf-1 docs tie at
    // score 2 and order by ascending docid.
    let docids: Vec<u32> = hits.iter().map(|hit| hit.docid).collect();
    assert_eq!(docids, vec![300, 5, 150], "all three blocks must be served");
    assert_eq!(hits[0].score, WEIGHT_BASE + 2, "doc 300 carries tf 2");
}

#[test]
fn search_matches_brute_force_across_query_classes() {
    let mut stores = TextStores::init(fresh_regions());
    seed_corpus(&mut stores);

    // Pure-tie class: every "fox" posting carries tf 1 ⇒ ordering collapses to docid asc.
    assert_search_parity(&stores, "fox", 10);
    // Mixed scores: doc 103's double "red" outranks the tf-1 crowd.
    assert_search_parity(&stores, "red fox", 10);
    // CJK bigrams: identical docs 105/106 tie at docid asc, partial bigram doc 108 lower.
    assert_search_parity(&stores, "東京都", 10);
    assert_search_parity(&stores, "京都", 10);
    // NFKC-folded duplicates inside one document (tf 2).
    assert_search_parity(&stores, "fulltext", 10);
    // Truncation, clamping, dedupe, and misses.
    assert_search_parity(&stores, "fox", 3);
    assert_search_parity(&stores, "fox", u32::MAX);
    assert_search_parity(&stores, "fox fox FOX", 10);
    assert_search_parity(&stores, "zzz-not-present", 10);
    assert!(stores.search("", 10).expect("empty query").is_empty());
    assert!(stores.search("fox", 0).expect("k=0").is_empty());
}

#[test]
fn unflushed_ingest_is_under_posted_until_flush_steps_apply_it() {
    let mut stores = TextStores::init(fresh_regions());
    stores
        .enqueue_ingest(vec![doc(1, "alpha beta"), doc(2, "beta gamma")])
        .expect("enqueue");

    // Lag contract: durable log holds both ops, search sees nothing yet.
    assert_eq!(stores.get_stats().pending_ops, 2);
    assert!(stores.search("beta", 10).expect("search").is_empty());

    let mut steps = 0;
    loop {
        let report = stores.flush_step(1);
        steps += 1;
        if report.done {
            break;
        }
    }
    assert_eq!(steps, 2, "one op per bounded step, closing report included");
    assert_eq!(stores.search("beta", 10).expect("search").len(), 2);

    // FIFO application order fixes term-id assignment by arrival, not sorted insertion.
    let term_of = |unit: &str| {
        stores
            .dict
            .get(&unit.to_string())
            .expect("dict entry")
            .term_id
    };
    assert_eq!(term_of("alpha"), 0, "first applied op assigns term id 0");
    assert_eq!(term_of("gamma"), 2, "later applied ops assign higher ids");
}

/// True when `docid` is physically posted under `term_id` (regardless of tombstones).
fn list_contains(stores: &TestStores, term_id: u32, docid: u32) -> bool {
    let Some(blob) = stores.postings.get(&term_id) else {
        return false;
    };
    let mut reader = FreqVarintReader::new(&blob);
    while let Some(posted) = reader.peek() {
        if posted == docid {
            return true;
        }
        reader.next();
    }
    false
}

#[test]
fn delete_is_over_posted_until_merge_and_exact_after_reclaim() {
    let mut stores = TextStores::init(fresh_regions());
    seed_corpus(&mut stores);
    let fox_term_id = stores
        .dict
        .get(&"fox".to_string())
        .expect("fox in dict")
        .term_id;
    let doc_104_docid = stores.docid_by_key.get(&104).expect("doc 104 mapped");

    // Before delete: five live "fox" docs.
    assert_eq!(stores.search("fox", 10).expect("search").len(), 5);

    stores.enqueue_delete(doc_keys(&[104])).expect("delete");
    flush_all(&mut stores);

    // Logically excluded immediately...
    assert!(
        !stores
            .search("fox", 10)
            .expect("search")
            .iter()
            .any(|hit| hit.key == 104)
    );
    // ...but physically still posted (over-posted until merge).
    assert!(stores.is_tombstoned(doc_104_docid));
    assert_eq!(stores.get_stats().tombstoned_docs, 1);
    assert!(
        list_contains(&stores, fox_term_id, doc_104_docid),
        "tombstoned docid must remain physically posted before merge"
    );

    // Merge reclaims physically: bit cleared, blob shrunk, stats reconciled.
    let report = stores.merge_step(MAX_MERGE_TERMS_PER_STEP);
    assert!(report.done, "single call covers every term at this scale");
    assert_eq!(report.units_reclaimed, 1);
    assert_eq!(stores.get_stats().tombstoned_docs, 0);
    assert_eq!(stores.get_stats().ndocs, 9);
    assert!(
        !list_contains(&stores, fox_term_id, doc_104_docid),
        "reclaim must drop the tombstoned posting"
    );
    let fox_blob_len = stores
        .postings
        .get(&fox_term_id)
        .map(|blob| blob.len())
        .expect("fox postings survive with four live docs");
    // Exact physical shape: u32 count header + four postings × (single-byte docid delta +
    // single-byte tf) — all deltas and tfs fit in one byte here.
    assert_eq!(fox_blob_len, 4 + 4 * 2);

    // Results unchanged by physical reclaim.
    assert_search_parity(&stores, "fox", 10);
    assert_eq!(stores.search("fox", 10).expect("search").len(), 4);
}

#[test]
fn reingesting_a_key_tombstones_the_previous_incarnation() {
    let mut stores = TextStores::init(fresh_regions());
    stores
        .enqueue_ingest(vec![doc(200, "alpha beta")])
        .expect("v1");
    flush_all(&mut stores);
    stores
        .enqueue_ingest(vec![doc(200, "gamma")])
        .expect("v2 update");
    flush_all(&mut stores);

    let stats = stores.get_stats();
    assert_eq!(stats.ndocs, 1, "update keeps one live doc");
    assert_eq!(stats.next_docid, 2, "update allocated a fresh docid");
    assert_eq!(stats.tombstoned_docs, 1, "old incarnation tombstoned");

    assert!(stores.search("alpha", 10).expect("search").is_empty());
    assert!(stores.search("beta", 10).expect("search").is_empty());
    let hits = stores.search("gamma", 10).expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, 200);
}

#[test]
fn state_survives_reopen_round_trip_mid_log_and_post_merge() {
    let regions = fresh_regions();

    // Phase 1: ingest, flush partially, delete, leave a merge mid-pass.
    {
        let mut stores = reopen(&regions);
        stores
            .enqueue_ingest(vec![doc(1, "red fox"), doc(2, "blue fox")])
            .expect("ingest");
        stores.flush_step(1); // apply only the first op — reopen mid-log below
        stores
            .enqueue_delete(doc_keys(&[2]))
            .expect("delete enqueued");
        stores
            .enqueue_ingest(vec![doc(3, "red fish")])
            .expect("third");
        flush_all(&mut stores);
        stores.merge_step(1); // partial pass: cursor must persist
    }

    // Phase 2: finish the lifecycle over reopened bytes.
    {
        let mut stores = reopen(&regions);
        assert_eq!(
            stores.get_stats().pending_ops,
            0,
            "flush_all drained everything before the partial merge"
        );
        assert_eq!(
            stores.get_stats().next_docid,
            3,
            "monotonic counters survive reopen"
        );

        let mut report = stores.merge_step(1);
        while !report.done {
            report = stores.merge_step(1);
        }
        assert_search_parity(&stores, "red", 10);
        assert_search_parity(&stores, "fox fish", 10);
        let after = stores.get_stats();
        assert_eq!(after.tombstoned_docs, 0);
        assert_eq!(after.ndocs, 2, "doc 2 deleted, docs 1 and 3 live");
    }

    // Phase 3: byte-for-byte observable stability after the completed pass.
    {
        let stores = reopen(&regions);
        let stats = stores.get_stats();
        assert_eq!(stats.ndocs, 2);
        assert_eq!(
            stats.total_units, 4,
            "red+fox (2) and red+fish (2) stay posted; doc 2's units reclaimed"
        );
        assert_search_parity(&stores, "red fox fish", 10);
    }
}

#[test]
fn resumable_merge_converges_to_the_single_pass_result() {
    let build = |regions: &TestMemories| {
        let mut stores = reopen(regions);
        stores
            .enqueue_ingest(vec![
                doc(1, "shared alpha"),
                doc(2, "shared beta"),
                doc(3, "shared gamma"),
                doc(4, "shared delta"),
                doc(5, "shared epsilon"),
                doc(6, "solo"),
            ])
            .expect("ingest");
        flush_all(&mut stores);
        stores.enqueue_delete(doc_keys(&[1, 3])).expect("delete");
        flush_all(&mut stores);
        stores
    };

    let stepped_regions = fresh_regions();
    {
        let mut stores = build(&stepped_regions);
        let mut report = stores.merge_step(1);
        while !report.done {
            report = stores.merge_step(1);
        }
    }

    let single_regions = fresh_regions();
    {
        let mut stores = build(&single_regions);
        let report = stores.merge_step(MAX_MERGE_TERMS_PER_STEP);
        assert!(report.done);
    }

    let stepped = reopen(&stepped_regions);
    let single = reopen(&single_regions);
    assert_eq!(stepped.get_stats(), single.get_stats());
    for query in ["shared", "shared solo", "alpha beta gamma"] {
        assert_eq!(
            stepped.search(query, 10).expect("stepped search"),
            single.search(query, 10).expect("single-pass search"),
            "resume must converge: {query}"
        );
    }
}

#[test]
fn preflight_rejections_leave_the_pending_log_untouched() {
    let mut stores = TextStores::init(fresh_regions());
    stores
        .enqueue_ingest(vec![doc(1, "kept")])
        .expect("baseline append");

    let long_text = "x".repeat(MAX_TEXT_BYTES_PER_DOC + 1);
    let err = stores
        .enqueue_ingest(vec![doc(2, "fine"), doc(3, &long_text)])
        .expect_err("oversized doc must reject the whole batch");
    assert!(err.contains("MAX_TEXT_BYTES_PER_DOC"), "{err}");

    let big_batch: Vec<TextDoc> = (0..=MAX_DOCS_PER_INGEST)
        .map(|i| doc(i as u64, "tiny"))
        .collect();
    let err = stores
        .enqueue_ingest(big_batch)
        .expect_err("oversized batch must reject");
    assert!(err.contains("MAX_DOCS_PER_INGEST"), "{err}");

    // The baseline op survives untouched; no partial appends occurred.
    assert_eq!(stores.get_stats().pending_ops, 1);
    flush_all(&mut stores);
    assert_search_parity(&stores, "kept", 10);

    let err = stores
        .enqueue_delete((0..=MAX_KEYS_PER_DELETE as u64).collect())
        .expect_err("oversized delete batch must reject");
    assert!(err.contains("MAX_KEYS_PER_DELETE"), "{err}");
    assert_eq!(stores.get_stats().pending_ops, 0);
}

#[test]
fn search_guards_fail_closed() {
    let mut stores = TextStores::init(fresh_regions());
    seed_corpus(&mut stores);

    let long_query = " ".repeat(MAX_QUERY_BYTES + 1);
    let err = stores.search(&long_query, 10).expect_err("oversized query");
    assert!(err.contains("MAX_QUERY_BYTES"), "{err}");

    // k clamps instead of erroring; results stay driver-canonical.
    let hits = stores.search("fox", u32::MAX).expect("clamped search");
    assert!(hits.len() <= MAX_SEARCH_K as usize);
    assert_search_parity(&stores, "fox", MAX_SEARCH_K);
}

#[test]
fn layout_validation_rejects_foreign_bytes_fail_closed() {
    let regions = fresh_regions();

    // Foreign-but-decodable meta (wrong magic/version) must trap on open.
    {
        let mut foreign = Cell::init(regions.meta.clone(), TextMeta::default());
        foreign.set(TextMeta {
            magic: MAGIC ^ 1,
            ..TextMeta::default()
        });
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reopen(&regions)));
    assert!(result.is_err(), "foreign layout bytes must fail closed");
}

#[test]
fn controller_round_trip_and_anonymous_sentinel() {
    let regions = fresh_regions();
    let controller = Principal::from_slice(&[7, 7, 7]);
    {
        let mut stores = reopen(&regions);
        assert_eq!(
            stores.controller(),
            Principal::anonymous(),
            "unset controller defaults to the deny-all sentinel"
        );
        stores.set_controller(Some(controller));
    }
    // The configured principal survives reopen.
    assert_eq!(reopen(&regions).controller(), controller);
}
