# RISE extraction spike — report (slice 7)

Scratch-only. Upstream clone: `rise-rs/` (AngeloSav/rise-rs @ 3b178df, MIT).
Extraction crate: `rise-queries-min/`. Inventory artifact: `inventory.md` (same dir).

## Task B — minimal extraction compile: SUCCESS

Crate: std-only, zero dependencies, zero nightly features. Contents = copies of upstream
files with the edits listed under "Deviations from upstream" below, plus
`posting_iter.rs` (the two traits lifted out of `indexes/mod.rs`) and `stub_index.rs`
(test fixture + brute-force oracle, not part of an extracted library).

Exact commands and results (run in `rise-queries-min/`):

| Command | Result |
|---|---|
| `cargo build --release` | OK (exit 0; 4 warnings, all upstream dead code) |
| `cargo build --release --target wasm32-unknown-unknown` | OK (exit 0), emits `target/wasm32-unknown-unknown/release/rise_queries_min.wasm` (324 B — generic code, no exports) |
| `cargo test --release` | 7/7 pass (native nightly toolchain) |
| `cargo +stable build --release --target wasm32-unknown-unknown` | OK on **stable 1.95.0** → proves zero-nightly |
| `cargo +stable test --release` | 7/7 pass |

Correctness gate (`stub_index.rs`): WAND / BMWand / BMMaxScore each reproduce a
brute-force top-10 exactly on a 6-doc × 3-term corpus with DotScorer, including a
crafted exact score tie that pins (score desc, docid asc). BM25 variant runs with exact
BM25 block weights and agrees on the top hit.

### Extraction boundary (compiles for wasm32)

| File | Upstream origin | LOC |
|---|---|---|
| src/lib.rs | new (module decls) | 22 |
| src/posting_iter.rs | `indexes/mod.rs` traits only | 29 |
| src/queries/mod.rs | trimmed (traits + re-exports) | 34 |
| src/queries/topk_heap.rs | verbatim + tie-break + det. tests | 201 |
| src/queries/block_posting_metadata.rs | in-memory core (no epserde/fs/log) | 97 |
| src/queries/scorers/{mod,bm25,dot_scorer}.rs | minus epserde/TypeHash | 67 |
| src/queries/query_algorithms/{mod,wand,bm_wand,bm_maxscore}.rs | import-path edits only | 622 |
| **Total** (excludes stub_index.rs tests, 242 LOC) | | **1072** |

Not needed by the three operators (verified unused): block_partitioning.rs,
score_part.rs, config.rs, and/or/maxscore/ranked_and/ranked_or. score_part +
block_partitioning are themselves std-compilable once the vestigial `num::Float`
import is dropped (their bodies use inherent `f32` methods).

### Deviations from upstream (all recorded in-code)

1. **TopKHeap tie-break (requested contract change):** `Ord for PostingInfo` now ranks
   equal scores by docid descending inside the inner ordering so the min-heap evicts and
   `into_sorted_vec()` emits (score desc, docid asc) via `f32::total_cmp`. Verified by a
   dedicated test including eviction-among-equals.
2. epserde removed: `DocScorer: TypeHash` supertrait dropped; `#[derive(Epserde)]`
   removed; `load_file`/`create_file` persistence deleted; `peek_scorer_kind` /
   `peek_idx_kind` type-hash helpers dropped. `BlockPostingMetadata::from_parts` added as
   the construction path.
3. mem_dbg supertrait dropped from `InvertedIndex` (unused by operator bodies).
4. clap enums (`ScorerKind`, `QueryKind`, `IdxKind`) dropped with the binaries.
5. float_algebraic: **nothing to replace** — upstream's algebraic ops exist only in a
   commented-out BM25 block; active code already used plain ops (no semantic caveat).
6. `core::intrinsics::likely`: present only in excluded files (and/or/ranked_and);
   WAND/BMW/BM-MaxScore contain none. A stable `fn likely(b: bool) -> bool { b }` would
   be the drop-in if those operators are ever included.
7. log/indicatif progress lines deleted with create_file.

## Task C — API fit against our PostingReader

Upstream iteration surface (used by all three operators, cursor-only):

```rust
trait PostingListIter {
    fn current_doc(&self) -> u64;          // frontier docid; == n_docs past end
    fn current_pos(&self) -> usize;
    fn next_geq(&mut self, lb: u64);       // mutate only, no result value
    fn next_doc(&mut self);
    fn freq(&mut self) -> u64;             // per-posting term frequency
    fn len(&self) -> usize;
}
trait InvertedIndex {
    type IterType<'a>: PostingListIter where Self: 'a;
    fn n_docs(&self) -> usize;
    fn n_terms(&self) -> usize;
    fn get_plist_iter(&self, i: usize) -> Self::IterType<'_>;
}
```

Ours (`ic-stable-text-postings::PostingReader`): `len / is_empty / pos / peek / next /
advance(target) -> Option<u32>` over borrowed encoded buffers (zero-copy).

Method mapping:

| RISE | ours | Fit |
|---|---|---|
| `current_doc()` | `peek()` (None ⇒ map to n_docs sentinel) | direct adapter |
| `next_geq(t)` | `advance(t)` (ignore returned Option) | direct adapter |
| `next_doc()` | `next()` | direct adapter |
| `current_pos()` | `pos()` | direct |
| `len()` | `len()` | direct |
| `freq()` | **missing** — our readers carry no tf | gap |

Key findings:

1. **The operators are genuinely cursor-only.** They never index into sequences; every
   access goes through the six trait methods. No uncompressed random-access assumption
   inside wand/bm_wand/bm_maxscore (bm_maxscore's 4096-doc bitset window assumes only
   ascending docids). So our varint/FOR/EF/PEF readers could drive them through a ~50-line
   adapter without copying buffers.
2. **`freq()` is the real gap.** Our reader path is docid-set only; tf exists today only
   as the slice-6 parity byte format (interleaved delta-varint + u8 tf). Real scoring
   needs either that interleaved kernel implemented for readers, or keeping our
   caller-supplied constant-weight model (which is what our own topk.rs does).
3. Exhaustion sentinel differs (None vs `n_docs`): one match arm in the adapter.
4. Ownership: both sides are borrow-based cursors; no copying required anywhere.

## Task D — verdict inputs

(a) **Boundary that compiles for wasm32:** the file set above, 1072 LOC (+242 test),
zero deps, stable-Rust clean, `.wasm` artifact produced.

(b) **Nightly features after cleanup:** ZERO. Of upstream's six declared features:
`core_intrinsics` (3 query files outside the minimal set + elias_fano/positive_sequences),
`array_windows` (1 elias_fano site), `float_algebraic` (commented-out only) are real but
avoidable; `impl_trait_in_assoc_type`, `iter_array_chunks`,
`binary_heap_into_iter_sorted` are declared-but-unused vestiges even upstream.

(c) **Determinism fixes needed:** ① TopKHeap tie-break — DONE in extraction (our
contract). ② `query_freqs()` builds its term vector from `HashMap::into_iter` → term
processing order varies per process; combined with stable sort-by-current-doc this makes
f32 accumulation order (last-ulp) unspecified upstream — must switch to an ordered map or
count-sort before any internal adoption. ③ Optional hardening: `partial_cmp().unwrap()`
sorts (NaN panic) and `get_unchecked` reads are upstream perf choices to review.

(d) **API-fit effort:**
- *Extract queries layer only* onto our kernels: **S** for plumbing (traits + adapter +
  sentinel mapping), rising to **M** overall because scoring requires the interleaved
  (docid, u8-tf) reader kernel we have only defined at byte level (slice-6 parity format).
- *Also adopt EF/PEF codecs:* **L** — elias_fano+bitvector+positive_sequences ≈ 6.3 k LOC
  to strip of epserde/mem_dbg derives, replace persistence with our layout headers, port
  their builders, plus determinism/hygiene fixes; entanglement is moderate (derives +
  TypeHash everywhere) but no external bitstream dependency inside those modules.

(e) **Recommendation:** **No-go on vendoring the query layer wholesale into an internal
`ic-stable-text-query` crate; go on reference-porting the BMW/BM-MaxScore pruning
structure into our own topk.rs.** Rationale: the three operators total only ~620 LOC and
are pure cursor logic — the part we'd keep is small relative to the contract surface it
drags in (freq()-capable readers, HashMap-order determinism fix, sentinel conventions,
upstream's f32-threshold semantics vs our integer-score model). Our existing topk.rs
already implements the sound whole-total WAND form; porting bm_maxscore's essential/
non-essential partitioning + cumulative-block-max pruning (~150 LOC of ideas) captures
the algorithmic value while keeping our encoding choice (varint won Q1) and instruction
budget discipline. Keep this scratch crate as the reference oracle: its brute-force
tie-break tests transfer directly to any ported implementation. Revisit vendoring only if
we later adopt their EF/PEF codec stack too (then the queries layer rides along).
