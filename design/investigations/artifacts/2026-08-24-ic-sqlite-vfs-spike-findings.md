# ic-sqlite-vfs FTS5 feasibility spike — findings (plan 0293 slice 4)

Date: 2026-08-24. Scratch-only spike; zero gleaph repo changes (all work under this directory).

## Q1 — Where it lives, license, MSRV

- crates.io: `ic-sqlite-vfs` **2.0.0** (2026-06-26). Older lines: 1.0.1, 1.0.0, 0.2.0–0.2.2; 0.1.x yanked.
- GitHub: https://github.com/humandebri/ic-sqlite-vfs (single maintainer `humandebri`, forum user `hude`).
- License: MIT OR Apache-2.0. Edition 2021. **MSRV `rust-version = "1.95.0"`** (Cargo.toml).
- Deps: candid ^0.10, ic-cdk ^0.20.1, serde ^1, thiserror ^2; build-dep cc ^1.2; dev-dep `ic-stable-structures =0.7.0` (exact pin, alias `upstream-ic-stable-structures`, compat tests only).

## Q2 — wasm32-unknown-unknown compile results (exact commands)

```sh
# A) consumer configuration per README — SUCCESS, first try after one trivial API fix
cargo build --target wasm32-unknown-unknown --release
#   dep line: ic-sqlite-vfs = { version = "2.0.0", default-features = false, features = ["sqlite-precompiled"] }
#   → Finished release; spike.wasm = 714.3 kB (cdylib exercising FTS5 migrate/MATCH paths)

# B) default features (sqlite-bundled) for wasm32 — FAILS, documented limitation
cargo build --target wasm32-unknown-unknown --release   # with default features
#   clang --target=wasm32-unknown-unknown -DSQLITE_ENABLE_FTS5 ... -c sqlite3.c
#   fatal error: 'stdio.h' file not found   (no wasm32 C sysroot; bundled is "maintainers" territory)

# C) host-native runtime proof (macOS, sqlite-bundled compiles via cc) — SUCCESS
cargo test -- --nocapture    # in spike-native/
```

Import audit of artifact A (custom section parser, `import_audit.py`):
**total imports: 5, all module `ic0`; zero `env`/`wasi` imports.**

## Q3 — Is FTS5 behind a feature?

No crate feature gates FTS5. It is baked into the SQLite artifact itself:
`vendor/sqlite/build-flags.txt` contains `SQLITE_ENABLE_FTS5` (plus SQLITE_OS_OTHER=1,
SQLITE_THREADSAFE=0, SQLITE_OMIT_WAL/LOCALTIME/LOAD_EXTENSION/SHARED_CACHE, TEMP_STORE=3,
MEMSTATUS=0, API_ARMOR, URI). The same flag list drives both the vendored precompiled
wasm32 archive (`libsqlite3.a`, 977.5 kB, 454 fts5 symbol hits) and any `sqlite-bundled`
rebuild. Runtime confirmation: `SELECT ... FROM pragma_compile_options` returns
`ENABLE_FTS5`; SQLite version reported: **3.51.3**. `CREATE VIRTUAL TABLE ... USING fts5`,
`MATCH ?1 ORDER BY rank` verified working through the crate's `Db` facade.

## Q4 — Stable-memory contract

- The crate does **not** reserve a MemoryId. Consumer picks one (README convention:
  `MemoryId::new(120)`) and must keep it stable forever; inside it: superblock at virtual
  offset 0..64KiB, SQLite image bytes from 64KiB.
- Ships a minimal **MemoryManager-compatible fork** (same stable layout as
  ic-stable-structures 0.7 MemoryManager; MemoryIds 0..=254 usable, 255 reserved).
  It does NOT depend on ic-stable-structures; compatibility with upstream 0.7 is
  tested via dev-deps (`tests/memory_manager_compat.rs`, `_corruption.rs`). Forum post
  by author confirms: narrower API, stable64 backend, reads directly into xRead buffer.
- Co-residency with other ic-stable-structures users is explicitly supported ("the
  MemoryManager-backed path can coexist with other stable structures under the
  application's memory layout"), provided exactly one manager metadata region exists on
  raw stable memory and both sides agree (use `init_strict`).
- Fail-closed init: non-empty foreign image → `ForeignStableMemoryImage`, no rewrite.
- Durability: one update call = one transaction; writes go to a heap overlay until
  COMMIT, then dirty pages + superblock in-place; **no await / inter-canister /
  call_perform inside a transaction**; relies on IC trap rollback.

## Q5 — Red flags for canister use

- **Rust 1.91 issue does NOT apply.** That bug (wasm-forge/ic-rusqlite#2) is a
  wasm32-wasip1 link failure: SQLite C imports `time` from module `env`, ic-cdk 0.18
  imports `time` from `ic0`; wasm-ld ≥1.91 rejects the mismatch. ic-sqlite-vfs targets
  wasm32-unknown-unknown directly (no wasi2ic), pins MSRV 1.95 > 1.91, uses ic-cdk 0.20,
  runs a release gate "wasm import audit: only ic0.*" — reproduced here: 0 non-ic0 imports.
- **No rusqlite/libsqlite3-sys anywhere** (own small C FFI facade because
  SQLITE_THREADSAFE=0 violates rusqlite's thread-safety assumption) → no transitive
  toolchain/version pins from the SQLite binding layer.
- Instruction-limit guidance in README: public surfaces need bounded queries; full scans,
  `LIKE '%…%'`, unbounded ORDER BY, huge blobs flagged; checksum refresh is chunked
  (`db_refresh_checksum_chunk`); reference KV numbers: 10–17M instr per 1000-row write
  workload, ~10M per 1000 point reads.
- **Heap footprint caveats** (matters for canbench heap_increase and the 32-bit heap):
  connections set `PRAGMA cache_size = -32768` (~32 MiB page cache), journal_mode=MEMORY +
  TEMP_STORE=3 → journal/temp live in heap during update transactions; commit overlay is
  heap-resident until COMMIT. Large batch loads will show real heap growth.
- WAL intentionally unsupported; mmap/shared-memory unimplemented; VACUUM = admin only;
  no import/export in 2.0 (existing ic-rusqlite images NOT migratable — fresh start only).
- Maturity risk: single-maintainer project, first public release May 2026, 2.0 contract
  documented (docs/API_STABILITY.md, PUBLIC_API_2_0.snapshot, fuzz + PocketIC upgrade
  persistence tests claimed in README).

## Q6 — Verdict

**PRACTICAL.** An FTS5-on-VFS comparison arm inside a canbench-measurable wasm32 crate is
viable: consumer config is one dep line (`default-features = false, features =
["sqlite-precompiled"]`), the wasm links with only ic0 imports (same profile as our
existing canbench bench crates → instrumentation should work unchanged), and FTS5 is
already enabled in the shipped artifact. Not measured here (brief forbids canbench runs);
treat "works under canbench instrumentation" as high-confidence inference pending Slice 5.

Minimal wiring sketch:

- New bench-arm crate (e.g. `crates/ic-sqlite-ftx5-bench-arm`) mirroring our
  `ic-stable-vector-page-store` canbench template: `canbench.yml`, `cdylib+rlib`,
  `canbench` feature gating.
- `[dependencies] ic-sqlite-vfs = { version = "2.0.0", default-features = false,
  features = ["sqlite-precompiled"] }`.
- One `thread_local` `MemoryManager<DefaultMemoryImpl>` using the crate's re-exported
  types; SQLite owns a dedicated `MemoryId` (e.g. `new(120)`); in the canbench harness the
  arm owns the whole fresh canister state, so no co-residency conflicts arise. For future
  co-resident production use: initialize one manager (strict mode) and allocate disjoint
  MemoryIds per structure (postings/dict keep theirs; SQLite gets its own).
- Fixture: seed via `Db::update` inserts into `CREATE VIRTUAL TABLE docs USING fts5(...)`
  using the same Zipf corpus generator semantics as `CORPUS_SEED=2026_0823` (copy or share
  `corpus.rs`). Benches mirroring ours: bulk index (≈ merge_step analog), ranked top-10
  MATCH query (≈ topk_bmw_m3_top10 analog), point lookup.
- Watch heap_increase: 32 MiB default page cache dominates small-DB benches; consider
  overriding `PRAGMA cache_size` per connection if we want apples-to-apples memory numbers.
- Tokenizer note: default unicode61 treats a contiguous CJK run as ONE token (verified:
  MATCH '東京' misses '東京タワーは高い', prefix '東京*' hits). For the Japanese half of our
  corpus a fair comparison needs either the trigram tokenizer, custom tokenizer, or feeding
  pre-bigrammed text (mirrors our `expand_bigrams`).

Fix-attempt ledger (time-box ≤3): (1) `MemoryManager::init` is infallible in the mini
fork (dropped `.expect`) — trivial; (2) pragma reports `ENABLE_FTS5` without the
`SQLITE_` prefix (assertion string fix) — trivial. No hard blockers hit.

Scratch layout: `spike/` (wasm32 precompiled link proof + import_audit.py),
`spike-bundled/` (bundled-wasm failure record), `spike-native/` (runtime FTS5 proof test).
