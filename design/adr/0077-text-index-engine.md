# 0077. Text Index engine: custom segment-LSM with varint postings and a RISE-derived block-max driver

Date: 2026-08-24
Status: accepted
Last revised: 2026-09-04

## Context

ADR 0054 leaves the Text Index as an optional derived resource with its implementation and
partition strategy explicitly open ("including whether FST is appropriate"). GQL carries a
`FULLTEXT INDEX` syntax sketch ([extension-syntax.md](../design/gql/extension-syntax.md)), and no
engine exists behind it. The Internet Computer imposes hard shapes on any candidate: 500 GiB
stable memory, 2 GiB stable read/write per replicated message (8 GiB on upgrade), 40 B / 5 B /
300 B instruction budgets for update / query / upgrade, automatic deterministic time slicing,
2 MiB cross-subnet payloads, composite queries restricted to one subnet, and storage at
~$0.45/GiB/month with free non-replicated queries
([investigation §1](../design/investigations/2026-08-23-text-index-design-research.md), verified
2026-08-23/24 UTC).

A full PoC benchmark round (plan 0293) produced measured evidence on this exact platform shape:

- `crates/ic-stable-text-postings` — physical-layer kernels: varint/FoR/Elias-Fano/PEF posting
  codecs, block-max tables, resumable k-way merge, DAAT top-k driver, deterministic corpus
  fixtures; canbench-persisted baselines.
- `crates/text-index-fts5-arm` — SQLite FTS5 via `ic-sqlite-vfs` on the same fixture semantics.
- A RISE extraction spike validating that RISE's WAND / Block-Max WAND / BM-MaxScore layer
  extracts to 1,072 LOC of std-only, zero-nightly, wasm32-compiling code.

Measured D1 table @M=2000 docs (both arms far inside ICP budgets):

| Workload | FTS5-on-VFS | Custom (final PoC state) | Winner |
|---|---|---|---|
| Ingest | 244.27 M instr (~122 k/doc) | 188.80 M instr (`build_segment`, ~94 k/doc)¹ | custom ~23% |
| m3 OR rank top-10 | 34.84 M instr | **15.78 M** (tf-scored: 17.25 M) | custom ~2.2× |
| Storage | 376,832 B logical image | 141,355 B set / 193,109 B freq (incl. dict+block-max) | custom 2.0–2.7× |

¹ FTS5 ingest includes internal tokenization; the custom figure starts from unit-id streams —
disclosed asymmetry.

## Problem

`FULLTEXT INDEX` needs an engine that (a) fits the instruction/memory budgets above including
timer-driven maintenance, (b) integrates with Gleaph's derived-state contracts (pending flush,
backfill cursors, lag vocabulary, split thresholds), (c) gives Gleaph long-term ownership of the
analyzer (Japanese bigram default), scoring representation, and determinism contract, and (d) is
economical at scale. No existing component provides this, and no production-grade FTS canister
exists to adopt ([investigation §2](../design/investigations/2026-08-23-text-index-design-research.md)).

## Existing architecture assessment

- **graph-index cannot absorb full text.** Its postings are equality/range keys over sortable
  values; tokenization, ranking statistics, segment lifecycle, and merge machinery are foreign
  concepts there. Its planned value-bucket + ordinal indirection is *reused conceptually*
  (term_id scheme) but extending graph-index into a text engine would mix concerns the capacity
  model deliberately separates.
- **The Property/Vector Index pattern is the right host shape**: optional derived state in its
  own canister, attach/backfill/detach contracts, Router-owned definitions. ADR 0054 already
  reserves "Text index canister(s)" in the topology; this ADR fills the implementation slot
  rather than inventing a new resource kind.
- **gleaph-gql / gleaph-gql-planner purity** forbids engine logic in the generic GQL crates;
  analyzer and ranking policy belong to index-side crates.
- What current architecture lacks: a persisted inverted-index substrate, a merge protocol under
  ICP message budgets, and a scored top-k executor. The PoC crate demonstrates all three fit the
  budgets with large margins.

## Alternatives

1. **Minimum-change: SQLite + FTS5 via `ic-sqlite-vfs`.** Measured competitive pre-port
   (34.84 M vs our then-55.18 M ranked query) with mature BM25/tokenizers. Rejected: after the
   encoding fix (varint) and driver port, it ranks ~2.2× behind on the flagship workload;
   defaults need surgery (~32 MiB page cache); WAL unsupported; unicode61 treats CJK runs as one
   token so Japanese still requires pre-bigramming or a custom tokenizer; scoring/analyzer
   ownership moves into SQLite configuration; single-maintainer dependency. Remains valuable as a
   future comparison arm — both harnesses are retained.
2. **Moderate (chosen): custom segment-LSM physical layer** — immutable segments, level merges as
   cursor-resumable timer steps, tombstones, varint postings, block-max pruning driver. All
   components individually benched inside budget; ownership stays in-house.
3. **Large: adopt RISE wholesale** (codecs + query stack). Rejected as a dependency today:
   nightly-only features (incl. `float_algebraic`, `core_intrinsics`), rayon/memmap2/epserde
   dependency tree, no crates.io release, research-grade API churn. Validated instead as an
   extraction/reference source (1,072 LOC std-only subset compiles for wasm32; MIT). Codec
   consolidation into RISE remains a future option at L effort (~6.3 k LOC strip).
4. **PISA / DS2I via C/C++ FFI.** Rejected: SIMD-intrinsics cores assume SSE/AVX absent from
   wasm32, forcing scalar rewrites anyway; adds a C toolchain + precompiled-artifact pipeline for
   a hot loop small enough to own in Rust.
5. **Trie engines / off-chain services.** Trie lacks ranking/compression at target scale;
   off-chain contradicts on-chain derived-state ownership.

## Decision

Adopt the **custom segment-LSM inverted index** for the Text Index, as prototyped:

1. **Physical layer**: immutable segments in stable memory; per-segment term dictionary over a
   dense `u32 term_id` indirection (v0: StableBTreeMap-backed; SplitFstReader deferred until a
   fuzzy/regex requirement lands); postings as delta-varint docid sets plus an interleaved
   (delta-docid varint + u8 tf) variant where scoring requires frequencies; uncompressed
   block-max score tables aligned to 128-doc logical blocks; tombstone bitsets filtered at query
   time and reclaimed by merges.
2. **Maintenance**: DML deltas land in a durable pending queue; timer-driven flushes close
   level-0 segments; level merges run as cursor-resumable deterministic update steps sized within
   the 40 B-instruction / 2 GiB-write message budgets (measured headroom: ~2.2 k instructions per
   output id).
3. **Query execution**: DAAT disjunctive top-k with whole-total block-max skipping — the pruning
   structure reference-ported from RISE (MIT attribution) into an allocation-free integer-score
   driver. Measured 15.78 M instructions for m3/top-10 (~0.3% of the 5 B query budget).
4. **Scoring contract**: fixed-point integer scores owned by the index definition catalog
   (fixture parts precomputed outside the physical crate; formula finalized in the engine
   implementation slice).
5. **Analyzer default**: Unicode segmentation + NFKC/lowercase + CJK character bigram;
   morphological analyzers register as later pipeline ids with an explicit DDL clause;
   trigram indexing as a separate future kind. Analyzer identity is part of the index
   definition and never enters gleaph-gql/planner.
   - Execution note (2026-09-04, plan 0331): the plan 0330 spike (measured on wasm32) selected
     **vibrato 0.5.2 + ipadic-mecab (superseded 0334 by the morph-dict MeCab-format engine, 100% unit-sequence parity)** over lindera 6.0 (system-dictionary API is path-only — no
     bytes-based load, so no stable-resident landing shape; engine+dict wasm 48 MB) and over
     sudachi (crate absent from crates.io; 117 MB small dictionary; wasm build fails on its
     dynamic plugin loader) and vaporetto (pointwise prediction is not segmentation-consistent;
     no lemma tag without license-restricted BCCWJ training data). Landed as ANALYZER_ID=2 with
     the 8.0 MB ZSTD dictionary in stable region 16 (not the wasm: canister 1.85 MB, engine-only
     vibrato 228 KB per the corrected 0330 audit) and the `ANALYZER <name>` DDL clause
     (`unicode_bigram` | `mecab`).
   - Execution note (2026-09-05, plan 0334): analyzer-2 internals were swapped to the
     `morph-dict` byte-image Viterbi engine (derived from MeCrab, MIT OR Apache-2.0) with 100%
     unit-sequence parity vs the vibrato pipeline; region 16 now carries the MORPHDICT1
     container (52,930,923 B) and the DDL name was honestly renamed `vibrato` → `mecab` with
     NO compatibility identifiers (no aliases, no migration, layout stays 1). Rebind collapsed
     from 4,752,264,345 eager-decode cycles to a 59,900,562-cycle validate+memcpy delta.
   - Execution note (2026-09-05, plan 0332): the script-dispatched multilingual composite
     became ANALYZER_ID=0, the DEFAULT (absent clause, provision default, and admission
     default all resolve to 0; ids 1/2 kept non-default and byte-unchanged). The composite
     (branded koine; DDL identifier stays `multilingual`) follows the charabia precedent —
     a script-dispatched pipeline carrying a dictionary-backed layer is the production-
     proven multilingual pattern (Meilisearch/charabia dispatches to lindera-IPA, lindera-KO,
     jieba per script; the ES multi-fields N-index pattern is the alternative and stays the
     per-language precision-maximal shape). Script dispatch is a pure codepoint-range
     function (determinism contract, no statistical language detection); the `DICT_REQUIRED`
     gate widened to {0, 2}, so the DEFAULT index path carries the dictionary upload flow
     (recorded friction; Router-side dictionary relay is the Later-Slice mitigation).
6. **Partition strategy remains open** (ADR 0054), with docid-range sharding and same-subnet
   scatter-gather as the working assumption.

## Consequences

Positive: every measured workload lands 1.6×–14× under the relevant ICP budget; ingest, storage
density, and ranked-query cost all beat the only mature alternative; analyzer, scoring, and
determinism contracts stay Gleaph-owned (JP bigram without SQLite tokenizer work); no new
external runtime dependency; the PoC crates graduate into the production layer with persisted
baselines guarding regressions.

Trade-offs accepted: Gleaph owns merge/tombstone/crash-resume engineering SQLite would provide
for free; fuzzy search (SplitFstReader) and phrase search are deferred until their requirements
land; the FTS5 comparison arm must be re-run whenever the baseline claim needs refreshing.

## Migration

Nothing deployed depends on this decision yet. When the engine wires into a canister: add the
TextIndex variant to `LogicalResource` / artifact `CanisterKind` (ADR 0054 implementation gaps),
register the region map in the ADR 0007-style inventory, extend ADR 0059 backfill kinds with
Text, and require fresh state for any layout-version change per repository pre-production rules.
No data migration paths are built.

## Design documentation impact

- [investigation](../design/investigations/2026-08-23-text-index-design-research.md): §9 statuses
  updated alongside this ADR (this patch).
- Before canister wiring: new `design/index/text-index.md` contract (segments, merges, lag
  semantics), capacity-planning.md TEXT region rows, extension-syntax.md FULLTEXT semantics,
  derived-state-query-semantics.md text rows.

## Open questions

- Merge duty cycles on shared subnets (fairness measurement).
- Flat-blob dictionary with read caching vs StableBTreeMap at production vocabularies.
- Whether PostingBlockStore generalizes toward graph-index's ordinal-tail optimization
  (shared-crate question, architecture-integrity review at wiring time).
