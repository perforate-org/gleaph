//! FTS5-on-VFS comparison-arm measurement suite (plan 0293 decision point D1).
//!
//! Run from `crates/text-index-fts5-arm`: `canbench <pattern>` (see `canbench.yml`). This
//! module compiles only for wasm32 builds with the `canbench` feature — the engine under
//! test links the wasm32-only precompiled SQLite archive. Every measured closure is
//! self-contained w.r.t. state created *inside* it: corpus generation, document-body
//! materialization, DB init/migrate, the cache-size override, full ingestion, and all
//! correctness verification happen outside `bench_fn`, so a fast wrong path cannot be
//! measurable.
//!
//! Schema (contentless first): `CREATE VIRTUAL TABLE docs USING fts5(body, content='')`.
//! Our own postings store no text either, so stored-content bytes would inflate any storage
//! comparison; the one-time verifier below asserts that `MATCH` counting and
//! `ORDER BY rank LIMIT 10` still work through this schema before anything is measured.
//! If contentless mode misbehaves under instrumentation, fall back to a plain fts5 table
//! (`USING fts5(body)`) and explicitly record that stored-content bytes inflate the storage
//! comparison.
//!
//! PRAGMA policy: `PRAGMA cache_size = -2048` (2 MiB page cache) is applied to every
//! connection before loading or querying — the library default `-32768` (~32 MiB) would
//! dominate `heap_increase` at this fixture size (spike finding). `journal_mode` (MEMORY)
//! and temp storage (TEMP_STORE=3) stay at ic-sqlite-vfs defaults by design; they are part
//! of the engine's real cost profile.
//!
//! Read-path fairness: the verifier warms exactly the queries being measured through the
//! same cached read connection, so the measured closures see the warm-cache serving state
//! (the custom arm's encoded postings are likewise resident buffers), never cold-VFS noise.
//!
//! Stable-memory-increase semantics for this arm: the ic-sqlite-vfs memory-manager fork
//! allocates raw stable memory in 128-page (8 MiB) buckets, so canbench's
//! `stable_memory_increase` stays 0 unless a transaction crosses a bucket boundary — even
//! though commit publishes real pages into the image. The honest index-size number here is
//! therefore the logical database size (`PRAGMA page_count` × `PRAGMA page_size`), which
//! `bench_fts5_ingest_m` prints after its measured transaction; compare that against the
//! custom arm's encoded posting-list bytes, not against SMI.

use std::cell::RefCell;
use std::hint::black_box;
use std::sync::OnceLock;

use canbench_rs::bench;
use ic_sqlite_vfs::db::connection::Connection;
use ic_sqlite_vfs::db::migrate::Migration;
use ic_sqlite_vfs::{Db, DbError, DefaultMemoryImpl, MemoryId, MemoryManager, params};

use crate::fixture;

/// Page-cache override applied before loading (see module docs): 2 MiB instead of ~32 MiB.
const CACHE_SIZE_PRAGMA: &str = "PRAGMA cache_size = -2048;";

/// Contentless schema (see module docs for rationale and fallback contract).
const SCHEMA_SQL: &str = "CREATE VIRTUAL TABLE docs USING fts5(body, content='')";

/// Dedicated stable-memory region owned by SQLite forever (ic-sqlite-vfs README convention);
/// this arm owns the whole fresh canister state, so no co-residency conflicts arise.
const SQLITE_MEMORY_ID: u8 = 120;

/// Top-k width of the ranked disjunctive probe; matches the custom arm's top-10.
const TOP_K: usize = 10;

/// Point-lookup depth of the dense-term bench is fixed at 100 by the bench contract
/// (baked into the measured SQL below).

struct ArmFixture {
    /// Pre-built document bodies indexed by docid (rowid = docid + 1); built once outside
    /// every measured closure.
    docs: Vec<String>,
    /// Probe terms aligned with [`fixture::PROBE_RANKS`] (dense, mid, tail).
    probes: Vec<String>,
    /// `'p0 OR p1 OR p2'` over the three probe terms.
    m3_query: String,
    /// Brute-force expected dfs from [`ic_stable_text_postings::corpus`], aligned with
    /// `probes`.
    probe_dfs: [u32; 3],
}

static ARM: OnceLock<ArmFixture> = OnceLock::new();

fn fixture() -> &'static ArmFixture {
    ARM.get_or_init(|| {
        let corpus = fixture::corpus();
        let docs = fixture::doc_bodies(&corpus);
        let probes: Vec<String> = fixture::PROBE_RANKS
            .iter()
            .map(|&rank| corpus.vocab[rank].clone())
            .collect();
        let probe_dfs: [u32; 3] = fixture::PROBE_RANKS
            .iter()
            .map(|&rank| fixture::expected_df(&corpus, rank))
            .collect::<Vec<_>>()
            .try_into()
            .expect("three probe bands");
        let m3_query = probes.join(" OR ");
        ArmFixture {
            docs,
            probes,
            m3_query,
            probe_dfs,
        }
    })
}

thread_local! {
    /// Keeps the memory manager (and thus the `DbMemory` backing) alive for the process
    /// lifetime; benches and canbench are single-threaded.
    static MANAGER: RefCell<Option<MemoryManager<DefaultMemoryImpl>>> =
        const { RefCell::new(None) };
}

static VERIFIED: OnceLock<()> = OnceLock::new();

/// Initializes SQLite over its dedicated stable-memory region, migrates the schema, and
/// applies the page-cache override to the cached write connection. Idempotent.
fn ensure_db() {
    MANAGER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let manager = MemoryManager::init(DefaultMemoryImpl::default());
            let memory = manager.get(MemoryId::new(SQLITE_MEMORY_ID));
            Db::init(memory).expect("fresh stable memory must accept the SQLite image");
            *slot = Some(manager);
        }
    });
    Db::migrate(&[Migration {
        version: 1,
        sql: SCHEMA_SQL,
    }])
    .expect("schema migration must succeed");
    Db::update(|conn| conn.execute_batch(CACHE_SIZE_PRAGMA))
        .expect("cache-size override must succeed");
}

/// Runs one read-only query with the page-cache override applied first; the read connection
/// is cached by the facade, so later calls reuse the overridden, warmed connection.
fn with_query<T>(f: impl FnOnce(&Connection) -> Result<T, DbError>) -> T {
    Db::query(|conn| {
        conn.execute_batch(CACHE_SIZE_PRAGMA)?;
        f(conn)
    })
    .expect("query must succeed")
}

/// Counts rows currently indexed in `docs` (setup helper, never measured).
fn doc_count() -> i64 {
    with_query(|conn| conn.query_scalar("SELECT count(*) FROM docs", params![]))
}

/// Drops any existing `docs` table and recreates the contentless schema, guaranteeing a
/// genuinely empty index regardless of prior state in this instance (setup only, never
/// measured; `DROP TABLE` is unaffected by contentless row restrictions).
fn reset_schema() {
    Db::update(|conn| conn.execute_batch("DROP TABLE IF EXISTS docs"))
        .expect("schema drop must succeed");
    Db::update(|conn| conn.execute_batch(SCHEMA_SQL)).expect("schema create must succeed");
}

/// Inserts all M pre-built document strings in ONE transaction through a prepared
/// statement. This is the exact work `bench_fts5_ingest_m` measures.
fn load_all_docs(docs: &[String]) {
    Db::update(|conn| {
        let mut stmt = conn.prepare("INSERT INTO docs(rowid, body) VALUES (?1, ?2)")?;
        for (docid, body) in docs.iter().enumerate() {
            stmt.execute_i64_text(docid as i64 + 1, body)?;
        }
        Ok(())
    })
    .expect("bulk insert transaction must commit");
}

/// One-time correctness gate (outside every measured closure): brute-force dfs vs `MATCH`
/// hit counts for the three probe terms, plus the ranked m3 smoke asserting exactly ten
/// rows. Also deterministically warms the read path for the benches that follow.
fn verify_queries(fx: &'static ArmFixture) {
    if VERIFIED.set(()).is_err() {
        return;
    }
    for (slot, &rank) in fixture::PROBE_RANKS.iter().enumerate() {
        let expected = fx.probe_dfs[slot];
        let got: i64 = with_query(|conn| {
            conn.query_scalar(
                "SELECT count(*) FROM docs WHERE docs MATCH ?1",
                params![fx.probes[slot].as_str()],
            )
        });
        assert_eq!(
            got, expected as i64,
            "probe rank {rank}: MATCH hits must equal brute-force df"
        );
    }
    let rowids: Vec<i64> = with_query(|conn| {
        let mut stmt =
            conn.prepare("SELECT rowid FROM docs WHERE docs MATCH ?1 ORDER BY rank LIMIT 10")?;
        stmt.query_all(params![fx.m3_query], |row| row.get::<i64>(0))
    });
    assert_eq!(
        rowids.len(),
        TOP_K,
        "ranked m3 query must return exactly {TOP_K} rows (contentless rank check)"
    );
    let mut distinct = rowids.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), TOP_K, "ranked rows must be distinct docs");

    // Scored top-100 (plan 0296 fair-pair matrix): warm AND verify the exact statement
    // the scored bench measures — 100 ranked rows, distinct docs, finite bm25 scores.
    let dense = fx.probes[0].as_str();
    let scored_rows: Vec<(i64, f64)> = with_query(|conn| {
        let mut stmt = conn.prepare(
            "SELECT rowid, bm25(docs) FROM docs WHERE docs MATCH ?1 ORDER BY rank LIMIT 100",
        )?;
        stmt.query_all(params![dense], |row| {
            Ok((row.get::<i64>(0)?, row.get::<f64>(1)?))
        })
    });
    assert_eq!(
        scored_rows.len(),
        100,
        "scored dense probe must return exactly 100 ranked rows"
    );
    let mut distinct_ids = scored_rows
        .iter()
        .map(|&(rowid, _)| rowid)
        .collect::<Vec<_>>();
    distinct_ids.sort_unstable();
    distinct_ids.dedup();
    assert_eq!(distinct_ids.len(), 100, "scored rows must be distinct docs");
    assert!(
        scored_rows.iter().all(|&(_, score)| score.is_finite()),
        "bm25 scores must be finite"
    );
}

/// Setup shared by both query benches: DB ready, fully ingested, verified, warm.
fn setup_query_arm() -> &'static ArmFixture {
    let fx = black_box(fixture());
    ensure_db();
    let existing = doc_count();
    if existing == 0 {
        load_all_docs(&fx.docs);
    } else {
        assert_eq!(
            existing as usize,
            fx.docs.len(),
            "stale docs table state in fresh instance"
        );
    }
    verify_queries(fx);
    fx
}

/// D1 comparison evidence for ingestion; no absolute threshold — the custom arm's numbers
/// are the reference. Measures instructions AND heap increase (page cache + memory-journal
/// overlay) for inserting all M pre-built document strings through a prepared statement in
/// one transaction. Corpus build, body materialization, migrate, and the cache override
/// stay outside. After measurement, prints the resulting logical database size — see the
/// module docs for why that, not `stable_memory_increase`, is this arm's honest storage
/// number.
///
/// Counterpart workload class: custom-arm merge/index step benches.
#[bench(raw)]
fn bench_fts5_ingest_m() -> canbench_rs::BenchResult {
    let fx = black_box(fixture());
    ensure_db();
    reset_schema();
    assert_eq!(
        doc_count(),
        0,
        "ingest bench requires an empty database image"
    );
    let docs = black_box(&fx.docs);
    let result = canbench_rs::bench_fn(move || {
        load_all_docs(docs);
    });
    report_db_size();
    result
}

/// Prints the logical database size after ingestion (setup-side probe, never measured).
fn report_db_size() {
    let page_count: i64 = with_query(|conn| conn.query_scalar("PRAGMA page_count", params![]));
    let page_size: i64 = with_query(|conn| conn.query_scalar("PRAGMA page_size", params![]));
    ic_cdk::println!(
        "fts5_storage: page_count={page_count} page_size={page_size} bytes={}",
        page_count * page_size
    );
}

/// D1 point-lookup evidence: top-100 docids for the dense probe term. Setup ingests and
/// verifies everything first; the measured closure pays statement preparation plus the
/// MATCH scan, mirroring the custom arm's walk-style reader-per-sweep accounting.
///
/// Counterpart workload class: dense posting-list traversal.
#[bench(raw)]
fn bench_fts5_query_term_top100() -> canbench_rs::BenchResult {
    let fx = black_box(setup_query_arm());
    let dense = black_box(fx.probes[0].as_str());
    canbench_rs::bench_fn(|| {
        let hits: Vec<i64> = with_query(|conn| {
            let mut stmt = conn.prepare("SELECT rowid FROM docs WHERE docs MATCH ?1 LIMIT 100")?;
            stmt.query_all(params![dense], |row| row.get::<i64>(0))
        });
        black_box(hits);
    })
}

/// **D1 headline number:** disjunctive 3-term ranked top-10 (`ORDER BY rank`) over the
/// three probe terms (dense ≈ A band, mid ≈ B/C band, tail ≈ D band) — direct counterpart
/// to the custom arm's `bench_topk_bmw_m3_top10`. Setup verifies the exact result-width
/// contract (10 rows) before measurement.
///
/// **Predeclared threshold:** informational only; the custom arm recorded 74.23 M
/// instructions for its m3 top-10 against a ≤50 M working target. This arm exists to place
/// FTS5's cost next to that number for the D1 engine decision.
#[bench(raw)]
fn bench_fts5_query_rank_m3_top10() -> canbench_rs::BenchResult {
    let fx = black_box(setup_query_arm());
    let m3 = black_box(fx.m3_query.as_str());
    canbench_rs::bench_fn(move || {
        let hits: Vec<i64> = with_query(|conn| {
            let mut stmt =
                conn.prepare("SELECT rowid FROM docs WHERE docs MATCH ?1 ORDER BY rank LIMIT 10")?;
            stmt.query_all(params![m3], |row| row.get::<i64>(0))
        });
        black_box(hits);
    })
}

/// Scored half of the D1 fair-pair matrix (plan 0296): the SAME dense probe term as
/// [`bench_fts5_query_term_top100`] and corpus config, but through FTS5's ranking
/// machinery — `bm25(docs)` projected explicitly, `ORDER BY rank`, LIMIT 100. Workload
/// counterpart of the custom arm's tf-scored whole-path query bench; its unscored rowid
/// sibling above completes the 2×2 matrix on this side.
///
/// Setup verifies the exact statement first (100 ranked rows, distinct docs, finite
/// scores — see `verify_queries`) and warms it through the same cached connection.
#[bench(raw)]
fn bench_fts5_query_term_rank_top100_scored() -> canbench_rs::BenchResult {
    let fx = black_box(setup_query_arm());
    let dense = black_box(fx.probes[0].as_str());
    canbench_rs::bench_fn(|| {
        let hits: Vec<(i64, f64)> = with_query(|conn| {
            let mut stmt = conn.prepare(
                "SELECT rowid, bm25(docs) FROM docs WHERE docs MATCH ?1 ORDER BY rank LIMIT 100",
            )?;
            stmt.query_all(params![dense], |row| {
                Ok((row.get::<i64>(0)?, row.get::<f64>(1)?))
            })
        });
        black_box(hits);
    })
}
