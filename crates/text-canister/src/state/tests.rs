//! Unit tests for stable-state lifecycle, search parity, and guard rails. Everything
//! runs on fresh in-memory regions (`VectorMemory`) — no PocketIC involvement.

use std::collections::{BTreeMap, BTreeSet};

use candid::Principal;
use ic_stable_structures::VectorMemory;
use ic_stable_text_postings::blockmax::LOGICAL_BLOCK_SIZE;
use ic_stable_text_postings::enc::{FreqVarintReader, PostingReader};
use ic_stable_text_postings::topk::TfPartTable;

use super::arena::ARENA_CHUNK_BYTES;
use super::*;
use crate::analyzer::analyze;

type TestStores = TextStores<VectorMemory>;
type TestMemories = TextMemories<VectorMemory>;

/// Fourteen fresh, independent regions (stable structures never share memories).
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
        arena: VectorMemory::default(),
        term_entries: VectorMemory::default(),
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
        arena: regions.arena.clone(),
        term_entries: regions.term_entries.clone(),
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
        let Some(term_id) = stores.dict_term_id(&term) else {
            continue;
        };
        let Some(blob) = stores.postings_blob(term_id) else {
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
            key: stores.key_of_docid(docid).expect("live docid has key"),
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
    let term_of = |unit: &str| stores.dict_term_id(unit).expect("dict entry");
    assert_eq!(term_of("alpha"), 0, "first applied op assigns term id 0");
    assert_eq!(term_of("gamma"), 2, "later applied ops assign higher ids");
}

/// True when `docid` is physically posted under `term_id` (regardless of tombstones).
fn list_contains(stores: &TestStores, term_id: u32, docid: u32) -> bool {
    let Some(blob) = stores.postings_blob(term_id) else {
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
    let fox_term_id = stores.dict_term_id("fox").expect("fox in dict");
    let doc_104_docid = stores.docid_of_key(104).expect("doc 104 mapped");

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
        .postings_blob(fox_term_id)
        .map(|blob| blob.len())
        .expect("fox postings survive with four live docs");
    // Exact physical shape: u32 count header + four postings × (single-byte docid delta
    // + single-byte tf) — all deltas and tfs fit in one byte here — plus the layout-3
    // bi-level skip trailer for a one-block list (one level-0 entry 12 B + one level-1
    // entry 12 B + trailing count word 4 B).
    assert_eq!(fox_blob_len, 4 + 4 * 2 + 12 + 12 + 4);

    // Results unchanged by physical reclaim.
    assert_search_parity(&stores, "fox", 10);
    assert_eq!(stores.search("fox", 10).expect("search").len(), 4);
}

/// Adversarial gate for plan 0296 bulk dead-range skipping: clustered tombstone runs
/// straddle logical-block boundaries (128-doc edges at 128/256/384/512/640/768) while
/// the filtered driver must expose EXACTLY the alive subsequence the brute-force filter
/// produces. The hot-path counters prove the bulk path actually engaged and that no
/// stored posting is classified more than once (the pre-plan-0296 wrapper re-tested the
/// live frontier through its alive closure on every accessor — up to three times per
/// posting on the consume path alone).
#[test]
fn search_matches_filtering_oracle_on_block_straddling_tombstone_clusters() {
    let mut stores = TextStores::init(fresh_regions());
    // Keys arrive ascending and docids are sequential, so docid == key here.
    let docs: Vec<TextDoc> = (1..=900u64)
        .map(|i| doc(i, &format!("x filler {i}")))
        .collect();
    stores.enqueue_ingest(docs).expect("ingest");
    flush_all(&mut stores);

    // Dead runs: a 171-wide cluster crossing the 128 edge, a 201-wide cluster crossing
    // the 256 AND 384 edges — both starting inside the first ~80 live candidates so the
    // heap-fill walk enters each AT ITS HEAD and must bulk-jump — plus a two-doc sliver
    // ON the 768 edge and a tail run ending past the last posting. The sliver and the
    // tail are met mid-run by whole-total sweep landings, exercising the short-run
    // linear arm on sub-block residuals (by design: linear wins inside one block).
    let deleted: Vec<u64> = (20..=190)
        .chain(250..=450)
        .chain(767..=768)
        .chain(800..=900)
        .collect();
    stores.enqueue_delete(doc_keys(&deleted)).expect("delete");
    flush_all(&mut stores);
    let live = 900 - deleted.len();
    assert_eq!(live, 425);

    // Equivalence with the filtering oracle across truncation widths (search clamps k
    // at MAX_SEARCH_K=100, so parity runs at that width too).
    assert_search_parity(&stores, "x", 10);
    assert_search_parity(&stores, "x", 100);
    assert_eq!(
        stores.search("x", u32::MAX).expect("search").len(),
        100,
        "live postings must still fill the clamped top-100"
    );

    // Stored-list baseline for the decomposition report: a bare (unfiltered) fused
    // sweep visits exactly one posting per stored entry.
    let term_id = stores.dict_term_id("x").expect("dict");
    let blob = stores.postings_blob(term_id).expect("postings");
    let stored_df = FreqVarintReader::new(&blob).len();
    let mut bare = FreqVarintReader::new(&blob);
    let bare_steps = std::iter::from_fn(|| bare.next_step()).count() as u32;
    assert_eq!(
        bare_steps, stored_df,
        "bare sweep visits every stored posting"
    );

    driver_counters::reset();
    let _ = stores.search("x", u32::MAX).expect("search");
    let snap = driver_counters::snapshot();
    println!(
        "decomposition: bare_steps={stored_df} visible={} dead_linear={} jumps={} filter_tests={}",
        snap.visible_steps, snap.dead_linear_steps, snap.block_jumps, snap.filter_tests
    );
    // Uniform scores (tf=1 everywhere) fill the heap after exactly 100 evaluated
    // candidates; whole-total pruning sweeps the rest without consuming further ones.
    assert_eq!(
        snap.visible_steps, 100,
        "heap-fill candidates are the only consumed postings"
    );
    assert!(
        snap.block_jumps >= 2,
        "both block-straddling clusters must jump via the skip trailer"
    );
    assert!(
        snap.dead_linear_steps > 0 && snap.dead_linear_steps <= 103,
        "edge sliver + reachable tail-run suffix consumed linearly, never the wide clusters"
    );
    assert!(
        u64::from(stored_df) >= snap.filter_tests,
        "each stored posting may be classified at most once (verdict memoization)"
    );

    // Physical reclaim collapses the runs; parity must hold through the merge too.
    while !stores.merge_step(MAX_MERGE_TERMS_PER_STEP).done {}
    assert_search_parity(&stores, "x", 100);
}

/// The `next_alive: None` verdict (every remaining docid of a container tombstoned)
/// must drain the reader to exhaustion instead of looping or resurfacing dead postings.
#[test]
fn fully_tombstoned_container_exhausts_without_resurfacing_dead_postings() {
    let mut stores = TextStores::init(fresh_regions());
    stores
        .enqueue_ingest(vec![doc(1, "z"), doc(2, "z z"), doc(3, "z")])
        .expect("ingest");
    flush_all(&mut stores);

    // Mark EVERY docid of container 0 tombstoned — a superset of the posted docids.
    let mut container = Tombstone::default();
    for docid in 0..65_536u32 {
        container.set(docid);
    }
    if stores.tombstones.is_empty() {
        stores.tombstones.push(&container);
    } else {
        stores.tombstones.set(0, &container);
    }

    assert!(stores.search("z", 10).expect("search").is_empty());
    assert_search_parity(&stores, "z", 10);
}

/// The unscored traversal window (plan 0296 fair-pair bench primitive): first `limit`
/// live docids with keys, tombstones excluded, unknown terms empty, over-truncation safe.
#[test]
fn first_live_docids_matches_first_live_prefix_and_handles_limits() {
    let mut stores = TextStores::init(fresh_regions());
    seed_corpus(&mut stores);

    // "fox" posts in five docs; the first three live docids ascend with their keys.
    let fox = stores.first_live_docids("fox", 3);
    assert_eq!(
        fox,
        vec![(1, 101), (2, 102), (3, 103)],
        "docid==key here: sequential ingest"
    );
    // Over-wide limit truncates at exhaustion; unknown term is empty.
    assert_eq!(stores.first_live_docids("fox", u32::MAX).len(), 5);
    assert!(stores.first_live_docids("zzz-not-present", 10).is_empty());

    // Delete two fox docs: they leave the window immediately (tombstone filter), and
    // the next-live docids slide up without any physical reclaim.
    stores.enqueue_delete(doc_keys(&[102, 104])).expect("del");
    flush_all(&mut stores);
    assert_eq!(
        stores.first_live_docids("fox", 3),
        vec![(1, 101), (3, 103), (9, 109)]
    );
}

#[test]
fn first_clear_bit_scans_past_runs_and_reports_container_end() {
    let mut bits = [0u8; TOMBSTONE_CONTAINER_BYTES];
    bits[0] = 0b0001_0000; // only bit 4 set
    assert_eq!(first_clear_bit(&bits, 4), Some(5));
    // A run of set bits crossing a byte boundary clears right after its end.
    for b in 6..=12u32 {
        bits[(b / 8) as usize] |= 1 << (b % 8);
    }
    assert_eq!(first_clear_bit(&bits, 6), Some(13));
    assert_eq!(first_clear_bit(&bits, 12), Some(13));
    assert!(bits[0] & (1 << 5) == 0, "bit 5 stays clear for the scan");

    // Fully-set container: no live docid remains in range.
    let full = [0xFFu8; TOMBSTONE_CONTAINER_BYTES];
    assert_eq!(first_clear_bit(&full, 0), None);
    assert_eq!(first_clear_bit(&full, 65_535), None);
    // Clear bit just before the container end is found from any earlier position.
    let mut almost_full = [0xFFu8; TOMBSTONE_CONTAINER_BYTES];
    almost_full[TOMBSTONE_CONTAINER_BYTES - 1] = 0b0111_1111; // bit 65535 clear
    assert_eq!(first_clear_bit(&almost_full, 0), Some(65_535));
    assert_eq!(first_clear_bit(&almost_full, 65_534), Some(65_535));
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
fn block_max_bounds_bound_every_lazy_contribution() {
    // Lazy-scoring consistency gate (plan 0295): with contributions computed inline as
    // WEIGHT_BASE + table[tf], each stored docid-block bound (+ weight) must remain an
    // upper bound over every posting in that block — tombstone filtering can only lower
    // realized contributions, so the stored-posting level is the tightest check.
    let mut stores = TextStores::init(fresh_regions());
    seed_corpus(&mut stores);
    let identity_parts: Box<TfPartTable> = Box::new(std::array::from_fn(|tf| tf as u32));

    for term_id in 0..stores.meta.get().next_term_id {
        let Some(blob) = stores.postings_blob(term_id) else {
            continue;
        };
        let bounds = stores.load_bounds(term_id);
        let mut reader = FreqVarintReader::new(&blob);
        while let Some(docid) = reader.peek() {
            let tf = reader.freq().expect("interleaved tf");
            let realized = WEIGHT_BASE + identity_parts[tf.min(255) as usize];
            let block = (docid / LOGICAL_BLOCK_SIZE) as usize;
            let bound = bounds.get(block).copied().unwrap_or(0) + WEIGHT_BASE;
            assert!(
                bound >= realized,
                "term {term_id}: bound {bound} < realized {realized} at docid {docid}"
            );
            reader.next();
        }
    }
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

    // Stale-but-valid magic from layout 1 (pre-swap state) must also trap loudly rather
    // than misread region bytes written by the old structures.
    {
        let mut stale = Cell::init(regions.meta.clone(), TextMeta::default());
        stale.set(TextMeta {
            layout_version: LAYOUT_VERSION - 1,
            ..TextMeta::default()
        });
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reopen(&regions)));
    assert!(result.is_err(), "stale layout version must fail closed");
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

// -- Slice 11a (plan 0295 `structures-swap`): swapped-structure contracts ------------------

#[test]
fn dictionary_verification_rejects_forced_digest_collisions() {
    let mut stores = TextStores::init(fresh_regions());
    let shared_digest = 42u128;

    // Intern "alpha" through a forced digest pair; it occupies the first probe slot.
    let alpha_id = stores.dict_intern_digests("alpha", &[shared_digest, 7]);
    assert_eq!(alpha_id, 0, "interning allocates dense ids in order");

    // A verified probe hit matches by canonical string, not digest alone.
    assert_eq!(
        stores
            .dict_lookup_digests("alpha", &[shared_digest, 7])
            .map(|(term_id, _)| term_id),
        Some(0)
    );
    // Same primary digest, different string ⇒ digest collision degrades to a miss
    // (no false accept).
    assert_eq!(
        stores.dict_lookup_digests("beta", &[shared_digest, 7]),
        None,
        "hash-hit verification must reject foreign canonical strings"
    );

    // "beta" interns through its second probe because the first is occupied.
    let beta_id = stores.dict_intern_digests("beta", &[shared_digest, 9]);
    assert_eq!(beta_id, 1, "collision probing places at the absent digest");

    // A third term colliding on BOTH of beta's probes fails closed.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stores.dict_intern_digests("gamma", &[shared_digest, 9]);
    }));
    assert!(result.is_err(), "exhausted probe space must fail closed");

    // Forced-digest placements stay resolvable through THOSE digests (the natural
    // digests of these terms were never used, so they must miss).
    assert_eq!(
        stores
            .dict_lookup_digests("alpha", &[shared_digest, 7])
            .map(|(term_id, _)| term_id),
        Some(0)
    );
    assert_eq!(stores.dict_term_id("alpha"), None);
    assert_eq!(stores.dict_term_id("beta"), None);
}

#[test]
fn pending_log_drains_in_strict_fifo_order() {
    let mut stores = TextStores::init(fresh_regions());
    stores.enqueue_ingest(vec![doc(1, "apple")]).expect("op0");
    stores.enqueue_ingest(vec![doc(2, "bread")]).expect("op1");
    stores.enqueue_delete(doc_keys(&[1])).expect("op2");
    stores.enqueue_ingest(vec![doc(3, "cherry")]).expect("op3");
    assert_eq!(stores.get_stats().pending_ops, 4);

    // One op per step exposes the exact application order.
    let report = stores.flush_step(1);
    assert_eq!((report.drained_ops, report.done), (1, false));
    assert_eq!(stores.search("apple", 10).unwrap().len(), 1, "op0 first");
    assert_eq!(stores.get_stats().next_docid, 1);

    stores.flush_step(1);
    assert_eq!(stores.search("bread", 10).unwrap().len(), 1);
    assert_eq!(stores.get_stats().next_docid, 2);

    stores.flush_step(1);
    assert!(
        stores.search("apple", 10).unwrap().is_empty(),
        "op2 deletes k1 exactly when reached"
    );
    assert_eq!(
        stores.get_stats().next_docid,
        2,
        "delete allocates no docid"
    );

    let report = stores.flush_step(u64::MAX);
    assert!(report.done);
    assert_eq!(report.drained_ops, 1, "only op3 remained");
    assert_eq!(stores.search("cherry", 10).unwrap().len(), 1);
    assert_eq!(stores.get_stats().next_docid, 3);

    // Term ids follow application order across the deque, not sorted insertion.
    assert_eq!(stores.dict_term_id("apple"), Some(0));
    assert_eq!(stores.dict_term_id("bread"), Some(1));
    assert_eq!(stores.dict_term_id("cherry"), Some(2));
}

#[test]
fn multi_chunk_posting_lists_stay_exact_across_growth_and_reclaim() {
    let mut stores = TextStores::init(fresh_regions());
    // Three capped batches put "dense" into 3000 docs: its posting list (~6 KiB of
    // varints) spans multiple arena chunks, forcing growth relocations along the way.
    for batch in 0..3u64 {
        let docs: Vec<TextDoc> = (0..MAX_DOCS_PER_INGEST as u64)
            .map(|i| {
                let n = batch * MAX_DOCS_PER_INGEST as u64 + i;
                doc(n + 1, &format!("dense filler{n} dense"))
            })
            .collect();
        stores.enqueue_ingest(docs).expect("batch ingest");
        flush_all(&mut stores);
    }
    let dense_tid = stores.dict_term_id("dense").expect("interned");
    let blob_len = stores.postings_blob(dense_tid).expect("posted").len();
    assert!(
        blob_len > ARENA_CHUNK_BYTES,
        "fixture must span chunks (len {blob_len})"
    );
    assert_search_parity(&stores, "dense", 10);
    assert_search_parity(&stores, "dense filler5", 10);

    // Deleting every other doc forces reclaim-time shrink relocation of the big list.
    let victims: Vec<u64> = (1..=3 * MAX_DOCS_PER_INGEST as u64)
        .filter(|key| key % 2 == 0)
        .collect();
    stores
        .enqueue_delete(victims[..victims.len() / 2].to_vec())
        .expect("delete batch a");
    stores
        .enqueue_delete(victims[victims.len() / 2..].to_vec())
        .expect("delete batch b");
    flush_all(&mut stores);
    // ~3 k unique terms ⇒ several budgeted steps (MAX_MERGE_TERMS_PER_STEP per call).
    let mut report = stores.merge_step(MAX_MERGE_TERMS_PER_STEP);
    while !report.done {
        report = stores.merge_step(MAX_MERGE_TERMS_PER_STEP);
    }
    assert_eq!(stores.get_stats().ndocs, 1500, "half the corpus survives");

    let shrunk_len = stores.postings_blob(dense_tid).expect("still posted").len();
    assert!(
        shrunk_len < ARENA_CHUNK_BYTES && shrunk_len > 0,
        "reclaim must shrink the list below one chunk ({shrunk_len})"
    );
    assert_search_parity(&stores, "dense", 10);
    // All top-10 slots serve live survivors; the full count was asserted via stats.
    assert_eq!(stores.search("dense", 10).unwrap().len(), 10);
}

#[test]
fn dense_index_bounds_behave() {
    let mut stores = TextStores::init(fresh_regions());

    // Empty-state bounds: absent containers/refs/slots read as clean misses, never traps.
    assert!(
        !stores.is_tombstoned((1 << 16) + 5),
        "container ordinal beyond extent means no tombstones"
    );
    assert_eq!(
        stores.postings_blob(999),
        None,
        "term id beyond ref extent has no blob"
    );
    assert_eq!(stores.dict_term_id("ghost"), None);
    assert_eq!(stores.key_of_docid(3), None, "absent doc key slot");
    assert_eq!(stores.load_bounds(77), Vec::<u32>::new());

    seed_corpus(&mut stores);
    let doc_104_docid = stores.docid_of_key(104).expect("mapped");
    stores.enqueue_delete(doc_keys(&[104])).expect("delete");
    flush_all(&mut stores);

    // Cleared slots stay cleared: projections and reverse addressing agree.
    assert_eq!(stores.key_of_docid(doc_104_docid), None, "cleared slot");
    assert_eq!(stores.docid_of_key(104), None, "reverse map dropped too");
    assert!(stores.is_tombstoned(doc_104_docid));
    assert!(
        stores
            .search("fox", 10)
            .expect("search")
            .iter()
            .all(|hit| hit.key != 104),
        "projection serves live slots only"
    );
}

#[test]
fn every_swapped_structure_survives_reopen_round_trip() {
    let regions = fresh_regions();
    let (red_tid_pre, red_blob_pre, red_bounds_pre) = {
        let mut stores = reopen(&regions);
        stores
            .enqueue_ingest(vec![
                doc(1, "red fox"),
                doc(2, "blue fox"),
                doc(3, "red fish"),
            ])
            .expect("ingest");
        stores.flush_step(2); // op3 ("red fish") stays pending for now
        stores.enqueue_delete(doc_keys(&[2])).expect("real delete");
        flush_all(&mut stores); // applies red fish, then tombstones doc 2
        let partial = stores.merge_step(1); // reclaims fox@doc2; cursor persists
        assert_eq!((partial.terms_processed, partial.done), (1, false));

        // Enqueue an unflushed op so region 9's deque continuity is exercised too.
        stores
            .enqueue_ingest(vec![doc(4, "blue whale")])
            .expect("op pending");

        let red_tid = stores.dict_term_id("red").expect("interned");
        (
            red_tid,
            stores.postings_blob(red_tid),
            stores.load_bounds(red_tid),
        )
    };

    {
        let mut stores = reopen(&regions);

        // Region 2/13 dictionary: verified probes resolve identically post-reopen.
        assert_eq!(stores.dict_term_id("red"), Some(red_tid_pre));
        assert_eq!(stores.dict_term_id("missing-term"), None);
        assert_eq!(
            stores.get_stats().total_units,
            5,
            "fox@doc2's unit was already reclaimed pre-reopen (6 posted - 1)"
        );

        // Regions 3/4 blobs are byte-identical across the reopen.
        assert_eq!(
            stores.postings_blob(red_tid_pre),
            red_blob_pre,
            "posting blob bytes survive"
        );
        assert_eq!(
            stores.load_bounds(red_tid_pre),
            red_bounds_pre,
            "block-max table survives"
        );

        // Region 6/5 key addressing works in both directions for live docs.
        let red_docid = stores.docid_of_key(1).expect("doc 1 mapped");
        assert_eq!(stores.key_of_docid(red_docid), Some(1));

        // Region 7 tombstone bit set before the pass completed survives…
        assert_eq!(stores.docid_of_key(2), None, "deleted key left the map");
        assert!(stores.is_tombstoned(2), "bit persists across reopen");

        // …and the resumed pass finishes over reopened state, removing the emptied term.
        let report = stores.merge_step(MAX_MERGE_TERMS_PER_STEP);
        assert!(report.done, "resumed pass completes within one budget");
        assert_eq!(
            report.units_reclaimed, 1,
            "only 'blue's remaining posting was still tombstoned"
        );
        assert!(!stores.is_tombstoned(2), "pass completion clears bits");
        assert_eq!(
            stores.dict_term_id("blue"),
            None,
            "emptied term left the dictionary"
        );
        assert_eq!(stores.get_stats().tombstoned_docs, 0);

        // Region 9 deque continues FIFO application after reopen.
        assert_eq!(stores.get_stats().pending_ops, 1, "op4 still pending");
        flush_all(&mut stores);
        let hits = stores.search("whale", 10).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, 4, "post-reopen op applied last, in order");
        assert_eq!(stores.get_stats().next_docid, 4);

        assert_search_parity(&stores, "red fox fish whale", 10);
    }

    // Final reopen sees the converged state.
    let stores = reopen(&regions);
    let stats = stores.get_stats();
    assert_eq!(stats.ndocs, 3);
    assert_eq!(stats.tombstoned_docs, 0);
    assert_eq!(
        stats.total_units, 6,
        "fox + red×2 + fish + blue (re-interned by whale) + whale = 2+2+2"
    );
}
