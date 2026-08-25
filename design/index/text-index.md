# Text index

Last updated: 2026-08-24 (plan 0295 structures-swap + query-path-and-bench)
Status: **Partially Implemented** — engine accepted ([ADR 0077](../adr/0077-text-index-engine.md));
v0 canister (`crates/text-canister`) wired and lifecycle-verified on PocketIC (plan 0294,
2026-08-24); hot-path stable structures swapped to density-matched stores and the whole-path
term-search bench landed (plan 0295, 2026-08-24). Not yet implemented: Router admission/fan-out
wiring, backfill-kind extension (designed in [ADR 0059 §Text build kind](../adr/0059-create-index-migration-backfill.md), 2026-08-26 — not yet implemented),
fuzzy/phrase/trigram features (see Non-goals). Physical kernels
live in `ic-stable-text-postings`.

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

## Purpose

Define the contract for the Text Index canister: an optional derived search service over labeled
vertex properties (edges later), reachable through GQL `FULLTEXT INDEX`
([extension-syntax.md](../gql/extension-syntax.md)). Engine decision, alternatives, and measured
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

## Analyzer default

Unicode segmentation + NFKC + lowercase; CJK character runs expand to overlapping bigrams
(lone characters stay unigrams); ASCII words whole. Morphological analysis (lindera) is opt-in
per index definition behind a feature flag (~13 MB wasm for IPADIC). Trigram indexing is a
separate future index kind, not part of v1.

## Lifecycle and lag semantics (mapping onto derived-state contracts)

| Phase | Behavior | Lag class |
|---|---|---|
| DML on indexed text property | Unit deltas appended to a durable pending log (`StableBTreeMap<u64 seq, Op>` — bounded drain; `StableLog` lacks truncate); canonical write succeeds independently | none yet |
| Timer flush | Pending deltas become one immutable level-0 segment | Under-posted until flush completes |
| Backfill (`CREATE TEXT INDEX` on existing data) | Migration-driven replay per [ADR 0059 §Text build kind](../adr/0059-create-index-migration-backfill.md) (designed 2026-08-26, not yet implemented); cursor-resumable | Under-posted until done |
| Merge | Level merges run as resumable steps; mid-merge reads see old+new with tombstone arbitration (newer wins) | Over-posted transiently possible; no silent drops |
| Delete/update | Tombstone bitset entry; reclaim deferred to merge | Over-posted until merge |

Reads are query calls (free, 5 B instructions, 1 GiB stable reads) and therefore eventually
consistent, matching the Property-postings rows in
[derived-state-query-semantics.md](derived-state-query-semantics.md); the `ReadMode` barrier
applies once Router wiring lands.

## Region map (MemoryId plan, ratified at wiring time)

One `MemoryManager` in the text canister; ≤255 ids; one structure per id. Concrete 14-region
numbering (layout v3, plan 0295) lives in `crates/text-canister/src/state.rs` next to the manager:
meta cell · segment registry map · dictionary probes (linear hash map `u128→u32`) · dense term
entries (canonical string arena ref + df) · postings blob refs · block-max blob refs · dense
doc-key slots · doc-key→docid linear hash map (`u64→u32`) · tombstone container vector · stats
cell · pending-ops FIFO deque (payloads in the shared blob arena) · merge-cursor cell ·
controller cell · shared fixed-chunk blob arena · term-entry vector.

## Budgets and capacity

Measured @M=2000 docs (seed 20260823 fixture family): build 188.80 M instructions (~94 k/doc),
m3/top-10 query 15.78 M (tf-scored 17.25 M), storage 141–193 KB logical bytes. Formula model,
worked examples, and the soft/hard split thresholds (350/400/450 GiB) carry over from
[capacity-planning.md](capacity-planning.md); TEXT-specific rows are added there as wiring lands.

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
