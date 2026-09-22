# ADR0097 — Canonical point-state/liveness reads

**Status:** `70b080ca0`'s bounded topology/no-op admission slice independently approved through `a80d2e1ac` (base `e9211fcb5`).\
**Anchor timestamp:** 2026-09-22 08:19:50 UTC +0000\
**Base:** `e9211fcb52be8087040713dc8091f2948a917734`, branch `spill-swap`.\
**Scope:** GAP-2026-09-22-004's topology point-state and dead/missing admission contract only.

This approval does not cover live property-tree deletion, demotion, matching mutation, header
validation, selected tree-property hydration, obsolete maintenance, broad ADR0097 acceptance or
landing. Main remains
`9a64049f47fc07542446d651d4b09421636b73c5`, unmerged. The earlier deletion investigation and its
assertion-only failures are preserved, not reclassified as successful mutation.

**Subsequent read-only follow-up:** GAP006 selected tree-property hydration is implemented
separately in `036649e67`; [its evidence](adr0097-selected-tree-properties.md) does not expand this
point-state approval. The remaining-boundary audit below describes the `70b080ca0` source state.

## Owner and implementation

The existing [LARA point-read contract](../storage/lara.md#canonical-point-reads) is enforced by
`LabeledLaraGraph::read_edge_state` and its private `read_edge_state_at_slot` in `graph/traverse.rs`.
No new module, index, cache, persistent state, layout, policy or threshold was introduced.

- Tiny, slab/run and tree addressing now meet at that private reader before one liveness
  classification. Live results receive the logical slot and storage label.
- Bypass resolution reuses `bypass_bucket_view`, including the authoritative
  `EdgeStore::initialized_slab_slots` check. Retained spill positions no longer read adjacent LEG.
- The existing native `tree_mode_random_ordinal_access` accepts the already-resolved descriptor,
  checks edge width and logical bounds, then uses the existing root resolver and four-byte LTB
  payload read. Its visibility narrows to `pub(super)`; property width does not affect topology.
  Three existing raw-reader test calls adapt to the descriptor argument, preserving their assertions.
- `edge_exists_at_slot` is a live-only projection of `read_edge_state`, not a second addressing
  implementation. Storage errors propagate rather than becoming false/missing.
- The four direct private-reader consumers are rank/select's `visit_live_edge_slots_until`,
  direct selected-property topology admission, `read_edge_state_internal`, and selected topology
  without a usable replay. Redundant tiny classification and unused reader parameters are removed.
  Validated hybrid replay remains a separate borrowed-replay fast path, unchanged in behavior.
- All six forward/reverse removal/property-update admissions already use the existence helper;
  no per-wrapper mode guard was added. Mutation bodies, matching selection, the stale property
  deletion assertion, demotion and raw native visitor behavior remain unchanged.

[Exact caller inventory](sources/adr0097-point-state-callers.txt) and
[matching/counterpart search](sources/adr0097-point-state-matching-counterpart.txt).
Bypass rank/select already uses its own spill-aware bulk iterator before the private bucket loop;
that existing execution path is retained, not duplicated inside the point reader.

## Test-first evidence

The [six-test witness patch](sources/adr0097-point-state-witness.patch) applies to the base with
`git apply --unidiff-zero` and contains test-only changes. All production sources were unchanged
for the [isolated runtime red](sources/adr0097-point-state-before.txt): **0 passed / 6 failed**.
No mutation assertion was removed to construct or execute these witnesses.

| Witness suffix (`point_state_…`) | Owning contract and independent oracle |
|---|---|
| `tiny_preserves_all_memories` | Real three-row tiny bucket with a deleted interior slot; exact target/label/slot or Tombstone, missing label/end/MAX, both selected orders; all 16 memories unchanged. |
| `slab_and_retained_bypass_preserve_all_memories` | Real 67-row source, prefix 5/run 62, holes 0/5/66; checked prefix/run boundary and every state. Canonical bypass conversion retains the same run/extent. Exact labels/ordinals, selected orders, nonzero preceding-neighbor count; malformed bypass total extent returns `Store(CollectAllocationOverflow)` without memory changes. |
| `tree_native_boundaries_and_errors_preserve_all_memories` | Genuine promotion transcribes holes 0/3/511/1023, then append at 1024 creates a partial second edge leaf. Extent 1025/degree 1021, exact states and selected topology; both raw orders still include tombstones. A deliberately unminted root returns exact `LtbBlock(NotMinted { id })`, not Missing; all 16 memories unchanged around reads/rejection and exact restoration. |
| `property_tree_reads_are_native_and_read_only` | Unchanged accepted `tree_compaction_fixture(33)`: 1024 native positions/1020 live, multiple property leaves, source-transcribed zero-valued holes. Point identities/ordinals and missing bounds agree with construction-derived targets; full native values/order agree with injective target-derived expectations before and after reads. |
| `public_tree_noops_preserve_both_populated_orientations` | Two populated 33-byte fixture stores, not an empty reverse control. All six public admissions return None/false for four dead slots, end/MAX and wrong label. Each case preserves descriptors, vertices, physical root/slab regions, every minted LTB header/payload and allocation tuple, native values/neighbors, global/slab-live counts/free spans and queue bytes. |
| `counterpart_parallel_occurrences_use_live_native_ranks` | Real directed inverse projections: 1024 parallel rows each, source hole 3/mate hole 7 before promotion. Fast and verified lookup map `(2,2),(4,3),(7,6),(8,8),(1023,1023)` in both directions; exact SourceNotLive/SourceNotFound for dead/missing. All 32 memories unchanged. |

The public property no-op stores use the existing fixture's label 3 and are standalone populated
orientation stores, **not a reciprocal directed relation or Graph sidecar transaction**. The separate
zero-property counterpart witness supplies actual reciprocal directed projections. Public snapshots
cover the listed canonical regions, not every byte of their 32 backing memories; the lower-level
point/counterpart witnesses do compare complete memory images. None proves zero write calls or
absence of transient writes solely from final byte equality.

The six new witnesses execute **eight native graph constructions per full `point_state_` run**,
using existing `Row<4>` and `InlinePropertyTestEdge` specializations. The existing `Row<N>` test
adapter gains a non-persisted slot field so ordinals and labels can be asserted, without changing
its wire format or the existing four/ten-byte workloads. Across commands executing these new
witnesses: 43 native graph constructions (including overlapping final filters), excluding legacy
regression fixtures and the two Wasm benchmark fixtures. No PocketIC constructors or installs.

### Exact reds and intermediate outcomes

The isolated red demonstrates:

- Tiny tombstone: point state is dead but separate existence returns true.
- Bypass run slot 6: `Live(Row(0, 1, 0))`, rather than target 2006/label 1/slot 6.
- Edge-only tree slot 0: root ID 0 becomes a live target instead of Tombstone.
- Property tree slot 0: root ID 8 becomes a live target instead of Tombstone.
- Directed counterpart lookup fails with `CounterpartNotFound`.
- Dead-slot public admission reaches the **unchanged** property-tree assertion (33 versus 0).

[Initial green](sources/adr0097-point-state-after.txt) is **6/0**. The final witness strengthens
one bypass rank assertion from before-slot 65/count 0 to before-slot 66/count 1; fixture construction
and all other added witness bodies remain identical to the isolated red. The final six-test run
is also **6/0**. No assertion was weakened to accept a result.

Preserved setup history:

- [First compile failure](sources/adr0097-point-state-compile-type.txt): LTB bulk payload expects
  its existing fixed array, not a Vec. The fixture was corrected to `BLOCK_PAYLOAD_BYTES`.
- [Second compile failure](sources/adr0097-point-state-compile-ownership.txt): the shared valued
  test edge owns a Vec and is Clone, not Copy. The replacement is cloned for the second admission.
  Neither command ran a test or established a production red.
- [Five-test runtime red](sources/adr0097-point-state-five-test-before.txt): 0/5. Tiny failure
  masked the combined fixture's later bypass checks; splitting those cases produced the six
  independent reds above, without dropping tiny assertions.
- [Passing rank mutation](sources/adr0097-point-state-bypass-rank-mutation.txt): 1/0, **not
  sensitivity evidence**. It early-returned from the private loop's newly added bypass arm.
  Audit showed both rank/select callers handle bypass via their existing iterator first, so
  that added handling was unnecessary and was removed. No speculative bypass branch remains.

## Remaining boundaries

GAP-004 is only partially addressed:

1. `remove_edge_matching_skip_leaf_cascade_with_move` still groups Tree/Slab, selects through
   LEG and invokes slab deletion. This is source-audited, unchanged mutation code. No live tree
   predicate/matching deletion was executed or enabled.
2. Direct `visit_edges_at_with_inline_property` now reads correct topology, but still asks
   `bucket_live_ordinal_at_edge_slot` and slab/run value helpers to hydrate it. That value-plane
   path does not implement native tree-property addressing. **GAP-2026-09-22-006** records the
   source-audited prerequisite; this slice does not claim a runtime selected-property tree test.
   Full native property traversal and topology point reads are not substitutes for that API.
3. GAP-001's debug assertion and GAP-003's committed-delete/demotion panic remain. Correct
   liveness does not make live property mutation, demotion publication/retirement or survivor
   movement safe. No threshold was suppressed and no property filter workaround was added.
4. GAP-002 header owner/label/ordinal validation remains separate. Root resolution and byte
   preservation are not proof of those advertised metadata checks. GAP-005's obsolete combined
   maintenance handler still discards moves; this patch changes only tests in `deferred.rs`.
5. New witnesses are depth 1. Existing tree tests run below, but no new multidepth/corruption
   matrix, allocation fault injection, release/Wasm deletion, detach, Graph sidecar/index
   transaction, lifecycle or PocketIC gate ran. No blanket public mutation or counterpart
   integration approval follows the read-only paired witness.

## Focused instruction control

Both commands used the unchanged fixture and normal configured target:

```sh
cd crates/ic-stable-lara
canbench bench_t_vip_tail --csv
```

| Source | Instructions | Command time / exit |
|---|---:|---|
| Base production `e9211fcb5` | 6,142 | 71.597s / 0 |
| Final production (before module-comment-only clarification) | 6,214 | 72.286s / 0 |

**+72 instructions (+1.1723%)**, retained for the correctness/ownership repair, not an optimization.
This selected scalar topology/value caller is not a native-tree benchmark. It has no named scopes;
there is no isolated attribution of the small difference to a specific check or code generation.
Heap/stable page increases are zero, which does not mean zero allocation.

[Before CSV](sources/adr0097-point-state-bench-before.csv),
[after CSV](sources/adr0097-point-state-bench-after.csv),
[before log](sources/adr0097-point-state-bench-before.txt),
[after log](sources/adr0097-point-state-bench-after.txt).
CSV delta columns compare the old canonical YAML, **not** these two observations.

The [paired runner](sources/adr0097-point-state-bench-pair.py) temporarily restores only four
owned production files from start snapshots, records their hashes, then restores the candidate
before the after run. It preserves the foreign CSV around each command. No reference worktree,
cold target, fixture/configuration change or persistence. The before Wasm was not separately
archived; source, used-lock, complete command logs and result CSVs are the retained provenance.

## Final validation

All Rust commands used `-p ic-stable-lara --lib`; test commands append `-- --nocapture`.

| Command / filter | Result | Command seconds |
|---|---|---:|
| `cargo test … point_state_` | 6 passed, 666 filtered | 11.842 |
| `cargo test … labeled::graph::traverse::` | 38 passed, 634 filtered | 1.440 |
| `cargo test … tree_` | 98 passed, 574 filtered | 0.977 |
| `cargo test … spill_read_test` | 17 passed, 655 filtered | 0.661 |
| `cargo test … counterpart` | 17 passed, 655 filtered | 0.615 |
| `cargo test … compact` | 86 passed, 586 filtered | 1.061 |
| `cargo clippy … -- -D warnings` | exit 0 | 3.182 |
| Scoped `rustfmt --edition 2024 --config skip_children=true --check` | exit 0 | 0.452 |
| Comment-only final format check (`tree_read.rs`) | exit 0 | 0.313 |

Filters overlap and are **not additive full-library coverage**. The last unfiltered library
result remains the historical 659/0 descriptor checkpoint. New test count is 666 → 672, not a
new whole-suite pass. Existing test-target warnings remain 15; the new unused-ControlFlow
warning from the first runtime witness was fixed rather than suppressed.

Final logs: [point](sources/adr0097-point-state-final-point.txt),
[traversal](sources/adr0097-point-state-final-traverse.txt),
[tree](sources/adr0097-point-state-final-tree.txt),
[spill](sources/adr0097-point-state-final-spill.txt),
[counterpart](sources/adr0097-point-state-final-counterpart.txt),
[compaction](sources/adr0097-point-state-final-compact.txt),
[clippy](sources/adr0097-point-state-final-clippy.txt),
[format](sources/adr0097-point-state-final-format.txt),
[comment format](sources/adr0097-point-state-final-comment-format.txt),
[versions](sources/adr0097-point-state-versions.txt).
The final module-comment clarification changes no non-comment source line; tests, clippy and
benchmarks were not rerun merely for that prose change. Its scoped format check passed.

### Complexity and scope review

[Lizard 1.24.0 report](sources/adr0097-point-state-lizard.json), working-tree comparison with the
base, six changed source files, conventional CCN (`--csv -i -1`):

- Directly touched declarations: 16 → 26, including tests and test adapters; aggregate CCN
  177 → 185, maximum 45 unchanged. Exact-body equality excludes unchanged hunk-boundary overlaps.
- Seven touched production functions: CCN **139 → 102**, NLOC **605 → 430**. No new production
  helper was introduced; duplicated classification and unused arguments were removed.
- Private bucket point reader: CCN 18 → 14, NLOC 72 → 54, parameters 7 → 3.
- Existence adapter: CCN 15 → 3, NLOC 44 → 18. The selected-property function's CCN remains 45;
  its independent value-plane behavior is not hidden by the point-read refactor.

These metrics support scope review, not speed, atomicity or independent approval.

## Ledger and provenance

One cumulative monotonic 600s ledger, zero startup charge, 120s reserved before every non-final
command. No reset, timeout, cold target, cleanup or process control. The
[ledger](sources/adr0097-point-state-ledger.json) and [run metadata](sources/adr0097-point-state-run.json)
are authoritative for exact command arguments, source/log/lock hashes and cumulative charge.
Raw root: `/tmp/spill-point-e921-fhkzogkf/`.

All **27 recorded commands charged 227.77897737396415s / 600s**, leaving
**372.22102262603585s** unused. The [final structural validator](sources/adr0097-point-state-plan-final.txt)
passed for local plan `0337-canonical-point-state-reads.md`; this closes the bounded read slice,
not GAP-004's remaining mutation scope or independent approval. The first draft used unsupported
`in_progress` and failed; correction to `pending` passed. Both
[failed](sources/adr0097-point-state-plan-draft-failed.txt) and
[corrected](sources/adr0097-point-state-plan-draft.txt) logs are preserved.

The raw root also retains the two compile-stage patches, first five-test patch, initial green
candidate, passing mutation, before/after source snapshots, all per-command locks, Lizard inputs,
text-only reconstructions and verification results. The final source/lock reconstruction verifies
both preserved versions; the existing
[as-used lock patch](sources/adr0097-crossover-as-used-lock.patch) is reused unchanged, not duplicated.

A non-Rust pre-commit equality check initially split `tree_write.rs` at a nonexistent exact cfg
prefix, compared the test module too, and failed; the shell continued to the code commit. The
corrected check uses the actual `mod tests` boundary and proves the production prefix unchanged.
This was a reporting-script error, not a failed Rust gate or an unexamined mutation change.

Protected compaction sources/fixtures, benchmark inputs and canonical artifacts, foreign brief/CSV,
restored workspace lock and main are unchanged. No historical evidence was overwritten. The
canonical full benchmark artifacts still describe `3ea7dc739`, not this source. No full persisted
survey, unchanged 16K/native replay or merge ran. Completion of this slice authorizes no follow-on
execution or landing.
