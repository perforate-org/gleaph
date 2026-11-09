# Legacy LogHead codec retirement (plan 0372)

Anchor timestamp: 2026-11-09 15:00:00 UTC+0900
Scope: plan 0372, implementing ADR 0097's retirement for the 8-bit overflow-log head codec.
Parent commit: `10c137b4` (`fix(lara): accept index 169 in the LogHead boundary and re-freeze the
Verus mirror`, the plan-0371 slice). No canister, Candid, persistence-format, or benchmark surface
changed; the worktree was not committed.

## Result

The shared per-leaf overflow log has been retired since ADR 0097, but its codec layer survived: an
8-bit `LogHead` type, a `slab_index` encode/decode helper set, an i32 descriptor view, and Lean/Verus
transcriptions of that deleted code. Plan 0372 deleted all of it. The 34-byte `LabelBucket` row keeps
its exact wire bytes in all three modes; the only behavioural change is a validation tightening on
malformed rows that no constructor can produce.

## Deleted

Rust (`crates/ic-stable-lara/src`):

- `log_head.rs` (136 lines) and its `lib.rs` module declaration and `LogHead` re-export; the crate
  doc's `log_head >= 0` sentence.
- The six codec helpers in `slab_index.rs`: `encode_overflow_log_byte`, `encode_log_head`,
  `try_encode_overflow_log_byte`, `try_encode_log_head`, `decode_overflow_log_byte`,
  `decode_log_head`, their `use crate::LogHead`, and their four boundary tests.
- `InlinePropertyParts.log_head: i32`; `LabelBucket::inline_property_bytes_log_head() -> i32`; the
  three i32 setters (`try_with_inline_property_bytes_log_head`, `try_with_inline_property_bytes_log`,
  `with_inline_property_bytes_log_head`); the derived-length rule for a live head.
- The `InlinePropertyBytesLogHeadOutOfRange` variant, its `Display` arm, its `LabeledOperationError`
  mapping arm, and the legacy `log_len > 170` bound (the retired log's entry cap).

Formal fixtures:

- Lean `crates/ic-stable-lara/formal/Lar/LogHead.lean`, `Lar/BucketWord.lean`,
  `Lar/LabelBucket.lean` (their subjects are the deleted codec, the removed word-bit head packing,
  and the old 29-byte row validation), plus their imports/`#print axioms` guards in `Lar.lean`.
- The Verus mirror package `crates/ic-stable-lara/formal/verus/` (Cargo files, SCOPE, `src/lib.rs`,
  `stub/`, build output) — a mirror of code that no longer ships is a false contract.

Kept deliberately: `Lar/Basic.lean`, `Lar/SlotIndex.lean`, `Lar/BucketLabelKey.lean` (subjects still
ship; `SlotIndex.lean`'s dead `Lar.LogHead` import became the real `import Lar.Basic` it had been
receiving transitively), the Verus harness (`scripts/setup-verus-stack.sh`,
`scripts/verus-0.2026.10.18`, `.tools/` ignore), and every dated record.

## Replacement API

- `InlinePropertyParts.log_byte: u8` and `LabelBucket::inline_property_bytes_log_byte() -> u8` expose
  wire byte 27 raw.
- `pub(crate) const NO_LOG_BYTE: u8 = 0xFF`, descriptor-owned in `labeled/record.rs`, is the single
  no-log value for slab and tree rows (`OVERFLOW_LOG_NONE` in `slab_index.rs` is gone).
- One infallible `with_inline_property_bytes_log_byte(u8)` replaces the three i32 setters; the pair
  setter had no callers and was deleted. Tree markers keep using `with_tree_mode_physical_depth` /
  `with_tree_mode_property_depth`.
- `log_len` stays raw byte 28 with its mode-specific meaning (slab 0, tree depth marker, tiny used
  width).
- The graph placement snapshot projects the raw bytes: `LabelBucketPlacementInfo` now carries
  `inline_property_bytes_log_byte: u8` / `inline_property_bytes_log_len: u32`, and
  `LeafBucketPlacementStats.total_inline_property_bytes_overflow_log_slots` became
  `total_inline_property_bytes_values_run_rows` (it always summed values-plane spill rows). No
  in-repository consumer used the old names.

## Wire table (unchanged)

`LabelBucket::BYTES = 34`; bytes 27..34 are mode-dependent:

| Mode | Byte 27 | Byte 28 | Bytes 29..34 |
| --- | --- | --- | --- |
| slab | edge run's high-bits low byte (run id data; decoded view is `NO_LOG_BYTE`) | values run id data (decoded view is 0) | rest of the values run id + named reserve |
| tiny | T2 payload top byte | used slot width | zero |
| tree | `0xFF` (`NO_LOG_BYTE`) | packed depth marker (edge depth-1 in bits 0-1, property depth-1 in bits 2-3) | zero |

Byte-level goldens added in `labeled/record.rs`:

- `wire_golden_tiny_mode_round_trips_bytes_27_to_34` — `bytes[27] = 0xAA` (the value the retired i32
  head rejected as `>= 170`), `bytes[28] = 4`, `bytes[29..34] == [0; 5]`, full-row round trip equal.
- `wire_golden_tree_mode_round_trips_no_log_and_depth_marker` — `bytes[27] = 0xFF`,
  `bytes[28] = 1` (edge depth 2 / property depth 1; `byte_width == 0` caps the marker at 2),
  `bytes[29..34] == [0; 5]`, full-row round trip equal.
- `wire_golden_slab_mode_round_trips_run_ids` — edge run `0x0001_2345`, values run `0x89AB_CDEF`;
  `bytes[27] = 0x48` (edge run >> 10), packed low 17 bits equal that byte, values id at bit 17,
  decoded `inline_property_bytes_log_byte == 0xFF` / `log_len == 0`, full-row round trip equal.

`write_to` / `try_read_from` byte bodies are unchanged apart from the sentinel constant's owner; the
goldens are the acceptance gate that a layout change did not slip in.

## Validation narrowing (no migration)

The validating constructors now require:

- slab rows: byte 27 `== NO_LOG_BYTE` and `log_len == 0` (`InlinePropertyBytesLogStateMismatch`);
- tree rows: byte 27 `== NO_LOG_BYTE`; the marker keeps its domain (`len <= 10`, `(len & 0b11) <= 2`,
  and `len <= 2` when `byte_width == 0`) via `InlinePropertyBytesLogLenOutOfRange`;
- tiny rows: unchanged payload rules; byte 27 is T2 payload, so `0xAA` (170) stays legal.

`try_from_parts` only builds slab-shaped rows (its word is re-derived and never carries the tree
bit), so the tree marker domain is enforced on the wire path where tree rows actually arrive. The
tightening means a tree row whose byte 27 is `0..=169` — accepted before this slice — is now
rejected. That needs no migration: no constructor can emit such a row, no persisted byte changed, no
reader of the old head remains, and the repository is pre-production (fresh state or reinstall, not
compatibility). Tests: `slab_parts_reject_a_live_head_byte_and_a_nonzero_length`,
`tree_wire_row_requires_the_no_log_byte_and_keeps_the_marker_domain`; the pre-existing
`tiny_target_slot3_splits_across_three_fields` stays as the tiny-payload tripwire.

## Live documentation updated

Three current-contract documents lost their references to the deleted API in this slice:
`design/storage/inline-property-bytes-first-traversal.md` (dense predicates now read the raw no-log
byte and `edge_run()`), `design/storage/labeled-edge-inline-properties.md` (physical-ownership lines
and fold text), and `design/storage/bulk-ingest-finalize.md` (the dense pre-check predicate). Two of
those files still carry older drift that predates this slice — a 29-byte row table and "Payload
overflow log" / "slab/log traversal" wording; plan 0372's Later Slice 1 owns the full restatement,
and no live sentence there now names the deleted `LogHead` API.

## Validation transcript

| Command | Result |
| --- | --- |
| `cargo test -p ic-stable-lara --lib` (baseline, before edits) | 805 passed, 0 failed |
| `cargo test -p ic-stable-lara --lib` (final) | **799 passed, 0 failed** (805 − 11 deleted codec tests + 5 added) |
| `cargo test -p ic-stable-lara labeled::record` | 35 passed |
| `cargo test -p ic-stable-lara labeled::graph::compact` | 82 passed |
| `cargo test -p ic-stable-lara labeled::graph::tree_write` | 42 passed |
| `cargo test -p ic-stable-lara slab_index` | 4 passed |
| `cargo check -p ic-stable-lara --tests` | pass, no new warnings |
| `cargo check -p ic-stable-lara --features canbench` | pass (compiles the `cfg`-gated bench modules) |
| `cargo fmt -p ic-stable-lara -- --check` | pass |
| `git diff --check` | pass |
| `cargo metadata --no-deps` package count | 56 |
| `(cd crates/ic-stable-lara/formal && lake build)` | `Build completed successfully (6 jobs).`; surviving guards' axioms are `propext`/`Quot.sound`/`Classical.choice`, no `sorryAx` |
| `validate_plan.py --phase draft` | passed before implementation (the gate the slice was reviewed against) |
| `validate_plan.py --phase final` | **structurally valid** at close (TODOs completed, criteria checked) |

Scope greps:

- `rg -n "LogHead|log_head|encode_log_head|decode_log_head|try_encode_overflow_log_byte|OVERFLOW_LOG_NONE|InlinePropertyBytesLogHeadOutOfRange" crates/ic-stable-lara/src` → empty.
- `rg -n "LogHead|logNoneByte|maxLogEntries|tryEncodeOverflowLogByte" crates/ic-stable-lara/formal/Lar crates/ic-stable-lara/formal/Lar.lean` → empty.
- `rg -n "LogHead|log_head|overflow_log_head" crates/ic-stable-lara/formal/SCOPE.md crates/ic-stable-lara/formal/REPORT.md` → five lines, all in the retired-module note or the preserved dated Stage-1 findings.
- `rg -n "inline_property_bytes_log_head|InlinePropertyBytesLogHeadOutOfRange|LogHead|overflow-log head|overflow_log_head" design/` → historical/superseded only: ADR 0001/0045/0050/0088/0096 (dated decisions as of their date), ADR 0097 (implementation note + prior slice narrative) and ADR 0104 (implementation note; the 169-resolution bullet is now history), the plan 0371 and Verus-intake evidence records plus their raw source patch, the append-only `implementation-gaps.md` ledger, the dated Lean-fixture classification record, `storage/inline-property-bytes-first-traversal.md` (one sentence naming the retired field to state it no longer exists), and `storage/lara-dgap-contract.md` (a different `vertex_element` structure).

## Changed-path manifest

Rust (`crates/ic-stable-lara/src`): `labeled/record.rs`, `slab_index.rs`, `lib.rs`, `traits.rs`,
`lara.rs`, `labeled.rs`, `labeled/{bench.rs, bucket_store.rs, invariants.rs, access.rs}`,
`labeled/bidirectional/deferred.rs`,
`labeled/graph/{bucket.rs, compact.rs, error.rs, promote.rs, remove.rs, test_support.rs, tree_read.rs,
tree_write.rs, value_schema.rs, values.rs}`, `lara/{edge.rs, vertex.rs}`, `lara/edge/{bench.rs,
iter.rs}`, `log_head.rs` (deleted).

Formal: `formal/Lar/{LogHead,BucketWord,LabelBucket}.lean` (deleted), `formal/verus/**` (deleted),
`formal/Lar/SlotIndex.lean`, `formal/Lar.lean`, `formal/SCOPE.md`, `formal/REPORT.md`.

Design: `design/adr/0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md` (implementation
note), `design/adr/0104-verus-owns-implementation-proofs.md` (implementation note),
`design/storage/inline-property-bytes-first-traversal.md`, `design/storage/labeled-edge-inline-properties.md`,
`design/storage/bulk-ingest-finalize.md`, and this record.

Plan: `plans/0372-retire-log-head-codec.md` (gitignored by repository policy).

Pre-existing dirty/untracked paths outside this slice (left untouched): `crates/graph-view-core/**`,
`crates/gpui-graph/**`, `crates/cli/**`, the Rune console crates, `demo/knowledge/**`,
`frontend/**`, `pocket-ic-tests` changes, and the design files already dirty at disposition
(`design/README.md`, ADR 0076, `design/demo/**`, the 2026-11-07 investigations, and the untracked
2026-11-09 investigation notes).

## Remaining stale vocabulary (not deleted APIs)

`rg "log_head"` is empty in `src`, but 105 prose mentions of "overflow log"/"overflow-log" remain in
the crate (test names, historical rationale such as "the retired log head expressed this same
branch", and the blob store's still-live `drop_log_site`/`log segment` APIs for
`EdgeInlinePropertyBytesStore`). They name no deleted symbol; restating them is the naming follow-up
below. `traits.rs`'s duplicated doc line for `CsrVertex::spill_run` was corrected.

## Follow-up slices (recorded, not done)

1. Restate `design/storage/labeled-edge-inline-properties.md`: its wire table still says 29 B, and
   its "Stable memories" / "Payload overflow log" / blob-keying sections still describe the deleted
   LVL values log (code now keys blobs by `(run, row)`). This drift predates plan 0372; this slice
   only replaced the four retired API names in it.
2. `bulk-ingest-finalize.md` L7 ("slab/log traversal") is retired-log wording; the dense predicates
   were corrected but that sentence was left as pre-existing drift.
3. Rename surviving `log`-shaped naming where no log remains: `SlotIndex.lean`'s `logHead`
   parameter, `labeled/access.rs`'s `log_vertex`, and the `log_len`-shaped accessors if tree/tiny
   semantics can be expressed without a "log" name.
4. Restate bucket-word packing and the 34-byte `LabelBucket` validation in Lean (or a Verus target
   once adoption resumes) from a fresh anchor, and re-target the Verus harness to a surviving
   module; add the Lean fixture freshness gate per the classification record.
