# Text index

Last updated: 2026-08-26 (plan 0297 docs-sync)
Status: **Implemented (v1)** — engine accepted ([ADR 0077](../adr/0077-text-index-engine.md));
canister wired and lifecycle-verified on PocketIC (plan 0294, 2026-08-24); density-matched hot
stores + whole-path term-search bench (plan 0295); driver economics closed (plan 0296). Plan
0297 (landed 2026-08-26) wired the end-to-end surface: `CREATE TEXT INDEX` provisioning +
Router catalog/admission, migration-driven backfill (cursor-resumable across Router upgrades;
seal gates on scan-done AND flushed watermark; PocketIC-proven), graph-shard DML pending sync
(≤2 MiB acked batches), planner `TextScan` lowering (top-k and threshold modes) executed through
same-subnet composite queries. Recorded v1 limitations are listed under implementation notes
below; fuzzy/phrase/trigram stay in Non-goals. Physical kernels live in `ic-stable-text-postings`.

Implementation status notes (deviations from the original sketch, all recorded at plan 0294
docs-sync unless noted):

- Pending ops are an `ic-stable-vec-deque` FIFO of arena locators (plan 0295); op payloads are
  candid-encoded in the shared blob arena. The former `StableBTreeMap<u64 seq, Op>` is gone
  (`StableLog` had no bounded drain; the deque makes bounded drain natural).
- Layout version 3 (plan 0295): posting codecs carry an inline bi-level skip trailer and hot-path
  regions moved off stable B-trees; layouts 1–2 fail loudly at open — fresh state required
  (pre-production rule).
- v0 keeps one active segment plus a registry marker; multi-segment levels land with merge
  scheduling work.
- Scoring is weight × tf via docid-aligned block-max parts; tf→part application is inline in the
  driver via a caller-built lookup table (lazy scoring, no eager list decode). The full fixed-point
  BM25 formula lands when the catalog owns per-term weights.
- Production target is wasm32 (root `.cargo/config.toml`; SIMD enabled), matching canbench.
- E2E cycle observations (PocketIC, single calls): ingest of 1 doc ≈ 8.3 M cycles (fixed overhead
  dominated — do not extrapolate linearly), flush-to-done ≈ 9.0 M, merge-to-done ≈ 26 M on the
  M=242 lifecycle fixture.
- Layout version 4 (plan 0297 backfill-pull): adds durable regions 14/15 — the backfill
  registration cell + resumable cursor cell, bound by the backfill module through the shared
  `state::region()` accessor. Layouts 1–3 fail loudly at open; fresh state required.
- DML sync (plan 0297 dml-pending-flush): ops enqueue into a volatile catalog-gated queue;
  the maintenance timer ships deterministic ≤2 MiB batches to `ingest_text`/`delete_docs`;
  the ack watermark advances only after confirmed delivery and failures requeue the suffix
  for idempotent key-based replay. There is no durable journal variant yet: unconfirmed ops
  do not survive a canister upgrade (converges via next write or backfill).
- Known v1 limitations: `DROP TEXT INDEX` removes the catalog definition and fails closed
  while builds are active, but does not tear down the physical canister; multi-shard fan-out
  is deferred (single home shard).
- Compound lowering (plan 0329): a combined `WHERE text_score(v.prop, $q) cmp t` +
  `ORDER BY text_score(v.prop, $q) DESC LIMIT k` predicate fuses into ONE
  `TextScan { mode: ThresholdTopK }` when both halves reference the same
  (variable, property, query); the Router retains the threshold on the score-ranked canister
  window first, then truncates to the literal limit (top-k of the threshold-filtered set).
  A mismatch on any of (variable, property, query) never fuses and fails closed downstream.
  Literal-LIMIT parity with the top-k mode holds; the threshold bound may be a literal or a
  parameter. Canister-side `min_score` pushdown (shrinking the Router window for highly
  selective thresholds) is recorded as a later optimization — Router-side filtering is
  already correct.

## Purpose

Define the contract for the Text Index canister: an optional derived search service over labeled
vertex properties (edges later), provisioned by `CREATE TEXT INDEX` and queried through the
`text_score(prop, query)` scalar ([extension-syntax.md](../gql/extension-syntax.md)). Engine
decision, alternatives, and measured
evidence live in [ADR 0077](../adr/0077-text-index-engine.md) and
[the research investigation](../investigations/2026-08-23-text-index-design-research.md); this
document records the steady-state contract those imply.

## Ownership boundaries

| Concern | Owner |
|---|---|
| Index definitions, analyzer identity, activation/readiness | Router (catalog), mirroring Property/Vector patterns |
| Canonical property values | Graph shards (unchanged; text index is derived state) |
| Tokenization + normalization (analyzer) | Text canister (analyzer module; pluggable identity registered in the definition) |
| Segments, dictionary, postings, tombstones, stats, merges | Text canister |
| Scoring formula + weights | Index definition catalog; supplied to the physical layer as precomputed score parts |
| Fan-out orchestration and result merge | Router (same-subnet composite queries; cross-subnet falls back to parallel updates) |

`gleaph-gql` / `gleaph-gql-planner` never contain engine or analyzer logic.

## Engine shape (fixed by ADR 0077)

- Immutable **segments** (level 0..n) written by timer-driven flushes; readers merge across
  segments, newest wins over tombstones — mid-merge state stays searchable.
- Per-segment dictionary resolves expanded units → dense `u32 term_id` through a two-choice
  linear hash map keyed on dual xxh3_128 digests of the term, with a dense entry array holding
  the canonical string (arena locator) + df for verified-on-hit lookups (plan 0295;
  SplitFstReader deferred until fuzzy/regex requirements land).
- Postings: delta-varint docid sets; interleaved (delta-docid varint + u8 tf capped 255) variant
  where scoring needs frequencies. Uncompressed block-max score tables aligned to 128-doc logical
  blocks.
- Global stats record (ndocs, total units, per-field lengths) updated incrementally on
  flush/merge.
- Tombstones: bitset containers; document update = delete + insert; physical reclaim happens in
  merges.
- Query execution: DAAT disjunctive top-k with whole-total block-max skipping, allocation-free
  driver, integer fixed-point scores, deterministic tie-break (score desc, docid asc). Pruning
  structure reference-ported from RISE (MIT; see investigation D1 note ²).
- All replicated paths (flushes, merges) are fully deterministic: no hash-order iteration, sort
  keys include ids; timers are consensus-executed.

## Analyzers

Two registered pipelines (creation-fixed per index definition; plan 0330 spike + 0331 landing):

- **`unicode_bigram` (id 1, default)** — Unicode segmentation + NFKC + lowercase; CJK character
  runs expand to overlapping bigrams (lone characters stay unigrams); ASCII words whole. Trigram
  indexing is a separate future index kind, not part of v1.
- **`vibrato` (ANALYZER_ID=2, plan 0331)**: vibrato 0.5.2 + ipadic-mecab lemma units — whole-text
  NFKC + lowercase pre-pass, per-line tokenization, ipadic base-form (feature column 6) for
  content words with particles/auxiliaries/symbols dropped. Deterministic and strict-idempotent.
  The dictionary is NOT in the wasm (engine 1.85 MB canister wasm): the pinned ZSTD artifact
  (8.0 MB compressed, about $0.0088/month) lives in stable region 16, uploaded via
  controller-guarded `admin_upload_dict_chunk` (<= 1 MiB/call) + `admin_finalize_dict_upload`
  (xxh3_128 identity verified at finalize; exact replay idempotent), and eagerly decompressed at
  open (ruzstd) into the ~52 MB heap-resident tokenizer. The DDL clause contract lives in
  [extension-syntax.md](../gql/extension-syntax.md).

Selection evidence (plan 0330 spike, measured): vibrato 228 KB engine / 8.0 MB zstd dictionary /
51.8 MB heap / ~400k chars/s vs lindera 48 MB wasm (path-only dictionary API, wasm-embedded-only),
sudachi absent from crates.io + 117 MB dictionary, rule-stemmer smallest but coarse (kanji stems,
no lemmas). Recorded in `plans/0330-text-analyzer-spike.md`.

**Language coverage (measured, plan 0330/0331 cross-language fixtures):**

- `unicode_bigram` is the multilingual baseline: Han runs bigram (Chinese works — standard CJK
  strategy), Latin words whole (no stemming: `running` never matches `run`), kana bigrams.
- `vibrato` is **Japanese-only in practice**: its ipadic lexicon fragments Chinese mid-word
  (知识图谱 → 知/识图; query 数据库 misses) and Korean emits ZERO units (ipadic has no Hangul
  category; tokens are dropped). Measured in
  `crates/text-analyzer-spike/tests/cross_language.rs` — `ANALYZER vibrato` must not be selected
  for Chinese or Korean content.
- Korean is the weakest language today under BOTH pipelines: `unicode_bigram` keeps whole eojeol
  tokens with particles attached (학교에서 never matches a 학교 query), vibrato drops Hangul
  entirely. A future `ANALYZER_ID=3` could be a dictionary-free particle-stripper (Korean 조사
  suffix tables are rule-friendly) or a MeCab-compatible `mecab-ko-dic` model compiled for
  vibrato; both recorded as later slices, not implemented.

**Tier-0 rule analyzer family (recorded design — one plan, not implemented):** the remaining
recall gaps (Korean 조사, English stemming, Japanese inflection without vibrato) are all
rule-closable without dictionaries. They compose into ONE script-dispatched composite analyzer
(candidate `ANALYZER_ID=3`, name `rule_multilingual` — strategy-named like id 1, no dictionary):
UAX #29 segmentation, deterministic Unicode-script classification per run (no statistical
language detection), then per-script rule layers — Han runs: v1 bigram expansion; kana-tailed
runs: the 0330-measured Japanese stem FSA (走った → +走); Hangul runs: 조사/어미 suffix strip
(closed-class tables with 받침 allomorphy, enumerated irregulars) emitting surface+stem;
Latin words: surface + Porter stem. Surface+stem dual emission keeps precision while adding
recall; postings grow ~1 unit per inflected/Latin word. Deterministic and idempotent by
construction (pure text functions). It does NOT deliver dictionary-grade lemmas (Japanese
走る-grade recall remains ANALYZER_ID=2's role), Chinese word boundaries (bigram remains the
Chinese strategy), or Korean irregular-verb completeness beyond enumerated tables. Korean
조사 stripping + English Porter + Japanese stem FSA are small enough to land as one plan
(working title 0332); the quality upgrade path for Korean stays a vibrato × mecab-ko-dic
spike (reusing the 0331 region-16 dictionary machinery if a dictionary-based analyzer is
ever adopted).

## Lifecycle and lag semantics (mapping onto derived-state contracts)

| Phase | Behavior | Lag class |
|---|---|---|
| DML on indexed text property | Op enqueued into a volatile text-pending queue (catalog-gated); timer flush ships deterministic ≤2 MiB batches to `ingest_text`/`delete_docs`; ack watermark advances only after confirmed delivery; key-based idempotent replay | Under-posted until flush completes |
| Backfill (`CREATE TEXT INDEX` on existing data) | Migration-driven bounded Register/Build steps over frozen canonical export scopes per [ADR 0059 §Text build kind](../adr/0059-create-index-migration-backfill.md) (implemented 2026-08-26, plan 0297); cursor-resumable across Router upgrades; seal gates on scan-done AND flushed watermark; readiness flips exactly once | Invisible to queries (`Backfilling`) until convergence |
| Merge | Level merges run as resumable steps; mid-merge reads see old+new with tombstone arbitration (newer wins) | Over-posted transiently possible; no silent drops |
| Delete/update | Tombstone bitset entry; reclaim deferred to merge | Over-posted until merge |

Reads are query calls (free, 5 B instructions, 1 GiB stable reads) and therefore eventually
consistent, matching the Property-postings rows in
[derived-state-query-semantics.md](derived-state-query-semantics.md), where the `ReadMode`/lag
rows for TEXT are recorded.

## Region map (MemoryId plan, ratified at wiring time)

One `MemoryManager` in the text canister; ≤255 ids; one structure per id. Concrete 16-region
numbering (layout v5, plan 0331) lives in `crates/text-canister/src/state.rs` next to the manager:
meta cell · segment registry map · dictionary probes (linear hash map `u128→u32`) · dense term
entries (canonical string arena ref + df) · postings blob refs · block-max blob refs · dense
doc-key slots · doc-key→docid linear hash map (`u64→u32`) · tombstone container vector · stats
cell · pending-ops FIFO deque (payloads in the shared blob arena) · merge-cursor cell ·
controller cell · shared fixed-chunk blob arena · term-entry vector — plus layout-v4 additions:
backfill registration cell + resumable cursor cell (MemoryIds 14/15, bound by the backfill
module through `state::region()`) — plus the layout-v5 addition: the analyzer-2 ZSTD dictionary
blob (MemoryId 16, bounded 1 MiB chunk slots; upload/finalize contract in the analyzer section).

## Budgets and capacity

Measured @M=2000 docs (seed 20260823 fixture family): build 188.80 M instructions (~94 k/doc),
m3/top-10 query 15.78 M (tf-scored 17.25 M), storage 141–193 KB logical bytes. Formula model,
worked examples, and the soft/hard split thresholds (350/400/450 GiB) carry over from
[capacity-planning.md](capacity-planning.md); TEXT region growth rows are recorded there.

Analyzer-2 landing costs (plan 0331, measured): text-canister wasm 1,745,232 B (pocket-ic build)
/ 1,851,074 B (canbench build) — inside the ~2 MB gate with NO dictionary bytes embedded;
dictionary finalize (8.0 MB zstd decode + tokenizer build) measured 4,752,264,345 cycles on
PocketIC, inside the 300B install budget; heap-resident dictionary after load ~52 MB; transient
decode peak ~675 MB (plan 0330 spike).

## Non-goals (v1)

Phrase/proximity operators (positional postings), highlighting/snippets, fuzzy matching
(SplitFstReader), trigram substring indexes, Router fan-out wiring, edge-property text indexes.
Each has a recorded trigger in the investigation's staging notes.

## Cross-links

- [ADR 0077](../adr/0077-text-index-engine.md) — engine decision and evidence.
- [ADR 0054](../adr/0054-provisioned-logical-graph-topology-and-resource-activation.md) — resource
  topology and remaining partition-strategy open item.
- [ADR 0059](../adr/0059-create-index-migration-backfill.md) — migration-driven backfill to
  extend with a Text kind.
- [capacity-planning.md](capacity-planning.md) — platform limits and threshold framework.
- [property-index.md](property-index.md) / [vector-index.md](vector-index.md) — sibling derived
  services sharing lifecycle vocabulary.
