# Tiny cursor logical extent

## Status and scope

**Implemented; bounded runtime checks completed and independently approved.** This is slice0381
on `spill-swap`, starting from `7651d6e52d602969b9737cb38c52301d532698e9`. It repairs
[GAP-TINY-CURSOR-EXTENT](../implementation-gaps.md#gap-tiny-cursor-extent--cursor-paging-still-treats-tiny-placement-as-empty)
at the LARA read owner. The preceding tiny rank/visitor repair is a separate completed slice.

Strict test-target lint remains failed with the same 41 inherited diagnostics. Neither this
report nor the scoped passes establish full-crate, branch, canonical-lock, Graph integration,
PocketIC, mutation, layout or cursor-performance approval. No Graph workaround, public signature,
helper/type, dependency, writer or persisted-layout change is introduced.

## Reproduction and necessary read prerequisite

`cursor_tiny_pages_keep_used_width` first failed on a genuine four-edge tiny bucket:
start0/limit1 returned `(0, true)` instead of `(1, false)`. Placement correctly reported zero
slab/spill geometry; reconstructing a traversal bound from it was the defect.
`visit_edges_from_slot_with_inline_property` now resolves the normal descriptor and consumes the
existing `values.rs::bucket_reserved_edge_slots` owner, as rank/select does. The duplicate
placement arithmetic is removed. Vertex-owned bypass width, missing-label and zero-limit results,
start clamping, overflow-safe page-end arithmetic and cursor progression are unchanged.

The extent-only candidate exposed a necessary prerequisite in the shared
`visit_edges_at_with_inline_property` reader. A slot2 native tombstone occupies the descriptor's
reused schema bytes; the reader interpreted it as width65535 and read outside value memory.
The saved backtrace reaches `append_ordered_inline_property_bytes_ordinals_next`, not fixture
setup. A direct selected-property call in the same fixture reproduced that failure independently
of cursor paging. Disposition: **Fix now** at this shared read owner, not a cursor-only exception.

The selected reader now dispatches Tiny/Tree/Slab before schema interpretation. Tiny reuses the
existing valueless selected-topology path and canonical point state. Tree native property lookup,
slab live-rank/value hydration and liveness/slot/label normalization are not reimplemented.
No fallback or second decoder is added. The full-label property readers were inspected: their
existing Tiny arms return empty values before consuming schema width as value storage.

[ADR0096 §5](../adr/0096-inline-tiny-mode-for-low-degree-label-buckets.md#5-per-path-rules-dispatch-table)
now names both owners. Its obsolete cursor-exclusion sentence and duplicated selected-read row
are removed; the compaction exclusion is explicitly separate from read cursors.

## Behavioral coverage

Two new unit tests reuse existing memory/graph fixtures; no original test body or assertion changes.

- `cursor_tiny_pages_keep_used_width`: two mirrored orientation graphs, four parallel edges,
  then deletion of slots0/2 leaving slots1/3 with used width4, then a clean empty bucket. Both
  orders check direct selected sorting/deduplication, out-of-range selections, exact callback
  slots/neighbors, empty values, callback `Break`, full/nonzero/tombstone-only pages, exact
  `next_slot`/`exhausted`, zero limits, overflow-safe limits and missing labels. All 32 backing
  memories are byte-identical across each phase's reads. These are LARA callback-slot assertions
  using `TestEdge`, not a new Graph Edge metadata or Deferred wrapper-wiring test.
- `cursor_slab_pages_keep_retained_spill_and_values`: existing independent-plane fixture at
  widths8/33, edge prefix8/run9 versus value prefix13/run1. Both orders check pages across the
  prefix/run boundary and holes, exact slots/attached values and final/out-of-range cursors.
  This test already passed before production changes.
- The unchanged traversal suite retains real tree cursor assertions at widths0/8/33,
  corrupt-root propagation, bypass selected reads and the existing G6 scan proof. The
  unchanged counterpart suite retains the tiny hole-inclusive rank/select regression.

The new tests create four small native LARA graphs total (two mirrored tiny graphs and one
retained-plane graph per width). They add zero PocketIC constructors and zero canister installs.
No full stable-memory preservation claim is made for the slab test's descriptor-only postcondition.

## Commands and source-bound evidence

Evidence root: `$GLEAPH_EVIDENCE/adr0097/tiny-cursor-extent-0381/`.
`initial.json` freezes the empty index, 279 dirty paths, original lock, and 2551 preceding evidence
files. `commands.json` records unique logs, source hashes, elapsed time and the fresh 590-second
cumulative budget; each command is capped at 300 seconds. Cargo/canbench temporarily use the
recorded common lock and restore the original bytes. These results are not canonical-lock validation.

Commands below run from `$REPO` unless noted.

| Command | Terminal result | Log |
|---|---|---|
| `validate_plan.py plans/0381-tiny-cursor-extent.md --phase draft` | Pass | `plan-draft.log` |
| `cargo test -p ic-stable-lara --lib cursor_tiny_pages_keep_used_width -- --nocapture` | Expected RED, one cursor-pair assertion failure | `red-tiny.log` |
| `cargo test -p ic-stable-lara --lib cursor_slab_pages_keep_retained_spill_and_values -- --nocapture` | 1 passed before production changes | `baseline-slab.log` |
| Extent-only `cargo test -p ic-stable-lara --lib cursor_ -- --nocapture` | 3 passed, 1 failed on selected value addressing | `green-cursor.log` |
| Focused diagnostic with `RUST_BACKTRACE=1` | Same native value-memory panic, traced to shared selected reader | `diagnose-tiny.log` |
| Direct-selected regression before its owning fix, with `RUST_BACKTRACE=1` | Expected RED, one native value-memory panic | `red-selected-mode.log` |
| `cargo test -p ic-stable-lara --lib labeled::graph::traverse::tests::cursor_ -- --nocapture` | 2 passed | `green-owner.log` |
| `rustfmt --edition 2024 --check --config skip_children=true crates/ic-stable-lara/src/labeled/graph/traverse.rs` | Pass | `format-final.log` |
| `cargo clippy -p ic-stable-lara --lib --tests -- -D warnings` | Failed: same 41 inherited diagnostics | `lara-clippy.log` |
| `cargo test -p ic-stable-lara --lib labeled::graph::traverse::` | 46 passed | `traverse-final.log` |
| `cargo test -p ic-stable-lara --lib labeled::bidirectional::counterpart::tests::` | 16 passed | `counterpart-final.log` |
| `canbench tiny_scan_d2_1024` in `crates/ic-stable-lara` | Build and focused runtime pass | `bench-build.log` |
| `canbench tiny_scan_d2_1024 --csv --less-verbose` in external `measure/` | 1,332,111 instructions, heap/stable growth0/0 pages | `measure-scan.log` |
| Exact CSV ceiling check | Pass: <=1,350,000 | `perf-gate.log` |

`lint-classification.json` compares all 41 diagnostic identities with the frozen preceding log,
including source-line mapping for the inserted tests. No new/changed line is in that diagnostic set;
no suppression or unrelated cleanup is made. `wasm-binding.json` binds the rebuilt Wasm and unchanged
benchmark fixture to the tested source. The exact scan count matches slice0380's source-bound count.
It is an unchanged sibling-path guard, not cursor-performance evidence; persisted benchmark percentages
are not used as a before/after comparison. No `--persist` or repository benchmark-artifact update occurs.

## Review and commit boundary

Standards and Spec both approved the frozen 160,830-byte candidate packet. Standards completed in
about35 seconds with one file-only inspection; Spec completed in about79 seconds with two. Both
verified all 36 bound inputs without mismatches. Outputs stayed below12,000 characters including
newlines, with no writes, validation execution, intermediate messages or unapproved tools.
The accepted verdicts and audit are `review/{standards,spec}.md` and `review/audit.json`.

The first audit rejected Standards' CLI shell envelope `zsh -c` because the parser assumed only
`zsh -lc`. Inspection showed the same required inner file-only Python command. The owning parser
now normalizes exactly these two observed envelopes; AST restrictions, checksum proofs, tool-count,
output and immutability gates are unchanged. Re-auditing the same candidate/verdicts passed; the
agents were not rerun and the first audit is not reported as a pass (`audit-envelope-contract.json`).
Review preparation also corrected a source-function inventory that omitted public methods; its
failed attempt and exact visibility correction remain in the unique logs.

The final plan validator passed (`plan-final.log`). The index contains only the four planned paths;
reverse-applying the staged patch in an isolated projection restores HEAD exactly. Pre-existing
implementation-gap hunks and all other dirty bytes remain outside the index. Rust and ADR hashes
are identical to the approved candidate; post-review edits only record completion and receipts.

The HEAD-plus-index projection also passed the two named cursor tests and check-only formatting
(`index-cursor.log`, `index-format.log`), using the normal shared target and common-lock borrow.
Both worktree and snapshot locks were restored. Staged Lizard1.24.0 measured cursor CCN9→8 and
selected-reader CCN31→31; aggregate production CCN40→39 and NLOC196→196. The new tests add240 NLOC
without new production helpers, flags, types or parameters (`commit/lizard-review.md`). No unrelated
complexity cleanup is made. The scan measurement is source-bound to the prepared worktree; the
index-only runtime checks do not claim a separately measured index Wasm.

The separate fixing commit is identified by subject `fix(lara): preserve tiny cursor extent and
empty values`, parent `7651d6e52`, and the exact hash/tree/source pins in the external
`commit/final.json` receipt. Final scoped link/immutability/index checks gate that commit.
No push is authorized or performed. Full-suite/canonical-lock checks, inherited lint, fixture
prerequisites and F6 rekey remain separate work.

Static inspection also identifies a non-blocking work-bound mismatch:
[GAP-SPARSE-CURSOR-PREFIX](../implementation-gaps.md#gap-sparse-cursor-prefix--sparse-slab-value-pages-can-rescan-the-cursor-prefix).
Although the cursor selects a bounded position range, sparse property-bearing slab hydration still
scans prior topology positions to recover live value ranks. The existing no-prefix-rescan comments
are not implemented for that branch. This is recorded rather than optimized here; no cursor scaling
measurement or bounded-total-work claim is made. Tiny selected reads do not use that slab rank scan.
