//! Text Index lifecycle E2E on PocketIC (plan 0294, `pocketic-lifecycle`).
//!
//! ONE PocketIC bootstrap serves all four scenarios in order; exactly two
//! `text-canister` installs run against it — the primary lifecycle canister and a
//! fresh-state twin for the byte-for-byte determinism spot-check. The corpus is
//! deterministic (~240 docs, mixed ASCII/Japanese with NFKC-only vocabulary), and every
//! search expectation comes from a client-side brute-force oracle over the production
//! analyzer ([`text_canister::analyzer::analyze`]) with docids modeled as flush-order
//! allocation (= ingest order here), so a wrong tie-break fails: the engineered CJK tie
//! pair deliberately reverses key order relative to docid order.
//!
//! Budget posture (cost-aware-validation): one `PocketIc` constructor, two canister
//! installs, ~15 update calls, ~50 query calls total.
//!
//! Run note: this target provisions its own `text-canister` wasm (isolated target dir;
//! override artifact via `TEXT_INDEX_WASM=<path>`) and needs no federation artifacts, so
//! `POCKET_IC_SKIP_FEDERATION_WASM=1` may be used to skip the shared federation wasm
//! build when those sources are mid-change.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeTo;
use std::path::PathBuf;
use std::process::Command;

use candid::{Decode, Encode, Principal};
use gleaph_pocket_ic_tests::new_pocket_ic;
use pocket_ic::PocketIc;
use text_canister::analyzer::analyze;
use text_canister::{FlushReport, MergeStepReport, TextDoc, TextHit, TextIndexStats, WEIGHT_BASE};

// -- Standalone canister provisioning ---------------------------------------------------------

/// Builds the `text-canister` wasm in an isolated target dir and installs it as a
/// standalone custom canister.
///
/// This deliberately does not go through the shared pocket-ic-tests `build.rs`: that
/// builds the whole federation and is currently broken by unrelated in-flight WIP
/// (`ic-stable-vector-page-store` vs `vector-canister` signature skew), while this
/// lifecycle contract needs exactly one custom canister. Raw cargo output is installed
/// directly (the shared postprocess step adds deploy metadata this E2E does not read).
/// An existing artifact path may be supplied via `TEXT_INDEX_WASM` to skip the build.
fn ensure_text_wasm() -> Vec<u8> {
    if let Ok(path) = std::env::var("TEXT_INDEX_WASM") {
        return std::fs::read(&path)
            .unwrap_or_else(|e| panic!("read TEXT_INDEX_WASM {}: {e}", path));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|crates_dir| crates_dir.parent())
        .expect("workspace root above crates/");
    let target_dir = workspace_root.join("target").join("pocket-ic-text-wasm");
    let status = Command::new("cargo")
        .current_dir(&workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "--package",
            "text-canister",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .status()
        .expect("spawn cargo build for text-canister");
    assert!(status.success(), "text-canister wasm build failed");
    let wasm = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("text_canister.wasm");
    std::fs::read(&wasm).unwrap_or_else(|e| panic!("read {}: {e}", wasm.display()))
}

fn install_text_canister(pic: &PocketIc, controller: Option<Principal>) -> Principal {
    let id = pic.create_canister();
    pic.add_cycles(id, 2_000_000_000_000);
    pic.install_canister(
        id,
        ensure_text_wasm(),
        Encode!(&text_canister::TextCanisterInitArgs { controller }).expect("encode text init"),
        None,
    );
    id
}

// -- Deterministic fixture -------------------------------------------------------------------

/// Engineered docs ingested first (docids 1..=8). Keys sit outside the generated key
/// range; the CJK tie pair reverses key order vs ingest order so a key-based tie-break
/// cannot pass.
const ENGINEERED: &[(u64, &str)] = &[
    (501, "the red fox"),
    (712, "the blue fox"),
    (633, "red red fox"),
    (894, "fox"),
    (455, "東京都"),
    (344, "東京都"),
    (777, "ＦＵＬＬＴＥＸＴ fulltext"),
    (338, "東京 tower"),
    // Isolated equal-score pair (unique vocabulary): identical unit sets tie at score 4,
    // and the key order deliberately reverses the docid order so the deterministic
    // (score desc, docid asc) contract is directly observable.
    (560, "qqunique zzunique"),
    (349, "qqunique zzunique"),
];

/// Generated mixed-script docs appended after the engineered block.
const GENERATED_DOCS: usize = 232;

/// Merge-step budget that forces several resumable steps instead of one big pass.
const RESUME_BUDGET: u32 = 16;

/// The fixed query battery: single-term, multi-term, CJK-bigram, NFKC-folded, no-hit,
/// plus truncation.
const QUERIES: [(&str, u32); 8] = [
    ("fox", 10),
    ("red fox", 10),
    ("東京", 10),
    ("京都 大阪", 5),
    ("fulltext", 10),
    ("zzznotfound", 10),
    ("fox", 3),
    ("検索 索引", 10),
];

struct Corpus {
    /// `(key, text)` in exact ingest order; docid = index + 1 by the flush contract.
    docs: Vec<(u64, String)>,
}

/// Deterministic stream (no rand dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn build_corpus() -> Corpus {
    let mut docs: Vec<(u64, String)> = ENGINEERED
        .iter()
        .map(|(key, text)| (*key, (*text).to_string()))
        .collect();

    const ASCII_WORDS: [&str; 16] = [
        "graph", "index", "query", "vertex", "edge", "shard", "canister", "stable", "memory",
        "router", "search", "token", "segment", "posting", "merge", "budget",
    ];
    const JP_WORDS: [&str; 14] = [
        "東京",
        "京都",
        "大阪",
        "北海道",
        "ひらがな",
        "カタカナ",
        "漢字",
        "平仮名",
        "片仮名",
        "検索",
        "索引",
        "結合",
        "分散",
        "安定",
    ];

    let mut rng = Lcg(2026_0824);
    // Fisher-Yates over a contiguous range: unique keys whose assignment order is
    // unrelated to ingest order.
    let mut keys: Vec<u64> = (100..100 + GENERATED_DOCS as u64).collect();
    for i in (1..keys.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        keys.swap(i, j);
    }

    for key in keys {
        let len = 6 + (rng.next() % 9) as usize;
        let mut text = Vec::with_capacity(len);
        for _ in 0..len {
            if rng.next() % 2 == 0 {
                text.push(ASCII_WORDS[(rng.next() % 16) as usize]);
            } else {
                text.push(JP_WORDS[(rng.next() % 14) as usize]);
            }
        }
        docs.push((key, text.join(" ")));
    }

    Corpus { docs }
}

impl Corpus {
    /// Candid-ready docs for an ingest-order slice.
    fn batch(&self, range: std::ops::Range<usize>) -> Vec<TextDoc> {
        self.docs[range]
            .iter()
            .map(|(key, text)| TextDoc {
                key: *key,
                text: text.clone(),
            })
            .collect()
    }
}

/// Per-document term frequencies under the production analyzer (ingest-order aligned).
fn corpus_tfs(corpus: &Corpus) -> Vec<BTreeMap<String, u32>> {
    corpus
        .docs
        .iter()
        .map(|(_, text)| {
            let mut tfs: BTreeMap<String, u32> = BTreeMap::new();
            for unit in analyze(text) {
                *tfs.entry(unit).or_insert(0) += 1;
            }
            tfs
        })
        .collect()
}

/// Brute-force oracle mirroring the v0 identity scorer (`WEIGHT_BASE` + tf per matched
/// distinct term), tombstone-aware, restricted to flushed docids (`..=visible_docs`),
/// ordered (score desc, docid asc) with docid = ingest order, truncated to k after the
/// visibility cut (mirroring what a search over flushed state can see). Returns
/// `(key, docid, score)` triples.
fn expected_hits(
    corpus: &Corpus,
    tfs: &[BTreeMap<String, u32>],
    query: &str,
    k: u32,
    visible_docs: usize,
    deleted: &BTreeSet<u64>,
) -> Vec<(u64, u32, u64)> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut scores: BTreeMap<u32, u64> = BTreeMap::new();
    for term in analyze(query) {
        if !seen.insert(term.clone()) {
            continue;
        }
        for (idx, (key, _)) in corpus.docs.iter().enumerate() {
            if idx + 1 > visible_docs || deleted.contains(key) {
                continue;
            }
            if let Some(tf) = tfs[idx].get(&term) {
                *scores.entry((idx + 1) as u32).or_insert(0) += u64::from(WEIGHT_BASE + tf);
            }
        }
    }
    let mut ranked: Vec<(u32, u64)> = scores.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    ranked.truncate(k as usize);
    ranked
        .into_iter()
        .map(|(docid, score)| (corpus.docs[(docid - 1) as usize].0, docid, score))
        .collect()
}

// -- Canister call helpers ---------------------------------------------------------------------

struct Ctx<'a> {
    pic: &'a PocketIc,
    controller: Principal,
    text: Principal,
}

fn decode_result<T>(bytes: Vec<u8>, what: &str) -> T
where
    T: candid::CandidType + serde::de::DeserializeOwned,
{
    match Decode!(&bytes, Result<T, String>) {
        Ok(Ok(value)) => value,
        Ok(Err(err)) => panic!("{what} rejected: {err}"),
        Err(err) => panic!("decode {what}: {err}"),
    }
}

fn ingest(ctx: &Ctx, docs: Vec<TextDoc>) {
    let bytes = ctx
        .pic
        .update_call(
            ctx.text,
            ctx.controller,
            "ingest_text",
            Encode!(&docs).expect("encode docs"),
        )
        .unwrap_or_else(|e| panic!("ingest_text: {e:?}"));
    let _: () = decode_result(bytes, "ingest_text");
}

fn delete_docs(ctx: &Ctx, keys: &[u64]) {
    let bytes = ctx
        .pic
        .update_call(
            ctx.text,
            ctx.controller,
            "delete_docs",
            Encode!(&keys.to_vec()).expect("encode keys"),
        )
        .unwrap_or_else(|e| panic!("delete_docs: {e:?}"));
    let _: () = decode_result(bytes, "delete_docs");
}

/// Raw Candid reply bytes — the unit of the byte-for-byte comparisons.
fn search_bytes(ctx: &Ctx, query: &str, k: u32) -> Vec<u8> {
    ctx.pic
        .query_call(
            ctx.text,
            ctx.controller,
            "search",
            Encode!(&query.to_string(), &k).expect("encode search"),
        )
        .unwrap_or_else(|e| panic!("search({query:?}, {k}): {e:?}"))
}

fn search(ctx: &Ctx, query: &str, k: u32) -> Vec<TextHit> {
    decode_result(search_bytes(ctx, query, k), "search")
}

fn get_stats(ctx: &Ctx) -> TextIndexStats {
    let bytes = ctx
        .pic
        .query_call(
            ctx.text,
            ctx.controller,
            "get_stats",
            Encode!(&()).expect("encode get_stats"),
        )
        .unwrap_or_else(|e| panic!("get_stats: {e:?}"));
    Decode!(&bytes, TextIndexStats).expect("decode get_stats")
}

fn flush_step(ctx: &Ctx) -> (u64, bool) {
    let bytes = ctx
        .pic
        .update_call(
            ctx.text,
            ctx.controller,
            "admin_flush",
            Encode!(&()).expect("encode admin_flush"),
        )
        .unwrap_or_else(|e| panic!("admin_flush: {e:?}"));
    let FlushReport {
        drained_ops, done, ..
    } = Decode!(&bytes, FlushReport).expect("decode admin_flush");
    (drained_ops, done)
}

fn flush_all(ctx: &Ctx) {
    for _round in 0..200 {
        if flush_step(ctx).1 {
            return;
        }
    }
    panic!("admin_flush did not drain within 200 bounded steps");
}

fn merge_to_completion(ctx: &Ctx, budget: u32) -> u64 {
    let mut reclaimed_total = 0u64;
    for _step in 0..500 {
        let bytes = ctx
            .pic
            .update_call(
                ctx.text,
                ctx.controller,
                "admin_merge_step",
                Encode!(&budget).expect("encode admin_merge_step"),
            )
            .unwrap_or_else(|e| panic!("admin_merge_step: {e:?}"));
        let MergeStepReport {
            units_reclaimed,
            done,
            ..
        } = decode_result(bytes, "admin_merge_step");
        reclaimed_total += units_reclaimed;
        if done {
            return reclaimed_total;
        }
    }
    panic!("admin_merge_step did not complete within 500 bounded steps");
}

fn cycles(pic: &PocketIc, canister: Principal) -> u128 {
    let nat = pic
        .canister_status(canister, None)
        .expect("canister status")
        .cycles;
    u128::try_from(nat.0).unwrap_or(u128::MAX)
}

// -- Scenarios ------------------------------------------------------------------------------

/// Runs the full parity battery restricted to the flushed docid prefix, asserting every
/// result equals the oracle (membership, order, scores, tie-breaks). Returns the raw
/// Candid reply bytes per battery entry.
fn assert_parity(
    ctx: &Ctx,
    corpus: &Corpus,
    tfs: &[BTreeMap<String, u32>],
    visible: RangeTo<usize>,
    deleted: &BTreeSet<u64>,
) -> Vec<Vec<u8>> {
    let mut reply_bytes = Vec::new();
    for (query, k) in QUERIES {
        let got: Vec<(u64, u32, u64)> = search(ctx, query, k)
            .iter()
            .map(|hit| (hit.key, hit.docid, hit.score as u64))
            .collect();
        let want: Vec<(u64, u32, u64)> = expected_hits(corpus, tfs, query, k, visible.end, deleted);
        assert_eq!(
            got, want,
            "parity failed for {query:?} (k={k}, visible={})",
            visible.end
        );
        reply_bytes.push(search_bytes(ctx, query, k));
    }
    reply_bytes
}

/// Scenario 1: durable logging, under-posted-until-flush lag (including partial-prefix
/// visibility), and full brute-force parity once flushed. Returns the pristine-state
/// reply baseline for the determinism twin.
fn scenario_ingest_search_parity(
    ctx: &Ctx,
    corpus: &Corpus,
    tfs: &[BTreeMap<String, u32>],
) -> Vec<Vec<u8>> {
    let mid = corpus.docs.len() / 2;

    // Batch A logged durably but invisible until its flush step runs.
    ingest(ctx, corpus.batch(0..mid));
    let stats = get_stats(ctx);
    assert_eq!(stats.pending_ops, mid as u64, "ops must be durably logged");
    assert!(
        search(ctx, "fox", 10).is_empty(),
        "pre-flush search must be empty (under-posted-until-flush lag)"
    );
    flush_all(ctx);

    // Batch B enqueued but unflushed: batch A stays searchable and already matches the
    // oracle restricted to the flushed prefix; B contributes nothing yet.
    ingest(ctx, corpus.batch(mid..corpus.docs.len()));
    let stats_after_b = get_stats(ctx);
    assert_eq!(stats_after_b.pending_ops, (corpus.docs.len() - mid) as u64);
    assert_parity(ctx, corpus, tfs, ..mid, &BTreeSet::new());
    flush_all(ctx);

    let stats = get_stats(ctx);
    assert_eq!(stats.ndocs, corpus.docs.len() as u64);
    assert_eq!(stats.pending_ops, 0);

    // Direct deterministic tie-break evidence: the isolated engineered pair has
    // identical unit sets (equal score 4); ordering must follow docid asc even though
    // key order reverses it.
    let tie: Vec<u64> = search(ctx, "qqunique zzunique", 10)
        .iter()
        .map(|hit| hit.key)
        .collect();
    assert_eq!(tie, vec![560, 349], "equal scores must order by docid asc");

    // Full-corpus parity battery.
    assert_parity(ctx, corpus, tfs, ..corpus.docs.len(), &BTreeSet::new())
}

/// Scenario 2: deletes exclude docs immediately (over-posted physically, filtered
/// logically); merge reclaims physically without changing any logical result; the
/// surviving member of the engineered tie pair moves up in rank.
fn scenario_tombstone_then_merge_exactness(
    ctx: &Ctx,
    corpus: &Corpus,
    tfs: &[BTreeMap<String, u32>],
) {
    // Delete the higher-docid member of the isolated tie pair (key 349), the other
    // engineered tie pair's higher-docid member (key 344), the top scorer (key 633,
    // "red red fox"), and every 9th generated doc.
    let mut deleted: BTreeSet<u64> = BTreeSet::from([349, 344, 633]);
    for (idx, (key, _)) in corpus.docs.iter().enumerate() {
        if idx >= ENGINEERED.len() && idx % 9 == 4 {
            deleted.insert(*key);
        }
    }

    delete_docs(ctx, &deleted.iter().copied().collect::<Vec<_>>());
    flush_all(ctx);

    let stats = get_stats(ctx);
    assert_eq!(stats.tombstoned_docs, deleted.len() as u64);
    assert_eq!(stats.ndocs, (corpus.docs.len() - deleted.len()) as u64);

    // Implemented contract: results already exclude deleted docs pre-merge.
    let post_delete_replies = assert_parity(ctx, corpus, tfs, ..corpus.docs.len(), &deleted);

    // Deleting the higher-docid member of the isolated tie pair leaves exactly its
    // survivor.
    let tie_hits = search(ctx, "qqunique zzunique", 10);
    assert_eq!(tie_hits.len(), 1, "deleted tie member must vanish");
    assert_eq!(tie_hits[0].key, 560, "surviving tie member must remain");

    // Physical reclaim via resumable bounded steps.
    let reclaimed = merge_to_completion(ctx, RESUME_BUDGET);
    assert!(
        reclaimed > 0,
        "merge must physically reclaim tombstoned units"
    );

    let stats = get_stats(ctx);
    assert_eq!(
        stats.tombstoned_docs, 0,
        "completed merge pass clears tombstones"
    );
    assert_eq!(stats.ndocs, (corpus.docs.len() - deleted.len()) as u64);

    // Exactness: physical reclaim changes no logical output.
    let post_merge_replies = assert_parity(ctx, corpus, tfs, ..corpus.docs.len(), &deleted);
    assert_eq!(
        post_merge_replies, post_delete_replies,
        "merge must not change logical search outputs"
    );
}

/// Scenario 3: controller guard live on wasm; observed cycle cost per admin call logged;
/// upgrade preserves stats and byte-identical search outputs; admin surface stays alive.
fn scenario_budget_and_upgrade(ctx: &Ctx) {
    let anonymous = Principal::anonymous();
    let err = ctx
        .pic
        .update_call(
            ctx.text,
            anonymous,
            "admin_flush",
            Encode!(&()).expect("encode admin_flush"),
        )
        .expect_err("anonymous admin_flush must be rejected by the controller guard");
    assert!(
        err.reject_message.contains("text index controller"),
        "guard rejection should name the controller requirement: {:?}",
        err.reject_message
    );

    // The documented budgets are loop-count constants, so the asserted contract is
    // completion without trap; the cycle balance only decreases, and its drop is the
    // observed compute+storage cost of each call.
    let before = cycles(ctx.pic, ctx.text);
    ingest(
        ctx,
        vec![TextDoc {
            key: 99_999,
            text: "budget probe doc".to_string(),
        }],
    );
    println!(
        "[cycles] ingest_text(1 doc): {}",
        before.saturating_sub(cycles(ctx.pic, ctx.text))
    );

    let before = cycles(ctx.pic, ctx.text);
    flush_all(ctx);
    println!(
        "[cycles] admin_flush(to done): {}",
        before.saturating_sub(cycles(ctx.pic, ctx.text))
    );

    let before = cycles(ctx.pic, ctx.text);
    merge_to_completion(ctx, RESUME_BUDGET);
    println!(
        "[cycles] admin_merge_step(to done): {}",
        before.saturating_sub(cycles(ctx.pic, ctx.text))
    );

    let stats_before = get_stats(ctx);
    let replies_before: Vec<Vec<u8>> = QUERIES
        .iter()
        .map(|(q, k)| search_bytes(ctx, q, *k))
        .collect();

    ctx.pic
        .upgrade_canister(
            ctx.text,
            ensure_text_wasm(),
            Encode!(&()).expect("encode upgrade args"),
            None,
        )
        .expect("upgrade text canister");

    let stats_after = get_stats(ctx);
    assert_eq!(
        stats_after, stats_before,
        "stats must survive upgrade identically"
    );
    let replies_after: Vec<Vec<u8>> = QUERIES
        .iter()
        .map(|(q, k)| search_bytes(ctx, q, *k))
        .collect();
    assert_eq!(
        replies_after, replies_before,
        "search outputs must survive upgrade byte-for-byte"
    );

    // Admin surface still live post-upgrade (completion without trap).
    flush_all(ctx);
    merge_to_completion(ctx, RESUME_BUDGET);
}

/// Scenario 4: identical install shape + identical corpus + identical call order on
/// fresh state must reproduce the primary's pristine replies byte-for-byte.
fn scenario_determinism_twin(ctx: &Ctx, corpus: &Corpus, baseline: &[Vec<u8>]) {
    let twin_controller = Principal::from_slice(&[0x7F; 29]);
    let twin = install_text_canister(ctx.pic, Some(twin_controller));
    let twin_ctx = Ctx {
        pic: ctx.pic,
        controller: twin_controller,
        text: twin,
    };

    ingest(&twin_ctx, corpus.batch(0..corpus.docs.len()));
    flush_all(&twin_ctx);

    for ((query, k), want) in QUERIES.iter().zip(baseline) {
        let got = search_bytes(&twin_ctx, query, *k);
        assert_eq!(
            &got, want,
            "determinism: candid-encoded reply differs for {query:?} (k={k})"
        );
    }
}

#[test]
fn text_index_lifecycle_scenarios() {
    let pic = new_pocket_ic();
    let controller = Principal::from_slice(&[0x7E; 29]);
    let text = install_text_canister(&pic, Some(controller));

    let corpus = build_corpus();
    let tfs = corpus_tfs(&corpus);
    let ctx = Ctx {
        pic: &pic,
        controller,
        text,
    };

    let baseline = scenario_ingest_search_parity(&ctx, &corpus, &tfs);
    scenario_tombstone_then_merge_exactness(&ctx, &corpus, &tfs);
    scenario_budget_and_upgrade(&ctx);
    scenario_determinism_twin(&ctx, &corpus, &baseline);
}
