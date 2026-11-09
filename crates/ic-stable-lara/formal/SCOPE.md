# Formal Verification Scope — ic-stable-lara (Labeled LARA)

Anchor timestamp: 2026-11-09 19:05:00 UTC +0900
Target revision: git `d75d246789f628d19e8821a41a9881d151c7a6fa` (2026-11-09
re-anchor; the surviving transcriptions cover `src/slab_index.rs` and
`src/labeled/bucket_label_key.rs`. Plan 0375 re-anchored `src/slab_index.rs`
from `dbd76f3d`: the retired log-head vocabulary became spill-run vocabulary
and every citation was refreshed; `src/labeled/bucket_label_key.rs` was
unchanged since the old anchor.)

## Mode

**Audit mode against an existing implementation, run as a permanent fixture**
(interview decision, 2026-08-26). `formal/` is a standing Lean project that is
re-checked with `lake build` whenever the transcribed sources change. Findings
are recorded in `REPORT.md`, refreshed alongside the proofs.

Value target (interview decision): **scan correctness** — a clean scan must
decode exactly the adjacency that was encoded — and **storage safety** — all
wire fields stay inside their bounded spaces, checked arithmetic never silently
wraps, and malformed/reserved wire images are rejected fail-closed.

## Location and form

- Lean lake project: `crates/ic-stable-lara/formal/` (house convention shared
  with `lhm_formal` / `svd_formal`; the lean-formal-audit skill's generic
  `audit/` root is intentionally not used so all formal fixtures live beside
  their crates).
- Independent from the Cargo workspace (no `Cargo.toml`; the workspace lists
  members explicitly, so this directory is invisible to Cargo).
- Toolchain: Lean 4 core only (`leanprover/lean4:v4.33.1`). No Mathlib, no Std.
- Package `«lara_formal»`, default target lean_lib `«Lar»`.
- Run locally: `cd crates/ic-stable-lara/formal && lake build`.
- Freshness gate: [`SOURCES.sha256`](./SOURCES.sha256) pins the transcribed Rust paths
  (`src/slab_index.rs`, `src/labeled/bucket_label_key.rs`) at anchor `d75d2467…`; the checker
  `scripts/check-lean-sources.py` runs in CI (`.github/workflows/lean-freshness.yml`). Status is
  `anchored` since plan 0375: drift in either path is **blocking**. Re-anchor only by re-reading
  the source diff, updating the model and refreshing the manifest hashes together.
- CI: `lean-verify.yml` builds this fixture and applies the axiom gate (no `sorryAx`,
  no `Classical.choice`), alongside lhm and vec-deque, since plan 0381.

## Method

Hand-written Lean model plus proofs, adapted from the methodology of
`crates/ic-stable-linear-hash-map/formal/` (`Lhm_formal`) via
`crates/ic-stable-vec-deque/formal/` (`Svd`) without importing their code.
Rust semantics are transcribed faithfully into Lean definitions; each
definition cites the source file and line range it mirrors. No interpretation
or "fixing" while transcribing; doubts go into `-- NOTE:` / `-- SUSPICION:`
comments and REPORT.md findings. Every headline theorem is guarded by
`#print axioms`; the build must show no `sorryAx`.

## Why staged

The labeled module is ~48k LOC across multi-store coupled state (vertex rows ×
label-bucket slots × edge slab/PMA counts/span-meta/overflow logs × free-span
store × bidirectional counterparts) with range-permutation update operations.
A whole-module transcription is not one deliverable. The plan below stages the
audit so each stage is sorry-free when delivered; later stages are recorded now
so the roadmap survives agent handoffs.

## Component breakdown (audit layers)

### Stage 1 — record/slot arithmetic (in scope now)

Transcription surface, smallest self-contained layer everything else sits on:

1. **Arithmetic foundations** (`Lar/Basic.lean`) — bit-field packing over
   `Nat`: `pack2`/`pack3`, `midBits`, `replaceLow`, `lt_two_pow`. Shared by
   every surviving Stage-1 model, not a Rust transcription itself.
2. **36-bit slot index space + u40 byte offsets** — `Lar/SlotIndex.lean`,
   transcription of `slab_index.rs` (L6-L202): slot-index/byte-offset
   constants and fit predicates, checked addition
   (`checked_add_slot_index`/`checked_add_byte_offset`); locator words
   `try_encode_locator_word`/`decode_slot_index`/`decode_meta28`/
   `try_replace_slot_index`; vertex tail28 pack/unpack
   (`pack_vertex_tail28`/`unpack_vertex_tail28`/`try_pack_vertex_tail28`).
3. **BucketLabelKey** — `Lar/BucketLabelKey.lean`, transcription of
   `labeled/bucket_label_key.rs` (L11-L115): raw `u16` identity, directedness
   MSB (`BUCKET_LABEL_DIRECTED_BIT`), low-15-bit index masks, index-based
   constructors round-trip, `Ord` = raw order with undirected-before-directed
   grouping.

Properties to prove (headline set, for the surviving modules):

- P1-roundtrip: decode ∘ encode = id on each valid domain — locator word,
  tail28, BucketLabelKey index constructors.
- P2-injection: encodings are injective on valid domains (distinct fields ⇒
  distinct words; no aliasing between packed regions).
- P3-noninterference: `try_replace_slot_index` rewrites exactly its own field
  bits and preserves every other field's decode.
- P4-extent: slot fields ≤ `SLOT_INDEX_MASK`; successful
  `checked_add_slot_index s d` ⟺ `s + d ≤ SLOT_INDEX_MASK` (no wrap possible
  under that bound); byte offsets likewise against the u40 mask.
- P5-sentinel-faithfulness: tail28 `enc = 0 ↔ log_head = −1`, `enc = k+1 ↔ k`
  otherwise, tombstone bit orthogonal to log encoding.

### Stage 2 — scan-contract geometry over abstract stores (planned follow-up)

Abstract state per README core invariant and `invariants.rs` asserts:
`[base_slot_start, base_slot_start + degree) ⊆ [base_slot_start,
base_slot_start + stored capacity)` per row; strictly sorted `BucketLabelKey`
runs per vertex (invariants.rs L150-L169); bucket spans within bucket/edge
capacity (L138-L149, L197-L220). Mirrors design/storage/lara.md contract 1
(scan contract).

### Stage 3 — PMA accounting preservation (planned follow-up)

Per-leaf actual/total conservation for insert/remove paths against the
per-vertex contribution model of `expected_vertex_pma_contribution`
(invariants.rs L249-L338).

### Deferred class (recorded, NOT attempted)

Range-permutation proofs: `rebalance_weighted_with_layout`, segment
relocate/slide, compact/batch_write coalescing; free-span reuse disjointness;
failure atomicity of grow/promote preflight-commit splits. Same class as
`svd_formal` Stage 4/5, strictly larger.

## Assumptions (managed centrally here)

- A-L1 (release semantics): transcriptions mirror release-build semantics;
  `debug_assert!`s (e.g. slab_index.rs L38, L172, L179) are recorded but not
  obligations.
- A-L2 (infallible-constructor preconditions): `.expect(...)` panics in the
  transcribed functions become hypotheses "inputs are valid" on callers,
  matching how svd handled Rust asserts.
- A-L3 (byte-layer opacity): fixed-width LE conversions (`from/to_le_bytes`)
  map to trusted Nat conversions; wire-record framing is not modeled by the
  surviving modules.
- Element payloads (`CsrEdge` bytes), stable-memory page mechanics, and all
  Stage 2+ surfaces are out of scope for Stage 1.

## Status

- Stage 0: scope agreed with user 2026-08-26 (audit mode; Stage 1 first slice;
  scan correctness + storage safety as value targets).
- Stage 1: **delivered 2026-08-26, sorry-free, `lake build` green.** After the
  2026-11-09 retirement (plan 0372) the surviving modules are
  `Lar/Basic.lean`, `Lar/SlotIndex.lean`, `Lar/BucketLabelKey.lean`; headline
  guards live in `Lar.lean`. Findings in REPORT.md retain the original
  Stage-1 text as a historical record (see the retirement section below).
  Remaining within Stage 1 scope for the next increment: re-anchoring the
  surviving transcription to current HEAD and the u40 read/write pair.
- Stages 2–3 and deferred class: planned follow-up, not attempted.

## Stage 1 retirement (plan 0372, 2026-11-09)

`Lar/LogHead.lean`, `Lar/BucketWord.lean`, `Lar/LabelBucket.lean` and the
`formal/verus/` mirror were deleted because their subjects — the overflow-log
head codec, the word-bit head packing, and the old 29-byte row validation — no
longer exist in `crates/ic-stable-lara/src`: the row is now 34 bytes with
tiny/tree modes and per-bucket spill runs (ADR 0097). The surviving modules
(`Lar/Basic.lean`, `Lar/SlotIndex.lean`, `Lar/BucketLabelKey.lean`) still
transcribe shipped code; their freshness and re-anchoring are a later slice
per `design/investigations/2026-11-09-lean-fixture-migration-classification.md`.
No re-anchoring or re-proof happened in this slice.
