# Text Index (`CREATE TEXT INDEX`) design research

Date: 2026-08-23 23:29:06 UTC +0000
Status: **Planned / research notes** — no implemented behavior. Feeds the ADR 0054 open-research item
"Text Index implementation and partition strategy, including whether FST is appropriate."
Constraint accepted by the requester: stable-memory ports of any required data structures are
acceptable, but implementations must follow `ic-stable-structures` conventions (memory ownership,
header layout, and `new()` / `init()` constructor semantics).

## Purpose

Evaluate full-text-search index designs for a future Text Index canister against the Internet
Computer platform limits as verified on 2026-08-23 UTC, survey algorithms and prior art, and reduce
the design space to a small set of candidate architectures with concrete stable-port plans.

## Non-goals

- Choosing the final partition/sharding policy (ADR 0054 keeps this open).
- Any change to graph-index regions, Router contracts, or GQL syntax in this document.
- Implementing an analyzer or scoring contract; only candidate shapes are recorded here.

---

## 1. Platform constraints (verified 2026-08-23 UTC)

Source: [IC resource limits](https://docs.internetcomputer.org/references/resource-limits/),
[cycle costs](https://docs.internetcomputer.org/references/cycle-costs/),
[execution errors](https://docs.internetcomputer.org/references/execution-errors/),
[composite queries](https://docs.internetcomputer.org/guides/canister-calls/parallel-inter-canister-calls/),
[large wasm / wasm64](https://docs.internetcomputer.org/guides/canister-management/large-wasm/),
[DTS (execution layer)](https://learn.internetcomputer.org/hc/en-us/articles/34208985618836-Execution-Layer).

| Limit | Value | Design implication |
|---|---|---|
| Wasm heap | 4 GiB (wasm32), 6 GiB (wasm64) | Whole-dictionary heap residency is not viable; heap is a hot tier only |
| Wasm stable memory | 500 GiB per canister | Same ceiling as graph-index planning (soft 350 / hard 400 / critical 450 GiB) |
| Instructions: update / heartbeat / timer | 40 B per message | Segment build and one merge step must fit in one message budget |
| Instructions: install / upgrade | 300 B | `post_upgrade` must not rebuild large in-heap structures |
| Instructions: query call | 5 B | Bounds single-query traversal work; top-k early termination matters |
| Stable read/write per replicated message | 2 GiB / 2 GiB (upgrade: 8 GiB, replicated query: 1 GiB read) | Merge steps are batched exactly like backfill steps |
| DTS | Automatic multi-round continuation of long messages (~2 B instr/round) | Long builds continue across rounds, but resumable cursors remain mandatory |
| Payloads | ingress & cross-subnet 2 MiB; same-subnet inter-canister 10 MiB; responses 2 MiB (3 MiB non-replicated query) | Token batches and result pages must chunk |
| Composite queries | same subnet only, ingress-originated only, never replicated | Fan-out topology must respect subnet co-location (ADR 0054 placement preference) |
| Storage cost | ~$0.45 / GiB / month (13-node); 1 B instructions ≈ $0.00137; queries free | A few GiB of index costs a few dollars/month; serving search as queries is economically dominant |
| Wasm module | 100 MiB total, 10 MiB code section | Embedded analyzer dictionaries compete with this budget (see §4.5) |
| Snapshots | 10 per canister | Large indexes rely on derived-state rebuild instead of snapshot backups |

Additional behavioral facts:

- Timer executions are **replicated** (system API mode table). All merge/build logic that runs from
  timers is consensus-executed and therefore must be fully deterministic (no hash-order iteration;
  sort keys must include ids).
- The IC team identifies unbounded per-message heap access as the reason large heaps stay capped
  ([forum: wasm64 heap >4 GB](https://forum.dfinity.org/t/wasm64-and-the-expansion-of-heap-memory-past-4gb/40150));
  "keep the whole index in the heap" works against platform direction.

## 2. Prior art on ICP

| Prior art | Shape | Lesson for Gleaph |
|---|---|---|
| BigSearch (motoko-sequence, 2020) — [repo](https://github.com/matthewhammer/motoko-sequence/blob/master/service/BigSearch.mo) | Trie keyword search in Motoko | Tries run fine on-canister but cover only prefix/keyword needs |
| ic-rusqlite (WASI SQLite) — [repo](https://github.com/wasm-forge/ic-rusqlite) | rusqlite over wasi2ic; reported 80 GB database with indexed queries | Viable "don't build it" path; instruction-limit discipline still required |
| ic-sqlite-vfs (2026) — [forum](https://forum.dfinity.org/t/ic-sqlite-vfs-sqlite-without-wasi-dependencies/72955), [FTS5 test](https://docs.rs/crate/ic-sqlite-vfs/latest/source/tests/fts5.rs) | SQLite page I/O mapped directly onto a MemoryManager-compatible stable layout; FTS5 `MATCH` verified by tests | Strongest alternative to a custom engine; keeps a `MemoryId` reserved like stable structures do |
| SQLite-in-one-canister showcase (2026-02) — [forum](https://forum.dfinity.org/t/full-stack-sqlite-powered-website-and-search-engine-in-one-canister/64723) | Free-text search via FTS5 in a single canister | Small/mid scale demonstrably works |
| Forum FTS threads — [example](https://forum.dfinity.org/t/full-text-search/11382) | Client-built indexes rejected (tampering); background indexing canisters proposed | Matches Gleaph's SSOT/derived-state stance |

No production-grade general-purpose FTS canister exists as of 2026-08-23 UTC.

## 3. Architecture candidates

### 3.1 Custom segment-LSM inverted index (recommended direction)

Lucene/Tantivy/SQLite FTS5 share one shape: immutable segments written on commit, queried by
merging per-segment results (newer wins over tombstones), and merged into larger levels by a
background policy.

- FTS5 internals confirm the fit: per-write new "segment b-trees", level-based automerge /
  crisismerge, doclist indexes for long postings, a global averages record
  ([FTS5 docs](https://sqlite.org/fts5.html)).
- Tantivy documents the same segment model and log-style merge policies
  ([tantivy docs](https://docs.rs/tantivy/latest/tantivy/),
  [indexing notes](https://fulmicoton.com/posts/behold-tantivy-part2)).

Why it fits ICP specifically:

1. Segment construction is a bounded job → fits 40 B instructions / 2 GiB writes per message.
2. Merges decompose into resumable cursor steps → identical to Gleaph's backfill-step contract.
3. Search reads immutable data → pure query calls (free, 5 B instructions, 1 GiB reads).
4. Mid-merge state stays searchable (tombstone/newer-wins semantics) → maps to Gleaph's
   under-posted / over-posted lag vocabulary rather than requiring atomic swaps.

### 3.2 SQLite + FTS5 via stable VFS

ic-sqlite-vfs demonstrates SQLite pages directly on stable memory with working FTS5. Trade-offs:
analyzer/scoring contracts would be owned by SQLite configuration rather than Gleaph; GQL/planner
integration and derived-state lifecycle (attach/backfill/split) need custom glue; the wasm32 heap
hosts SQLite's page cache. Keep as the PoC comparison baseline (§7).

### 3.3 Trie-based engines (BigSearch style)

Simple and cheap, but no ranking infrastructure, no compression story at hundreds of millions of
postings, and phrase/fuzzy support is ad hoc. Suitable only for tiny deployments.

### 3.4 Off-chain search service

Rejected: contradicts Gleaph's on-chain derived-state ownership model.

## 4. Component survey

### 4.1 Term dictionary

Findings:

- Lucene's BlockTree terms dictionary is blocks of ~25–48 terms plus a RAM-resident FST mapping
  term → block pointer; FST *reading* was made off-heap years ago but *construction* memory remains
  a known pain point being addressed upstream
  ([McCardless](https://blog.mikemccandless.com/2010/12/using-finite-state-transducers-in.html),
  [OpenSearch off-heap FST construction issue](https://github.com/opensearch-project/project-website/issues/3991)).
  "FST = small" is misleading; construction footprint is the real constraint — directly relevant to
  a bounded canister heap.
- The `fst` crate supports constant-memory streaming construction over sorted input and Levenshtein
  automaton search, but assumes mmap-style random access and warns about 32-bit address caps
  ([transducers](https://burntsushi.net/transducers/), [docs.rs/fst](https://docs.rs/fst/latest/fst)).
  On a canister there is no mmap; either host each segment's FST in heap (small segments only) or
  port a stable-backed reader.
- Succinct alternatives (MARISA / LOUDS tries) reach ~5–10% of raw text size with predictive
  search, but are static structures — again pointing to per-segment adoption
  ([marisa-trie](https://github.com/pytries/marisa-trie),
  [LOUDS overview](http://www.eecs.tufts.edu/~aloupis/comp150/projects/SuccinctTreesinPractice.pdf)).
- A plain ordered dictionary also works: FTS5 resolves terms through a B-tree keyed by
  `(segment, term-prefix)` without any FST. FSTs become necessary for fuzzy (Levenshtein automaton
  intersection) and regex navigation, and for compression at very large vocabularies.

Design consequences:

- **Split FST**: one dictionary per segment (what Lucene/Tantivy actually do). Heap exposure =
  active small-segment dictionaries only; large-segment dictionaries stream from stable memory.
  This resolves the ADR 0054 question "whether FST is appropriate": yes, but split-per-segment and
  optionally deferred (v0 can ship a B-tree dictionary).
- **term_id indirection**: store `term bytes → u32 term_id` once per segment; postings key off
  `term_id`. This mirrors the planned value-bucket + ordinal-tail indirection in
  [capacity-planning.md](../index/capacity-planning.md) and avoids variable-length keys in
  stable B-trees (see RUSTSEC-2024-0406 on unbounded-key node behavior:
  [advisory](https://rustsec.org/advisories/RUSTSEC-2024-0406.html)).

### 4.2 Postings encodings

| Encoding | Property | Verdict |
|---|---|---|
| Delta + fixed-block bit packing (Lucene FoR, 128–256/block) | Fastest sequential scan; battle-tested ([Lucene encodings](https://towardsdatascience.com/lucene-inside-out-dealing-with-integer-encoding-and-compression-fe28f9dd265d/)) | Safe default; scalar-only, no SIMD dependency |
| Simple-8b / Stream VByte / PFor family (PISA catalog) | Compression/speed spread ([PISA](https://pisa-engine.github.io/pisa/book/guide/compressing.html)) | Optional variants behind one trait |
| Partitioned Elias-Fano (SIGIR 2014) | Near-optimal space with fast random access and `advance(target)`; exploits clustered docIDs (≥23–64% smaller than plain EF on Gov2/ClueWeb) ([paper](https://pages.di.unipi.it/rossano/assets/pdf/papers/SIGIR14.pdf)) | Preferred where skipping matters (top-k) |
| Roaring bitmaps | Best set algebra / random membership ([practical overview](https://junaid.foo/posts/roaring-bitmaps-inverted-indexes/)); note Lucene uses them for caches, not on-disk postings ([SO answer](https://stackoverflow.com/questions/58419937/can-i-use-roaring-bitmaps-for-lucence-inverted-index)) | Tombstones and filter composition, not posting storage |

### 4.3 Updates: segments, merges, tombstones

1. DML-derived token deltas land in a durable pending queue (StableLog) — same lifecycle shape as
   graph-index pending/outbox flush.
2. Timer-driven flush closes a level-0 segment (bounded by instruction/write budgets).
3. Level-based merges run as cursor-resumable update steps; each step re-reads its cursor from
   stable memory first (crash-safe, deterministic).
4. Deletes are tombstones filtered at query time and reclaimed during merges; document updates are
   delete+insert. FTS5's "reader merges segments, newer wins" makes mid-merge state correct.
5. Everything merge-related runs under replicated execution ⇒ deterministic ordering throughout.

### 4.4 Query processing and scoring

- BM25 may use f32 (deterministic on ICP), but fixed-point integer scores simplify cursor
  pagination, tie-breaking, and cross-shard merge contracts. Pick exactly one and own it in the
  index definition catalog.
- Global statistics (df sums, total length, ndocs) live in one record updated incrementally on
  flush/merge (FTS5 "averages record" analog).
- Early termination: WAND → Block-Max WAND using uncompressed per-block max scores stored beside
  compressed postings ([Ding & Suel SIGIR 2011](https://research.engineering.nyu.edu/~suel/papers/bmw.pdf)).
  PEF's native `advance(target)` composes naturally with block-max skipping within the 5 B
  instruction query budget.
- Pagination: `(score, docid)` cursors with deterministic tie-break; huge hit lists degrade to
  conjunctive pre-filter + rerank if needed.
- Phrase search requires positional postings (size jump) — defer to v3; v1 is bag-of-words.

### 4.5 Tokenization (incl. Japanese)

| Option | Cost | Notes |
|---|---|---|
| Unicode segmentation + NFKC/lowercase | ~zero | Baseline for all analyzers |
| Character bigram (CJK) | index ≈ ×1.5–2.5 | Dictionary-free; IR studies find bigram retrieval comparable to word segmentation (HathiTrust analysis; Lucene `CJKBigramFilter` is the reference implementation: [filter](https://lucene.apache.org/core/8_6_3/analyzers-common/org/apache/lucene/analysis/cjk/CJKBigramFilter.html), [study](https://old.www.hathitrust.org/blogs/large-scale-search/multilingual-issues-part-1-word-segmentation.html)) — recommended default for Japanese |
| Morphological analyzer (lindera) | IPADIC embedded adds ~13 MB to the wasm; UniDic ~47 MB ([size comparison](https://anila.me/en/blog/benchmarks-and-trade-offs-for-japanese-morphological-analyzer)); wasm module cap is 100 MiB | Opt-in feature flag; lemmatization improves recall for inflected forms |
| Trigram (pg_trgm style) | index ≈ ×3 | Enables substring/LIKE `%xx%` and typo similarity via inverted trigram sets ([pg_trgm](https://www.postgresql.org/docs/18/pgtrgm.html)) — optional second index kind, not the default text index |

Analyzer identity is part of the index definition (Router-owned catalog). Analyzer code lives in
the future text-index crate — never inside `gleaph-gql` / `gleaph-gql-planner` (generic-GQL purity
boundary).

### 4.6 Partitioning and fan-out

- Composite queries cannot cross subnets, so scatter-gather fan-out requires all target text
  canisters on one subnet (matches ADR 0054 co-location preference); otherwise fall back to
  parallel update calls or client-side merge.
- Shard axis: by document range (graph shard id) with partial top-k merged deterministically at
  the router layer. Term-based partitioning creates hot-term problems and is not recommended.
- Capacity thresholds reuse capacity-planning.md's soft/hard/critical bands.

### 4.7 Heap tier assessment ("heap-centric")

Heap residency is acceptable only for: active small-segment dictionaries, block-max arrays for
in-flight queries, cached global stats, and top-k heaps. Anything larger must survive upgrades
without reconstruction (300 B instruction post-upgrade cap) — i.e., belong in stable memory. This
matches Gleaph's existing rule that query paths must not materialize full buckets in heap.

## 5. Stable-port plan under `ic-stable-structures` conventions

Verified against ic-stable-structures **0.7.2** docs on 2026-08-23 UTC
([crate](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/),
[BTreeMap](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/btreemap/struct.BTreeMap.html),
[Cell](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/cell/struct.Cell.html)).

### 5.1 Library conventions that bind the implementation

1. **Memory ownership**: stable structures must not share memories. One structure owns exactly one
   virtual memory obtained from `MemoryManager::init(DefaultMemoryImpl::default()).get(MemoryId)`
   (≤255 ids). Every structure below gets its own `MemoryId`.
2. **Constructor trio semantics** (must be mirrored by any custom structure):
   - `new(memory)` — create fresh, assuming the memory is exclusively reserved and starts at
     address zero (typically used with `RestrictedMemory`); `BTreeMap`'s layout after `new` is
     `| BTreeHeader | Allocator | free |`.
   - `init(memory, …)` — load-or-create: if the memory already holds compatible data it is loaded;
     otherwise defaults are written (e.g. `Cell::init(memory, default_value)` loads the existing
     decoded value when present).
   - `load(memory)` — load without creation checks (advanced use with external validation).
3. **Storable contract**: `to_bytes(&self) -> Cow<[u8]>`, `into_bytes(self) -> Vec<u8>`,
   `from_bytes(Cow<[u8]>) -> Self`, and `const BOUND: Bound` where
   `Bound::Bounded { max_size, is_fixed_size }` enables node-layout optimizations but
   **max_size can never grow after deployment** (documented corruption risk). Fixed-shape records
   use `Bounded { max_size, is_fixed_size: true }`; variable-size blobs use `Bound::Unbounded`
   and evolve via layout versions, not bound growth (pre-production simplicity: fresh state on
   layout changes, no migration branches).
4. **Memory abstraction**: everything is generic over `Memory`; unit tests run on `VectorMemory`
   with no PocketIC involvement. Production wires `DefaultMemoryImpl`.
5. **GrowFailed**: allocation growth returns errors that callers must surface, not panic paths.

### 5.2 Proposed region map (text canister, own MemoryManager)

Illustrative `MemoryId`s (final numbering belongs to the implementing slice):

| MemoryId | Structure | Content |
|---|---|---|
| 0 | `StableCell<TextIndexMeta>` | magic/layout version, analyzer id, config counters |
| 1 | `StableBTreeMap<SegmentKey, SegmentMeta>` | segment registry (level, bounds, counts, dict/posting offsets) |
| 2..=k | per active segment: `PostingBlockStore` | compressed postings (+ block-max scores) |
| k+1.. | per active segment: `SegmentTermDict` | term_id ↔ term bytes + df/stats |
| n-3 | `StableCell<GlobalStats>` | ndocs, total tokens, per-field lengths |
| n-2 | tombstone bitmap store | deleted-docID containers |
| n-1 | `StableLog<PendingOp>` | durable pending deltas |
| n | `StableCell<MergeCursor>` | resumable merge position |

(255-id budget is ample because only *active* segments own dedicated memories; merged-away segments
release their ids.)

### 5.3 Reuse as-is

| Need | Existing structure | Notes |
|---|---|---|
| Metadata, stats, merge cursor | `StableCell<T>` | `init(memory, default)` load-or-default |
| Segment registry | `StableBTreeMap<K, V>` | bounded fixed-size key/value both sides |
| Pending delta queue | `StableLog` | append-only variable entries |
| Tombstone containers | `StableBTreeSet<u64, Blob>` or custom §5.4 | container granularity keeps values fixed-size |
| Ordered side structures | `StableVec`, `StableMinHeap` | block tables, merge heaps if needed |

### 5.4 New structures to port (following library idioms)

Each is generic over `M: Memory`, unit-tested on `VectorMemory`, and exposes the
`new` / `init` / `load` trio with header-first layouts:

```text
PostingBlockStore<M: Memory>
  header: magic(u32) | layout_version(u32) | block_count(u64) | last_block_len(u32)
  body:   concatenated encoded blocks (PEF or FoR) + block-max score table
  new(memory)            // fresh; traps-not-used pattern of BTreeMap::new applies
  init(memory)           // load-or-create via header magic/version check
  load(memory)           // unchecked load
  append_block(&mut self, block: &[u8], block_max: u32) -> Result<(), GrowFailed>
  iter_from(doc_hint)    // advance()-capable cursor for BMW

SegmentTermDict<M: Memory>
  variant A: two StableBTreeMaps (term_id -> {df, offset}; term_hash -> term_id) + blob log
  variant B: sorted-term blob + sparse offset index (FST-shaped)
  constructors mirror the trio; term bytes never become raw B-tree keys

TombstoneBitmap<M: Memory>
  roaring-style containers: key u16-high-bits -> fixed 8 KiB bitmap container (Bounded, is_fixed_size=true)
  insert/remove/contains; iteration in ascending doc order for merge reclaim

SplitFstReader<M: Memory>            // v2, only if fuzzy/regex navigation is required
  stable-backed automaton traversal over a per-segment FST image built by constant-memory
  streaming construction from sorted terms
```

Header/versioning rule: every custom structure starts with `magic | layout_version`, matching how
stable structures validate compatibility on `init`; incompatible versions trap loudly instead of
silently migrating (Gleaph pre-production simplicity: reinstall/rebuild rather than migrate).

### 5.5 Determinism and upgrade rules

- No structure relies on HashMap/hash iteration anywhere in replicated paths.
- `post_upgrade` only re-inits structures from their memories (`init` calls); no bulk rebuild.
- Layout version bumps require fresh canister state (rebuild via backfill) — consistent with the
  repository's pre-production simplicity rule.

## 6. Capacity model draft (planning-grade)

Symbols: N vertices indexed, ā avg tokens per indexed value, T = N·ā token instances,
U distinct terms, p = compressed bytes per posting pair (0.7–2.5 B typical for FoR/EF blocks),
η = B-tree/page overhead factor (reuse η=2.0 convention).

```text
S_dict      ≈ U · (avg_term_bytes + 12)        // term bytes + df/offset record
S_postings  ≈ T · p + (T / block_size) · 4     // pairs + block-max scores
S_text      ≈ η · (S_dict + S_postings) + S_meta
```

Worked examples (η = 2.0, block_size = 128, p = 1.2 B, CJK bigram inflation ×1.8 folded into U/T):

| Scenario | N | ā | T (post-bigram) | S_text est. | vs soft 350 GiB |
|---|---|---|---|---|---|
| Social profiles | 10^7 | 20 | 3.6×10^8 | ~0.9 GiB | safe |
| Catalog descriptions | 10^8 | 40 | 7.2×10^9 | ~17 GiB | safe |
| Heavy corpus | 5×10^8 | 60 | 5.4×10^10 | ~130 GiB | plan split before growth |

Index-build instruction estimate: ~300–800 instructions per token instance ⇒ the heavy corpus row
(~8×10^10 tokens incl. bigrams) costs on the order of $0.11–0.30 one-time at $0.00137 per billion
instructions, delivered across ≥ tens of update messages (budget-bounded). These numbers are
planning estimates until measured by canbench.

## 7. Validation plan

- Structure-level unit tests on `VectorMemory` (no PocketIC): round-trips, header-version traps,
  crash-resume of merge cursors, determinism assertions.
- canbench patterns (focused, in the eventual crate): `bench_postings_walk`, `bench_merge_step`,
  `bench_topk_bmw`; unfiltered `canbench --persist` only when updating final artifacts.
- PocketIC E2E target sketch: `text_index_lifecycle` (create → ingest → lag semantics → drop) and
  `text_index_backfill_resume`; run via ordinary `cargo test -p gleaph-pocket-ic-tests --test <name>`.
- Decision benchmark vs alternative: same fixture corpus indexed through ic-sqlite-vfs FTS5 to
  compare instruction/storage envelopes before committing to the custom engine.

### First-round PoC results (measured 2026-08-24 UTC)

Implemented as `crates/ic-stable-text-postings` (physical-layer PoC crate; plan 0293). Fixture:
deterministic corpus seed 20260823, 300k docs, vocab 2048 (mixed ASCII/Japanese), Zipf lists
A(dense)/B/C/D; block-max tables aligned to 128-doc logical blocks. Environment caveats: bench
wasm is **wasm32 + SIMD-enabled**, matching the repository production target (root
`.cargo/config.toml` / `icp.yaml`; wasm64 is retained only as a possible future re-switch), so
instruction figures map directly onto production architecture. Kernels are heap-buffer backed,
so stable-read system-API overhead is **not** included yet; baseline persisted in the crate's
`canbench_results.yml` after one pathology fix round (the initial EF reader re-ran a stateless
unary select per element — fixed to an incremental cursor, all correctness gates unchanged).

| Benchmark (list A unless noted) | Instructions | Note |
|---|---|---|
| `bench_postings_walk_varint` | 31.92 M | ~109 instr/id |
| `bench_postings_walk_ef` | 54.29 M | ~186 instr/id after cursor fix |
| `bench_postings_walk_for` | 119.05 M | ~408 instr/id |
| `bench_postings_walk_pef` | 328.89 M | two-level indirection costs at this scale |
| `bench_postings_advance_varint` | 14.29 M | 1024 advances |
| `bench_postings_advance_ef` | 25.18 M | |
| `bench_postings_advance_for` | 38.53 M | |
| `bench_postings_advance_pef` | 23.20 M | |
| `bench_merge_step_k4_b8192` | 18.00 M | K=4 mixed codecs, ~2.2k instr/output id |
| `bench_dict_lookup_btree_512` | 14.40 M | StableBTreeMap u64 dict, ~28.1k instr/probe (256 hits + 256 misses) |
| `bench_topk_bmw_m3_top10` | 74.23 M | predeclared working target was ≤50 M |

Verdicts against §9 decision inputs:

- **Q1 encoding ranking**: varint wins both walk and advance in scalar wasm32 at canister-relevant
  list sizes; block codecs pay per-block setup and PEF's two-level indirection does not pay for
  itself in instructions here. Space-per-posting now matters more than decode speed when picking
  anything fancier than varint (+ a separate block-max table).
- **Q2 dictionary**: StableBTreeMap term_id lookups are a workable v0 baseline (~28k instr/probe
  under emulation); FST reader remains deferred until fuzzy/regex navigation is required.
- **Q3 merge steps**: ~2.2k instr/output leaves orders-of-magnitude headroom inside the
  40 B-instruction / 2 GiB-write message budgets; step sizing will be bounded by stable-read
  overhead once stores are stable-backed, not by merge compute.
- **Q4 top-k feasibility**: m=3 disjunctive top-10 at 74.23 M instructions is above the optimistic
  ≤50 M working target but only ~1.5% of the 5 B query-call budget → **feasible**; since varint
  dominates traversal, re-encoding query-side lists as varint is the first optimization lever.

FTS5-on-VFS comparison arm (feasibility spike, findings archived at
[artifacts/2026-08-24-ic-sqlite-vfs-spike-findings.md](artifacts/2026-08-24-ic-sqlite-vfs-spike-findings.md)):
**practical**. `ic-sqlite-vfs` 2.0.0 builds for wasm32-unknown-unknown out of the box with
`sqlite-precompiled` (audited: all wasm imports are `ic0.*`, zero WASI/env), FTS5 is baked into
the artifact, MSRV 1.95.0, dual MIT/Apache-2, no rusqlite/libsqlite3-sys transitive pins; it
reserves no `MemoryId` itself (consumer picks one, README convention `MemoryId::new(120)`) and its
MemoryManager-compatible layout coexists with `ic-stable-structures`. Watch-items: default
`PRAGMA cache_size=-32768` (~32 MiB heap page cache) plus heap-resident commit overlay distort
heap comparisons unless overridden; unicode61 tokenizes contiguous CJK runs as single tokens
(verified: `MATCH '東京'` misses, `'東京*'` hits), so fair Japanese comparison requires
pre-bigrammed text or a custom tokenizer; WAL unsupported; single-maintainer maturity risk.
The 1.91 toolchain clash reported for ic-rusqlite does not apply to ic-sqlite-vfs.

### D1 comparison arm results (measured 2026-08-24 UTC)

Implemented as `crates/text-index-fts5-arm` (bench-only harness; ic-sqlite-vfs =2.0.0
`sqlite-precompiled`; contentless fts5 table; CJK tokens pre-bigrammed on insert so both arms
index equivalent units; `cache_size=-2048`). Corpus identical to the custom arm's fixture family
(seed 20260823), M = 2000 docs. Baseline persisted in that crate's `canbench_results.yml`.

| Workload | FTS5-on-VFS | Custom kernels | Winner |
|---|---|---|---|
| Ingest M=2000 docs | 244.27 M instr (~122 k/doc) | **188.80 M** instr (`bench_build_segment_m2000`, ~94 k/doc) | custom ≈23% cheaper¹ |
| m3 OR top-10 by rank | 34.84 M instr | **15.78 M** varint + ported driver (tf-scored: 17.25 M; frame history: 74.23 M) | custom ~2.2× cheaper² |
| Term lookup top-100 rowids | **261.08 K** instr (whole path) | components only (dict probe ~28 k/probe + walk) | unsettled³ |
| Storage @ M=2000 | 376,832 B logical DB image | **141,355 B** set-mode / 193,109 B freq-mode (incl. dict 42,107 B + block-max 12,856 B; [artifact](artifacts/2026-08-24-storage-parity.txt)) | custom 2.0–2.7× smaller⁴ |

¹ FTS5 ingest includes its internal tokenization; the custom bench starts from unit-id streams
(analyzer lives above this crate) — disclosed asymmetry favoring the custom figure.
² Encoding choice alone accounted for ~19.05 M of the apparent gap (`topk_bmw_m3_top10_varint`
55.18 M vs frame 74.23 M). Code inspection of the remaining ~20.34 M (`src/topk.rs`): the DAAT
loop allocates two `Vec`s, re-sorts frontiers through enum-dispatched `peek()`, and scans the
k-heap linearly on every iteration while this workload touches only ~1.3 k postings total
(~40 k instr/iteration observed) — bench-layer plumbing cost, not posting-decode or scoring
fundamentals; the driver's scorer is also coarser (per-list constant weights, no tf/bm25).
**Closed by the reference port (2026-08-24 UTC):** rewriting the driver allocation-free with a
binary heap and porting the RISE BMW pruning structure dropped the varint workload to
**15.78 M (-71.4% vs the 55.18 M baseline)**; adding tf-carrying `FreqVarintReader` scoring via
fixture-supplied score parts costs only +1.47 M (**17.25 M**, persisted). The historical gap to
FTS5 was therefore entirely encoding choice + driver quality; with both fixed, the custom arm is
~2.2× under FTS5 on this workload.
**RISE extraction spike (2026-08-24 UTC)** validated this path beforehand: RISE's WAND / BMW /
BM-MaxScore layer extracts to **1,072 LOC, std-only, zero nightly**, compiling for
wasm32-unknown-unknown with brute-force-verified correctness on stable Rust
([report](artifacts/2026-08-24-rise-extraction-report.md), dependency inventory:
[inventory](artifacts/2026-08-24-rise-extraction-inventory.md),
[oracle snapshot](rise-queries-min-snapshot/PROVENANCE.md)); whole-crate vendoring remains a
no-go (rayon/memmap2/epserde dependency tree), verbatim reuse would additionally require an
ordered-map fix upstream-side (`query_freqs()` iterates a `HashMap` — non-deterministic term
order under replicated execution) — our reference port sidesteps both by keeping integer scores
and crate-local state only.
³ Not directly comparable yet; the custom dictionary probe cost suggests a flat-blob+skip
dictionary (FTS5 `%_idx` style) plus read caching as the closing lever.
⁴ Set mode is the current docid-set contract; freq mode adds u8 tf per posting (bm25-compatible).
Both include real encoder outputs, headers, dictionary, and block-max tables over the full
expanded corpus (units=3003, postings set/tf = 63,766/75,275). SMI fields on both arms read
structural-zero (bucketed memory-manager growth outside closures); byte figures above are the
honest storage numbers ([parity artifact](artifacts/2026-08-24-storage-parity.txt)).

Remaining caveats: probe-term bands differ slightly ({0,84,1000} vs {53,83,1000}) — same
workload class, not identical lists; both arms exclude host-side memory-op internals equally
(canbench counts wasm instructions); bm25() inside SQLite uses floats while the custom contract
leans fixed-point (§9-D4); heap-buffered custom kernels carry no stable-read overhead yet.

**Interim D1 verdict (final for this PoC):** every measured workload sits far inside ICP budgets
for both engines, and the measurable edges now split decisively — custom wins ingest (~23%),
storage density (2.0–2.7×), **and ranked-query instructions (~2.2× under FTS5 after the
reference-port closed the driver/encoding gap)**; FTS5 retains ecosystem maturity (BM25, token
filters, aggregation surface) but carries a heavier dependency posture. The remaining
ADR-deciding factors are ownership (analyzer/scoring contracts, JP bigram control), integration
with Gleaph's split/backfill/lag machinery, storage economics at scale, and long-term dependency
posture.



## 8. Risks and open questions

- `fst` crate cannot be used unchanged (mmap assumption); SplitFstReader is a genuine port —
  quantify effort before promising fuzzy search in v2.
- Scalar posting decode cost on wasm32 is now measured (§7 first-round results): varint is the
  instruction-cost winner; the former "unmeasured EF cost" risk is closed.
- Timer-driven merge duty cycles on shared subnets need measurement (neighbor fairness).
- Whether PostingBlockStore should live beside graph-index's planned ordinal-tail optimization
  (shared crate) or stay text-local — architecture-integrity decision at ADR time.
- Snapshot/export tooling for multi-GiB indexes (derived-state rebuild assumed sufficient).
- ic-sqlite-vfs single-maintainer maturity and its ~32 MiB default page cache if the FTS5 arm is
  pursued (see §7 results).

## 9. Decision points for the future ADR

1. Engine choice: custom segment-LSM vs SQLite+FTS5-on-VFS — **decided** in
   [ADR 0077](../adr/0077-text-index-engine.md) (**accepted** 2026-08-24): custom segment-LSM with
   varint postings, RISE-derived block-max driver, and integer fixed-point scores; FTS5-on-VFS
   measured and rejected (D1 table above), retained as a future comparison arm. Query-driver
   strategy settled separately: reference-port BMW / BM-MaxScore pruning from RISE (MIT;
   1,072-LOC std-only extraction validated) into our own integer-score driver — whole-crate
   vendoring rejected on dependency grounds; RISE's codecs remain a future consolidation option
   at L effort (~6.3 k LOC strip).
2. Dictionary: B-tree-only v0 vs SplitFstReader in v2 — B-tree baseline now has a cost number
   (Q2); fuzzy requirement owner still decides v2.
3. Postings encoding: **evidence favors plain delta-varint (+ separate block-max table)** over
   FoR/EF/PEF on instruction cost (Q1); space-per-posting is the counterweight to weigh at ADR.
4. Scoring representation: fixed-point integer BM25 (recommended) vs f32 — pick one, own it.
5. Analyzer default: unicode + CJK-bigram; lindera opt-in; trigram as separate index kind —
   reinforced by the spike's unicode61 CJK-run finding (pre-bigramming required for fair FTS5
   comparison too).
6. Region map ratification and ADR 0007-style inventory entry for the text canister.

## Cross-links

- [ADR 0054](../adr/0054-provisioned-logical-graph-topology-and-resource-activation.md) — Text Index
  as optional resource; open research item this document feeds.
- [capacity-planning.md](../index/capacity-planning.md) — platform limit table source of truth;
  ordinal/value-bucket indirection pattern reused by term_id scheme.
- [derived-state-query-semantics.md](../index/derived-state-query-semantics.md) — lag vocabulary
  and composite-query ReadMode constraints reused for text reads.
- [property-index.md](../index/property-index.md) / [vector-index.md](../index/vector-index.md) —
  sibling derived services sharing attach/backfill lifecycle.
- [extension-syntax.md](../gql/extension-syntax.md) — `FULLTEXT INDEX` syntax sketch.
- [ADR 0007](../adr/0007-stable-memory-layout.md) — region inventory precedent for the text
  canister's own MemoryId table.

## Sources (accessed 2026-08-23 UTC)

Platform: resource-limits, cycle-costs, execution-errors, composite-queries guide, large-wasm/wasm64
guide, execution-layer (DTS) article, wasm64 forum thread — links inline in §1.

Algorithms: fst/transducers blog and docs; McCardless FST posts; OpenSearch off-heap FST issue #3991;
Partitioned Elias-Fano (SIGIR'14) paper; PISA guide; Ding & Suel Block-Max WAND (SIGIR'11);
Lucene integer-encoding walkthrough; Roaring practical overview; Lucene RoaringDocIdSet SO thread;
SQLite FTS5 specification; tantivy docs and indexing blog series; marisa-trie; LOUDS succinct-tree
overview; Lucene CJKBigramFilter API; HathiTrust multilingual segmentation study; lindera wasm size
comparison; pg_trgm documentation — links inline in §3–§4.

ICP prior art: motoko-sequence BigSearch; wasm-forge ic-rusqlite and ic-sqlite-vfs (incl. FTS5 test);
forum threads cited in §2. Library conventions: ic-stable-structures 0.7.2 crate/BTreeMap/Cell docs
cited in §5.
