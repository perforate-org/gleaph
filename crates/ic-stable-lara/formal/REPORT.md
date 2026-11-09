# Verification Report — ic-stable-lara formal (Stage 1)

## Target and version / Mode

- Crate: `crates/ic-stable-lara` at git `dbd76f3d3441e804336d901798cf21b9d86ac563`
  (working tree clean for all transcribed files).
- Mode: audit of an existing implementation, run as a permanent fixture
  (`lake build` re-checks; see SCOPE.md).
- Lean: core-only `leanprover/lean4:v4.33.1`, package `lara_formal`, lib `Lar`.

## Scope (Stage 0 interview result)

Stage 1 slice only: record/slot arithmetic — LogHead codec, 36-bit slot index
space, locator words, vertex tail28, BucketLabelKey semantics, bucket word
packing, LabelBucket wire validation. Value targets: scan correctness and
storage safety. Stages 2–3 (scan-contract geometry, PMA accounting) and the
range-permutation class are planned follow-up recorded in SCOPE.md.

## Method

Hand transcription with file+line citations on every definition; properties
stated as headline theorems guarded by `#print axioms` in `Lar.lean`. Wire
words are `Nat`; legacy `i32` values are `Int` with explicit domain bounds.
Build status: **green, sorry-free**; axiom dependencies are exactly
`[propext, Quot.sound]`, plus `[Classical.choice]` where `by_cases` splits an
Int order decision (`unpack_pack_canonical`,
`tryReadFromFields_ok_of_wireValid`, `wireValid_of_tryReadFromFields_ok`).

## Assumption list

- A-L1 release semantics: `debug_assert!`s are not obligations
  (slab_index.rs L38, L172, L179).
- A-L2 infallible-constructor preconditions: `.expect(...)` panics become
  caller-side validity hypotheses (labeled/slot_index.rs L25,
  record.rs L78/L107/L384).
- A-L3 byte-layer opacity: LE chunk extraction of the 29-byte record is
  trusted; `tryReadFromFields` consumes extracted fields.
- A-L4 bitwise-or over disjoint packed regions equals addition; used for
  `pack2`/`pack3`/`replaceLow`. Justified by P2-injection lemmas per module.

## Findings per file

### src/log_head.rs

- F6 (SUSPICION, Low): the codec-level round trip `from_byte ∘ as_byte`
  breaks exactly at index 255 colliding with the NONE sentinel
  (`logHeadOfByte_of_logHeadByte_canonical` therefore restricts to the
  canonical domain). Additionally, bytes 170–254 decode to `i32` values
  outside the documented `[0, 170)` domain at this layer. This is by design —
  enforcement lives at record validation (record.rs L396-L399, L407-L412),
  not in the codec — but it means "decoded head ∈ domain" is a property of
  *validated records*, not of the codec. Captured by restricting the Lean
  round-trip theorem rather than weakening the source.
- F7 (Info): `LogHead::from_i32` canonicalizes *every* negative legacy value
  (not just −1) to NONE; the Lean model reproduces this
  (`logHeadFromI32_negative`) so callers passing garbage negatives silently
  get "no log" rather than an error.

### src/slab_index.rs

- F8 (Info, positive): proved that `wrapping_add(1)` in
  `pack_vertex_tail28` (L171) cannot wrap for any reachable `i32` input
  (`tailEnc_noWrap`: max `2^31 − 1`, plus one stays below `2^32`), and that
  `checked_add_slot_index` success is exactly `sum ≤ 2^36 − 1` with no u64
  wrap possible (`checkedAddSlotIndex_spec`). The infallible
  `pack_vertex_tail28` still relies on callers for the 27-bit payload bound;
  `try_pack_vertex_tail28` is the fail-closed variant
  (`tryPackVertexTail28_none_iff` characterizes rejection exactly).

### src/labeled/bucket_label_key.rs

- F9 (Info): directed/undirected construction round-trips through the index
  accessor modulo masking (`keyLabelIndex_of_directed/undirected`), and the
  derived `Ord` contract "all undirected < all directed" holds at raw-wire
  level (`undirected_lt_directed`).

### src/labeled/slot_index.rs

- F10 (Info, positive): bucket-word encoding always produces a zero reserved
  nibble (`encodeBucketWord_reserved_zero`) — dividing out bits `[0,60)` lands
  exactly on the head byte which is `< 2^8`. Encoding is injective on field
  ranges (`bucketWord_encode_injective`) and `replace_*` helpers preserve
  sibling fields via `replaceLow_mid`.

### src/labeled/record.rs

- F11 (Info, positive): the decoder's seven acceptance conditions were
  transcribed as `LabelBucketWireValid` and proved both sufficient
  (`tryReadFromFields_ok_of_wireValid`) and necessary
  (`wireValid_of_tryReadFromFields_ok`): no malformed image passes, and every
  error variant corresponds to one violated constraint.
- F12 (Note): `with_inline_property_bytes_log_head` (record.rs L250-L266)
  preserves validity only inductively (it does not re-check `log_len ≤ 170`);
  out of Stage-1 scope, relevant if setters are ever exposed on unvalidated
  records.

## Findings (severity)

| ID | Severity | Finding |
|----|----------|---------|
| F6 | Low / SUSPICION | Codec-level LogHead round trip breaks at sentinel collision; out-of-domain bytes decode successfully until record validation |
| F7 | Info | Negative legacy heads canonicalize silently to "no log" |
| F8 | Info | No-wrap and exact success-criterion guarantees proved for checked-add/tail28 arithmetic |
| F9 | Info | BucketLabelKey ordering/masking contracts verified |
| F10 | Info | Reserved nibble always zero on encoded words; packing injective |
| F11 | Info | Decoder validation is exactly fail-closed (iff with wire-valid predicate) |
| F12 | Info | Setter validity preservation is inductive-only |

No Critical/High findings. The audit surfaced no defect requiring a Rust
change in the Stage-1 surface.

## List of sorry / unproven spots

None. `lake build` is green with zero errors/warnings and `#print axioms`
shows no `sorryAx` on any headline theorem.

## Conclusion

Stage 1 establishes a verified arithmetic substrate for the labeled layout:
encodings round-trip, packings are injective, checked arithmetic never wraps,
sentinels are faithful, and the record validator is exactly fail-closed.
Next increment (recorded in SCOPE.md): constructor-side characterization
(`try_from_parts` ↔ decoder agreement), then Stage 2 scan-contract geometry.

## Stage 1 retirement note (plan 0372, 2026-11-09)

The LogHead/BucketWord/LabelBucket findings above — including the
`src/log_head.rs` section and findings F6/F7 — describe code deleted by
ADR 0097 and plan 0372. They are retained as a dated historical record; no
replacement transcription exists yet.

## Stage 1 re-anchor (plan 0375, 2026-11-09)

The surviving Stage-1 modules were re-anchored from `dbd76f3d` to
`d75d2467`. The source diff since the old anchor did not change the model's
theorems: `src/slab_index.rs` had dropped the log-head codec (plan 0372) and
named the tail28 payload a spill run (`VERTEX_TAIL_RUN_MASK`,
`try_pack_vertex_tail28(spill_run, …)`), which is numerically the same
`(run + 1)`-in-bits-1–27 encoding the model already proved. The re-anchor
therefore renamed the retired vocabulary (`vertexTailLogMask` →
`vertexTailRunMask`, `logHead` → `spillRun`), refreshed every line citation,
and left the theorem statements and proofs semantically unchanged.
`src/labeled/bucket_label_key.rs` is byte-identical to the old anchor.

`lake build` is green, `Lar.lean` prints the same axiom set as before
(`propext`, `Quot.sound`; `unpack_pack_canonical` additionally uses
`Classical.choice`), and there is no `sorryAx`. The fixture's freshness
manifest is now `anchored`, so future drift in the two cited sources is
blocking. The new exclusive-end/validity helpers in `slab_index.rs`
(`checked_add_*_exclusive_end`, `slot_exclusive_end_fits`,
`validate_elem_capacity`, `read_u40`/`write_u40`) are not modelled; adding
them, or retiring this fixture with the Verus migration (ADR 0104), is a
later slice.

## Axiom-discipline cleanup and CI (plan 0381, 2026-11-09)

`unpack_pack_canonical` was the last lara guard carrying `Classical.choice`: its
`simp` unfoldings of `unpackVertexTail28`/`packVertexTail28` expanded to
`Int.toNat_eq_zero` (which depends on choice). The negative branch (and the
`hpk` helper) now compute the two tombstone cases explicitly, and the positive
branch uses the choice-free `unpack_pack_positive` helper. Every lara guard now
uses only `propext`/`Quot.sound` (or no axioms), so `lean-verify.yml` builds and
axiom-gates lara alongside lhm and vec-deque. The fixture remains anchored; its
retirement follows ADR 0104.
