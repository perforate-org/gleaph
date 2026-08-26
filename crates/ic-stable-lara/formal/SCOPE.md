# Formal Verification Scope — ic-stable-lara (Labeled LARA)

Anchor timestamp: 2026-08-26 07:26:04 UTC +0000
Target revision: git `dbd76f3d3441e804336d901798cf21b9d86ac563`
(`src/slab_index.rs`, `src/log_head.rs`, `src/labeled/*` are clean in the
working tree at this revision)

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
- No CI wiring yet (repository has no `.github/workflows/`).

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

1. **LogHead codec** — `log_head.rs`: valid domain `{−1} ∪ [0, 170)`
   (`DEFAULT_MAX_LOG_ENTRIES = 170`), `NONE = 0xFF` sentinel,
   `from_i32`/`to_i32`/`as_byte`/`from_byte` (log_head.rs L10-L70).
2. **36-bit slot index space + u40 byte offsets** — `slab_index.rs`:
   `SLOT_INDEX_BITS/MASK`, `MAX_SLOT_EXCLUSIVE_END`, `slot_index_fits`,
   `slot_exclusive_end_fits`, `checked_add_slot_index`,
   `checked_add_slot_exclusive_end` (slab_index.rs L6-L133);
   locator word `try_encode_locator_word`/`decode_slot_index`/`decode_meta28`/
   `try_replace_slot_index` (L68-L108); vertex tail28 pack/unpack
   (`pack_vertex_tail28`/`unpack_vertex_tail28`/`try_pack_vertex_tail28`,
   L159-L202); u40 helpers `read_u40`/`write_u40`/`byte_offset_fits`/
   `checked_add_byte_offset(_exclusive_end)` (L21-L66).
3. **BucketLabelKey** — `bucket_label_key.rs`: raw `u16` identity, directedness
   MSB (`BUCKET_LABEL_DIRECTED_BIT`), low-15-bit index masks, index-based
   constructors round-trip, `Ord` = raw order with undirected-before-directed
   grouping (bucket_label_key.rs L11-L115).
4. **Bucket word packing** — `labeled/slot_index.rs`: field layout slot
   `[0,36)` / label key `[36,52)` / overflow-log head byte `[52,60)` /
   reserved nibble `[60,64)`; `try_encode_bucket_word`, decoders,
   `replace_bucket_label_key`, `replace_bucket_overflow_log_head`,
   `bucket_word_has_zero_reserved_bits` (slot_index.rs L10-L75).
5. **LabelBucket validation** — `labeled/record.rs` (subset L18-L459): 29-byte
   wire record; `try_from_parts` field range checks (record.rs L110-L160); wire
   decode `try_read_from` fail-closed checks — reserved nibble zero (L393-L395),
   head bytes `< 170` or `0xFF` (L396-L399, L407-L412), log len ≤ 170
   (L413-L416), log head/len state agreement (L417-L421), no value state
   without schema width (L422-L426); error taxonomy
   `LabelBucketFieldError` (L442-L459).

Properties to prove (headline set):

- P1-roundtrip: decode ∘ encode = id on each valid domain — locator word,
  bucket word, tail28, LogHead byte, BucketLabelKey index constructors.
- P2-injection: encodings are injective on valid domains (distinct fields ⇒
  distinct words; no aliasing between packed regions).
- P3-noninterference: `replace_*` rewrites exactly its own field bits and
  preserves every other field's decode.
- P4-extent: any successfully encoded word has zero reserved nibble; slot
  fields ≤ `SLOT_INDEX_MASK`; successful `checked_add_slot_index s d` ⟺
  `s + d ≤ SLOT_INDEX_MASK` (no wrap possible under that bound), exclusive-end
  variant allows exactly up to `MAX_SLOT_EXCLUSIVE_END`.
- P5-sentinel-faithfulness: `−1 ↔ 0xFF`, `0..=170 ↔ i32` identity for LogHead;
  tail28 `enc = 0 ↔ log_head = −1`, `enc = k+1 ↔ k` otherwise, tombstone bit
  orthogonal to log encoding.
- P6-fail-closed-validation: `try_read_from` accepts exactly the byte images
  satisfying every constraint, and each `LabelBucketFieldError` variant
  characterizes precisely one violated constraint; constructor→wire→decoder
  round-trip never fails.

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
- A-L2 (infallible-constructor preconditions): `.expect(...)` panics
  (encode_bucket_word L25, from_parts L78, read_from L384) become hypotheses
  "inputs are valid" on callers, matching how svd handled Rust asserts.
- A-L3 (byte-layer opacity): fixed-width LE conversions (`from/to_le_bytes`)
  map to trusted Nat conversions; `Storable` framing beyond the 29-byte record
  layout is out of scope.
- Element payloads (`CsrEdge` bytes), stable-memory page mechanics, and all
  Stage 2+ surfaces are out of scope for Stage 1.

## Status

- Stage 0: scope agreed with user 2026-08-26 (audit mode; Stage 1 first slice;
  scan correctness + storage safety as value targets).
- Stage 1: **delivered 2026-08-26, sorry-free, `lake build` green**
  (`Lar/Basic.lean`, `Lar/LogHead.lean`, `Lar/SlotIndex.lean`,
  `Lar/BucketLabelKey.lean`, `Lar/BucketWord.lean`, `Lar/LabelBucket.lean`;
  headline guards in `Lar.lean`; findings in REPORT.md F6–F12). Remaining
  within Stage 1 scope for the next increment: constructor-side
  characterization (`try_from_parts` ↔ decoder agreement on shared checks)
  and the u40 read/write pair.
- Stages 2–3 and deferred class: planned follow-up, not attempted.
