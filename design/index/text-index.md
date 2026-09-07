# Text index

Last updated: 2026-09-07 (plan 0340 docs-sync)
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
- (plan 0295): posting codecs carry an inline bi-level skip trailer and hot-path
  regions moved off stable B-trees — fresh state required
- v0 keeps one active segment plus a registry marker; multi-segment levels land with merge
  scheduling work.
- Scoring is weight × tf via docid-aligned block-max parts; tf→part application is inline in the
  driver via a caller-built lookup table (lazy scoring, no eager list decode). The full fixed-point
  BM25 formula lands when the catalog owns per-term weights.
- Production target is wasm32 (root `.cargo/config.toml`; SIMD enabled), matching canbench.
- E2E cycle observations (PocketIC, single calls): ingest of 1 doc ≈ 8.3 M cycles (fixed overhead
  dominated — do not extrapolate linearly), flush-to-done ≈ 9.0 M, merge-to-done ≈ 26 M on the
  M=242 lifecycle fixture.
- (plan 0297 backfill-pull): adds durable regions 14/15 — the backfill
  registration cell + resumable cursor cell, bound by the backfill module through the shared
  `state::region()` accessor.
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

Three registered pipelines (creation-fixed per index definition; plan 0330 spike + 0331/0334
landings + plan 0332 default promotion):

- **`multilingual` (id 0, DEFAULT — plan 0332, branded koine)** — the script-dispatched
  composite: whole-text NFKC + lowercase pre-pass → UAX #29 segmentation → deterministic
  codepoint-range script classification (NO statistical language detection) → per-run layers:
  {kanji∪kana} runs through the mecab engine (id 2's analyzer, the MPD container — the same
  `DICT_REQUIRED` gate as id 2), pure-Han runs through v1 bigram (zh/ja ambiguity: bigram is
  correct for both), Hangul runs through the 조사/어미 closed-class suffix strip (받침
  allomorphy, surface+stem dual emission), Latin words through surface+Porter stem (non-English
  Latin over-stem accepted and recorded). Its {kanji∪kana} layer delegates to the id-2 mecab
  analyzer, so it inherits the plan 0339 variation-selector strip and the kana counter-variant
  fold (ヶ/ヵ → ケ) on Japanese content; the composite's own whole-text pre-pass copy remains
  (plan 0339 wired ids 1/2; pure-Han/Hangul/Latin runs carry no kana counter variants, so the
  fold is a no-op there). Deterministic and idempotent (monotone: re-analysis
  of the joined units preserves every unit). Module: `analyzer_multilingual.rs`; DDL identifier
  stays `multilingual` (the Meilisearch precedent: crate charabia, config field descriptive).
  The ABSENT `ANALYZER` clause, the provision default, and the DDL admission default all
  resolve to 0 — the breaking V1-fresh-install consequence is that the DEFAULT index path now
  requires the dictionary (the same MPD container as id 2), which the Provision canister now
  supplies automatically through the plan 0335 catalog + relay — a newly provisioned
  dictionary-carrying index reaches Ready with ZERO manual dictionary steps. Per-doc analysis
  cycles (canbench, ASCII fixture): composite 131.3K vs bigram 113.2K instructions (~16%
  Latin-layer overhead; the Japanese mecab layer rides the id-2 engine cost).
- **`unicode_bigram` (id 1, non-default)** — Unicode segmentation + per-segment shared
  pre-pass (NFKC + lowercase + variation-selector strip) + the kana counter-variant fold
  (ヶ/ヵ → ケ) applied per segment BEFORE the CJK run accumulates, so the run bigrams form
  over folded chars and non-CJK tokens emit folded; CJK character runs expand to overlapping
  bigrams (lone characters stay unigrams); ASCII words whole; NO rule layers (no Porter
  stem, no 조사 strip). Trigram indexing is a separate future index kind, not part of v1.
- **`mecab` (ANALYZER_ID=2, plan 0334)**: MeCab-format ipadic 2.7.0 Viterbi lemma units over the
  `morph-dict` byte-image engine (derived from MeCrab, MIT OR Apache-2.0) — whole-text shared
  pre-pass (NFKC + lowercase + variation-selector strip), per-line bounded common-prefix search + Viterbi, ipadic base-form
  (基本形, feature column 6) for content words with particles/auxiliaries/symbols dropped, all
  parameterized by the Japanese `DictionaryProfile`, then the kana counter-variant fold
  (ヶ/ヵ → ケ) applied to the EMITTED units only. Deterministic and strict-idempotent;
  100% unit-sequence parity with the previous vibrato engine (plan 0333 gate). The dictionary is
  NOT in the wasm (1,797,134 B after plan 0335 re-added ruzstd, +157 KB): stable region 16 carries the MPD container
  (52,931,159 bytes total — the four images sum to 52,930,923: sys.dic 49,199,027 + unk.dic
  5,684 + matrix.bin 3,463,716 + char.bin 262,496 — plus the 8-byte MPD header and the
  228-byte entry table; about $0.058/month), uploaded via relay-guarded `admin_upload_dict_chunk` in two
  transport modes (plan 0335): RAW (≤ 1 MiB/call, raw contiguous appends into region 16;
  the manual path for directly-installed canisters — unreachable from provisioning targets,
  whose relay auto-finalizes — byte-identical to the pre-0335 shape) and COMPRESSED (~1.9 MiB/call
  under the shared `MAX_DICT_COMPRESSED_CHUNK_BYTES = 1,945,600` cap, the cross-subnet 2 MiB
  inter-canister payload limit minus Candid/envelope headroom; zstd container bytes staged
  into region 17 during the provision relay, ~6 calls for the 10.9 MB zstd-19 artifact).
  `admin_finalize_dict_upload` accepts the same trailing mode: RAW verifies one xxh3_128 over
  the container (exact replay idempotent); COMPRESSED verifies the streaming compressed digest
  over region 17 FIRST (fail-closed before any mutation), streams ruzstd decompression into
  region 16 (bounded windows — no materialization), checks the declared raw length + raw
  digest, then runs the same structural validation + resident-set materialization — the end
  state is byte-identical to the raw path. The provision relay caller is a SECOND authorized
  principal on exactly these two endpoints (plan 0335 §5-2: init arg `dict_relay_caller`,
  durable region 18; all other `admin_*` endpoints keep controller-only guards;
  `admin_get_dict_status` is an unguarded read-only query). Shared wire shapes and the
  `dict_required` predicate live in `gleaph_graph_kernel::provisioning::dictionary` (single
  source of truth; the text canister re-exports, no copies). Upgrade rebind = structural
  container validation + resident-set memcpy over batched stable reads — NO decode, NO
  full-container copy: the resident set is ~21 MB (matrix.bin + char.bin + unk.dic + sys.dic
  trie/word-params), the feature-string region stays lazy over stable memory (measured hot set
  ~472 KiB of pages per MB of text). Measured rebind delta 59,900,850 cycles (plan 0335 gate 6, same-run bare-canister baseline;
  matching the 0334 measurement 59,900,562 within 288 cycles) vs the
  4,752,264,345-cycle eager-decode baseline (79x). Native throughput 0.5-0.6x vibrato
  (bounded-lookup engine); the DDL clause contract lives in
  [extension-syntax.md](../gql/extension-syntax.md).

### Shared normalization + Japanese folding (plan 0339, ported to morph-dict in plan 0340)

A single normalization module, `morph_dict::normalize` (`crates/morph-dict/src/normalize.rs`),
is the choke point for the wired pipelines (ids 1 and 2; koine's Japanese layer inherits it
through the id-2 delegation). Two pure, deterministic steps apply at different stages:

- **Pre-pass (before tokenization): NFKC → Unicode lowercase → variation-selector strip.**
  The strip removes U+FE00–FE0F, U+180B–180D, U+180F, and U+E0100–E01EF. It is safe
  pre-tokenization because no ipadic surface (entry text) contains a variation selector —
  verified at test time by byte-scanning every valid-UTF-8 field of the four dictionary
  images (sys.dic's binary table regions carry two coincidental 4-byte collisions that
  decode as nothing, so the predicate is a selector inside a valid-UTF-8 field, not a raw
  byte scan). Without the strip, 葛󠄀-style sequences (base + U+E0100) survive NFKC, ride into
  analysis, break dictionary/token identity, and drop recall for 人名/地名/official text.
- **Emitted-unit fold: ヶ/ヵ → ケ.** Applied to EMITTED UNITS ONLY, never to pre-mecab
  input. ipadic surfaces contain ヶ (茅ヶ崎, 関ヶ原), so folding pre-tokenization would
  rewrite the input away from the dictionary and damage Viterbi matching; folding the
  emitted lemma/surface units instead preserves dictionary fidelity while unifying the
  index/query space (3ヶ月 ⇄ ３ケ月 recall). The fold is idempotent and adds no error paths.

Index/query parity is structural: both sides run the same `analyze` entry points, so both
fold identically through the same functions. Known limitations (recorded, not silently
ignored): the hiragana counter 3か月 is NOT folded (unconditional か→ケ would merge the
particle か; context-dependent folding deferred), 箇/個所-style context variants are deferred,
々/〆 and 異体字 equivalence-class folding are out of scope (the latter needs a character-
variant table and belongs to the future dictionary-profile architecture), and Korean/Chinese
variant folding is out of scope (ko-dic / UniDic profiles are future work).

Selection evidence (plan 0330 spike, measured): vibrato 228 KB engine / 8.0 MB zstd dictionary (0330 baseline; superseded by the morph-dict engine 0334) /
51.8 MB heap / ~400k chars/s vs lindera 48 MB wasm (path-only dictionary API, wasm-embedded-only),
sudachi absent from crates.io + 117 MB dictionary, rule-stemmer smallest but coarse (kanji stems,
no lemmas). Recorded in `plans/0330-text-analyzer-spike.md`.

**Language coverage (measured, plan 0330/0331 cross-language fixtures):**

- `unicode_bigram` is the multilingual baseline: Han runs bigram (Chinese works — standard CJK
  strategy), Latin words whole (no stemming: `running` never matches `run`), kana bigrams.
- `mecab` (the ipadic Viterbi engine; vibrato-era measurements transfer — parity-proven,
  plan 0333) is **Japanese-only in practice**: its ipadic lexicon fragments Chinese mid-word
  (知识图谱 → 知/识图; query 数据库 misses) and Korean emits ZERO units (ipadic has no Hangul
  category; tokens are dropped). Measured in
  `crates/text-analyzer-spike/tests/cross_language.rs` — `ANALYZER mecab` must not be selected
  for Chinese or Korean content.
- Korean is the weakest language today under BOTH pipelines: `unicode_bigram` keeps whole eojeol
  tokens with particles attached (학교에서 never matches a 학교 query), the engine drops Hangul
  entirely. Fixes recorded as later slices: a dictionary-free particle-stripper (Korean 조사
  suffix tables are rule-friendly) or a `mecab-ko-dic` container through the morph-dict
  pipeline — neither implemented.

**Tier-0 rule analyzer family (superseded 0332 — landed as the dictionary-integrated composite):** the remaining
recall gaps (Korean 조사, English stemming, Japanese inflection) are all
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
(working title 0332); the quality upgrade path for Korean stays a morph-dict × mecab-ko-dic
spike (reusing the 0331 region-16 dictionary machinery if a dictionary-based analyzer is
ever adopted).

**Dictionary-integrated composite (LANDED plan 0332 as id 0 — production-precedented):** the composite
carries a DICTIONARY-BACKED layer — the charabia pattern (Meilisearch's production
tokenizer) dispatches per detected script/language to specialized segmenters including
dictionary-backed ones (Japanese = lindera + IPA-dict, Korean = lindera KO-dict, Chinese =
jieba; see the charabia README language table). The Gleaph shape: script-run chunking over
the NFKC pre-pass, then {kanji∪kana} chunks → the mecab engine (ipadic lemma, the 0334 dictionary
machinery), Hangul chunks → 조사 strip layer, Latin chunks → Porter, pure-Han chunks → bigram
(fundamental zh/ja ambiguity: bigram is safe for both; dual-emission of mecab+bigram units
is the quality option at ~2× Han postings). Consistency holds because index-time and
query-time run the same composite; per-chunk dispatch loses sentence context across script
boundaries (accepted, same as charabia). Implementation deltas vs the pure-rule composite:
generalize the region-16 dictionary machinery from id-2-specific to the shared `DICT_REQUIRED`
gate ({0, 2}) and include the morph-dict engine (getrandom-free — verified). The
id-2 pure-Japanese analyzer coexists: the composite is the multilingual single-index answer;
per-language indexes remain the precision-maximal shape (the ES multi-fields pattern is the
N-index equivalent).

**Performance roadmap (morph-dict engine — opportunistic, recorded 2026-09-05):** measured
native throughput is 0.47–0.62× the vibrato engine (plan 0334 gate ≥0.5×); per-doc analyze
is ~40K instructions of the ~8.3 M ingest/doc baseline, so closing the gap moves ingest cost
by only a few percent — implement only on a trigger below. Levers, with vibrato-author
evidence (Kanda, "MeCab互換な形態素解析器Vibratoの高速化技法", LegalOn Technologies
Engineering Blog 2022-09-20; all numbers native, mecab-ipadic 2.7.0):

- **Codepoint-unit trie** (Crawdad-style: patterns as codepoint sequences, not UTF-8 bytes —
  3 transitions/char → 1): −12.3% analysis time on ipadic. COST: sys.dic's trie section
  becomes a morph-dict-owned layout (MeCab-format compat is dropped) + a builder is needed;
  `sys_dic::from_parts` already owns the parser. Second IC payoff: the DMT page-charge model
  (5,000 instr/4 KiB page, uniform heap/stable) rewards any hot-structure shrink.
- **Persistent scratch arena** (lattice nodes/edge/slot vectors, single-threaded pinned
  analyzer): kills per-sentence allocation churn (the recorded 0334 residual). Cheapest
  lever; estimate 5–15% pending profile.
- **Feature-read short-circuit** (bounded POS-prefix read for the drop decision before the
  full feature string): the feature region is only ~5% of bytes read — minor.
- **Frequency-ordered context-ID mapping** (reorder matrix IDs by training-corpus usage
  frequency): −3.4% on ipadic (3.3 MiB matrix fits in L3) but −35.6% on unidic-cwj's
  459 MiB matrix — implement ONLY with a UniDic-class dictionary (pairs with the
  vibrato-rkyv/compact-matrix reconsideration trigger).
- **IC-specific**: DMT page charges make compact hot structures (bitset connector,
  vibrato 0.5.x dual-connector pattern; codepoint trie) reduce BOTH instructions and
  touched pages; wasm SIMD is available on IC but vibrato's simd is nightly AVX2 — a wasm
  port is a long-shot research item.
- Triggers: mass backfill where analyze dominates ingest cost; UniDic-class dictionary
  adoption; measured query-latency complaints (queries are short — unlikely).

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
numbering (plan 0331) lives in `crates/text-canister/src/state.rs` next to the manager:
meta cell · segment registry map · dictionary probes (linear hash map `u128→u32`) · dense term
entries (canonical string arena ref + df) · postings blob refs · block-max blob refs · dense
doc-key slots · doc-key→docid linear hash map (`u64→u32`) · tombstone container vector · stats
cell · pending-ops FIFO deque (payloads in the shared blob arena) · merge-cursor cell ·
controller cell · shared fixed-chunk blob arena · term-entry vector — plus the plan 0297 additions:
backfill registration cell + resumable cursor cell (MemoryIds 14/15, bound by the backfill
module through `state::region()`) — plus the 0331 addition: the analyzer-2 ZSTD dictionary
blob (MemoryId 16, bounded 1 MiB chunk slots; upload/finalize contract in the analyzer section)
— plus the plan 0335 additions: the compressed MPD staging region (MemoryId 17, plain zstd
container bytes during the provision relay, streamed-decompressed into 16 at compressed
finalize then logically deactivated) and the provision relay caller cell (MemoryId 18,
`Cell<Principal>`; the second authorized principal on the two relay endpoints).

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
