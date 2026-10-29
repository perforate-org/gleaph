# ADR0097 — Selected tree inline-property reads

**Status:** Bounded GAP006 read repair in `036649e67` independently approved through `f297706ab`.\
Approval covers selected reads only, not deletion/demotion, matching, general atomicity/performance
or landing. Dated raw completion artifacts retain their original pending-review status.\
**Anchor timestamp:** 2026-09-22 10:15:06 UTC +0000\
**Base:** `a80d2e1acad7f282790218ba63f80674807d56e5`, branch `spill-swap`.\
**Scope:** Selected topology/value hydration only, not tree mutation.

The prior independent read-only review approved `70b080ca0`'s canonical topology/dead-missing
admission slice through `a80d2e1ac` (base `e9211fcb5`). That approval remains bounded. Prior fixes
and historical benchmarks retain their existing source-qualified evidence. Main remains
`9a64049f47fc07542446d651d4b09421636b73c5`, unmerged.

## Read ownership

The [selected-read contract](../adr/0050-lara-traverse-read-api.md#7-selected-slot-reads-remain-a-first-class-capability)
belongs to `graph/traverse.rs::visit_edges_at_with_inline_property`. Its slot-window caller
`visit_edges_from_slot_with_inline_property` and `Traversal` adapter delegate to it; no caller
receives a mode guard. The selected owner now dispatches trees before slab live-rank/value reads:

1. The existing canonical `read_edge_state_at_slot` supplies `Live`, `Tombstone` or `Missing`.
   Only `Live` is emitted, with its unchanged canonical logical-slot/label adaptation.
2. Native properties use that same original position through the existing
   `tree_read::read_property_value_at_slot` and property-root resolver. A single width-sized
   buffer is reused and borrowed for each callback. Width zero has no property-root access.
3. Sorting, deduplication and the user's `Break` value retain their contract. No all-or-nothing
   callback/error ordering or new error-after-Break promise is introduced. The existing slab
   path can prepare topology or bulk values before callbacks; raw native streaming has its
   own stronger lazy Break contract.

The entire non-tree selected path, canonical point/replay bodies, `values.rs`, `tree_read.rs`,
writers/removal/deferred code and accepted fixtures are unchanged. Slab values still use their
independent dense live sequence; edge and value prefixes/runs need not align. Native bulk
visitors remain tombstone-inclusive, unlike this selected API.

Three unused private helpers formed a second, slab-only point-hydration pipeline:
`resolve_label_bucket_and_inline_property_bytes_chains`, `read_inline_property_bytes_for_slot`,
and `read_inline_property_bytes_for_ordinal`. [Source search](sources/adr0097-selected-properties-obsolete.txt)
found no live callers outside that chain. They are deleted rather than given another mode
implementation. No new production helper, data structure, index, mapping, cache, module,
dependency, schema, layout, threshold or mutation contract is introduced.

[Before caller inventory](sources/adr0097-selected-properties-callers-before.txt),
[final inventory](sources/adr0097-selected-properties-callers-final.txt).

## Runtime failure before repair

The first command failed **compilation**, not runtime: the fixture named `labeled_memories` in
the wrong module. The diagnostic identified the existing graph-test helper; using that imported
helper changed no fixture shape. [Compile log](sources/adr0097-selected-properties-compile.txt).

The corrected unchanged-production run completed **1 passed / 4 failed**, with the retained-plane
control passing. The width8/33, width-zero and first corrupt-property-root cases panicked in:

```text
collect_out_edge_refs_slot_order
  -> slab row must have exactly `degree` live edges among stored slots
width8/33: left 1025, right 1021
width0:   left 1023, right 1021
```

This is the read collector despite its `lara/edge/insert.rs` filename, not fixture insertion or
tree mutation. The [saved narrow backtrace](sources/adr0097-selected-properties-red-trace.txt)
shows `Traversal::visit_edges_at_with_inline_property` → selected owner →
`bucket_live_ordinal_at_edge_slot` → slab collection. The width8 native bulk sanity oracle has
already passed when the selected call fails. Tree mode and both absent runs are asserted, so the
working compact-range bulk fallback cannot mask the direct path.

The corruption witness's intended `NotMinted` assertion was preempted by the same slab-rank
panic. Do not label its pre-fix run an isolated native-root error test. The later edge-root case
and assertions after the first selected call were not reached in these reds.

[Executed red](sources/adr0097-selected-properties-before.txt),
[exact reconstructible witness patch](sources/adr0097-selected-properties-witness.patch),
[first green: 5/0](sources/adr0097-selected-properties-after.txt).

## Valid-backed witnesses

Five tests reuse `InlinePropertyTestEdge` and `VectorMemory`; there is no new full-graph generic
specialization. One full filter constructs **six native graphs**: three tree widths, one root-error
fixture, and two retained-plane controls. Across commands executing the new witnesses, **30 native
graph constructions** occurred, including overlap/trace/mutation runs; legacy regression fixtures
and the two Wasm scalar-control fixtures are excluded from that count. PocketIC constructors and
canister/federation installs: **zero**.

### Native width8/33 and no-property cases

`selected_property_tree_eight_byte_boundaries`,
`selected_property_tree_thirty_three_byte_boundaries`, and
`selected_property_tree_zero_width` use the same small backed family:

- Sixteen memory handles, initial capacity256, ordinary constructor/default label1.
- Owner0/label2: targets20000..20015, made slab-resident before the tree exists.
- Owner0/label3: targets1000..2023, genuine pre-promotion removals at0/3/511/1023, then real
  promotion and native append2024 at slot1024. Tree extent1025/live1021, two edge-root entries,
  partial second edge leaf. No post-promotion deletion is used.
- Owner1/label3: independent target30000. Total live edges1038.
- Values: full nonzero `u64(target)+1` in the first eight bytes, with `0xa5` in the width33
  remainder. These are target-derived, not rank-derived. Transcribed holes have their producer's
  zero property bytes. Width8 crosses the512-value property leaf boundary and appends a third
  leaf at1024; width33 crosses124-value leaves and exercises the shifted combined-root origin.

Unsorted requests include duplicates, all four dead positions, end1025 and `u32::MAX`. Expected
live positions are independently enumerated as `[1,2,123,124,125,510,512,513,1022,1024]`, reversed
for descending. Tests assert exact targets, logical ordinals, widths, full bytes and callback count.
They exercise both the `Traversal` adapter and direct call, an owned `Break("selected stop")`
after one exact row, and slot windows510..514,1022..1025 and an exhausted start1025 in both orders.

Missing label99 and empty/dead-only requests yield no callbacks. Label2 and owner1 requests return
the distinct slab/peer values at the same positions, protecting label/owner routing. The reused
valued edge type has no label field: these tests check request/bucket label routing, not a new
edge-label adapter. The prior canonical point tests retain their explicit label-adaptation coverage.

Raw native edge/property visitors independently emit all1025 original positions in both orders,
including all four holes and their property bytes. Every read phase is bracketed by equality of
**all16 complete backing memories**, covering canonical vertices, descriptors, roots, headers,
LTB payloads, counts, free-span/allocator backing and neighbors. This does not measure transient
write calls, heap-only caches, IC rollback or Graph sidecar transactions.

### Exact root errors and selection isolation

`selected_property_tree_corrupt_roots_propagate` uses width33. It temporarily replaces either the
second property-root entry (selected slot124) or the partial second edge-root entry (slot1024)
with `ltb.tail_next()`, a genuinely unminted ID. In both orders, selection returns exactly
`LtbBlock(NotMinted { id })`, invokes no callback, and preserves the entire corrupt memory image.
Restoring the exact saved root returns all16 memories to their original image.

A final strengthening also selects an intact row in another leaf while that unselected root is
poisoned. It must return the exact row/value rather than scan through the bad root. No callback
Break is used in that check, and no error-after-Break/prefetch rule is inferred. This catches a
replacement that silently substitutes whole-tree traversal for sparse point selection.

This is unminted-root coverage, not validation of arbitrary minted wrong-kind/owner/ordinal
headers; that remains GAP002. Native property/root helper bodies are unchanged.

### Slab and retained independent planes

`selected_property_retained_independent_planes` reuses the accepted fixture unchanged at widths8/33:
edge prefix8/run9, value prefix13/run1, source holes0/3/9, neighboring label and owner. Sparse
requests normalize to exact positions1/7/8/15/16 in both orders; all full values, descriptor state,
peer values and global count18 are checked. This control passed before the fix; it is not a red.
The native family also covers a separate slab-only label and a width-zero tree. Existing final
traversal/hybrid filters retain dense, sparse, early-Break and borrowed replay regressions.

### Mutation sensitivity and strengthening

Changing only the native property argument to `slot_index.saturating_sub(1)` returns the correct
ten topology rows with wrong values (slot1 reads the preceding dead slot's zeros, for example).
The unchanged width8 assertion fails: [mutation log](sources/adr0097-selected-properties-position-mutation.txt).
The cumulative mutation patch is preserved in the raw root and was removed before final checks.
This is a wrong-position mutation, not an implemented alternate live-rank algorithm.

The executed-red fixture and original selected assertions are preserved. Final tests add the
explicit1038 census assertion and intact-selection/poisoned-unselected-root checks; all fixture
construction and other witness bodies remain unchanged. No failure was relaxed.

## Scoped validation and scalar control

All tests use `cargo test -p ic-stable-lara --lib FILTER -- --nocapture` in the worktree.

| Final filter | Passed / failed | Command seconds | Compiler / test runtime seconds |
|---|---:|---:|---:|
| `selected_property_` |5/0|15.360|13.05 /0.04|
| `labeled::graph::traverse::` |43/0|1.595|1.23 /0.23|
| `tree_` |102/0|1.145|0.60 /0.47|
| `point_state_` |6/0|0.791|0.65 /0.07|
| `hybrid_` |10/0|0.784|0.62 /0.09|
| `retained_compaction` |2/0|0.684|0.58 /0.03|

These filters overlap. Test inventory is677, not a whole-library pass; the last unfiltered659/0
remains historical. Existing15 test-target warnings remain; no new warning is suppressed.
[Final selected](sources/adr0097-selected-properties-final-selected.txt),
[traversal](sources/adr0097-selected-properties-final-traverse.txt),
[tree](sources/adr0097-selected-properties-final-tree.txt),
[point](sources/adr0097-selected-properties-final-point.txt),
[hybrid](sources/adr0097-selected-properties-final-hybrid.txt),
[retained](sources/adr0097-selected-properties-final-retained.txt).

`rustfmt --edition 2024 --config skip_children=true --check` on the one changed Rust file passes
(0.113s). `cargo clippy -p ic-stable-lara --lib -- -D warnings` passes (3.874s).
[Format](sources/adr0097-selected-properties-final-format.txt),
[library clippy](sources/adr0097-selected-properties-final-clippy.txt),
[versions](sources/adr0097-selected-properties-versions.txt).

Paired unchanged `canbench bench_t_vip_tail --csv`, from `crates/ic-stable-lara`, completed with
150s caps: **6214→6218 instructions, +4 (+0.0643707757%)**, heap/stable page growth0 (not zero
allocation). Before94.287s (release compile1m26s), after89.044s (1m22s). No scopes, native-tree pricing, isolated branch
attribution, optimization claim or general regression waiver. Raw CSV deltas against historical
canonical YAML (-19.52%/-19.47%) are not this A/B result.
[Before CSV/log](sources/adr0097-selected-properties-scalar-before.csv) /
[log](sources/adr0097-selected-properties-scalar-before.txt),
[after CSV](sources/adr0097-selected-properties-scalar-after.csv) /
[log](sources/adr0097-selected-properties-scalar-after.txt).

The [paired runner](sources/adr0097-selected-properties-bench-pair.py) temporarily used only the
owned base `traverse.rs` snapshot for the before build, restored the candidate in `finally`, and
preserved the foreign CSV around each command. Normal configured targets only; no reference
checkout, cold target or persistence. Production bytes match the final source; later strengthening
changes only tests. Before Wasm is not separately archived. Sources/logs/CSV/used locks are retained.

## Complexity and preservation

Only `graph/traverse.rs` changes:556 insertions/81 deletions, predominantly tests. Lizard1.24.0,
working tree versus base, conventional `--csv -i -1`:

- Four touched declarations before /nine after; CCN63→82 including tests, max45→51.
- Production: four functions become one; CCN63→51, NLOC286→241, tokens1782→1467.
- Selected owner: CCN45→51, NLOC215→241, tokens1315→1467; Lizard's parameter count7 is unchanged.
  The Rust arguments remain receiver/owner/label/slots/order/callback (six); no option bag or new
  helper hides the existing signature.
- Three unused private helpers removed. Test helpers reuse one graph family and memory snapshots;
  no production state or speculative performance abstraction added.

[Final paired metrics](sources/adr0097-selected-properties-lizard.json) support review, not speed
or safety claims. The initial hunk-only report omitted the before owner because the change was
an insertion-only hunk; paired-name/body matching corrected it. Both raw reports are retained.
A first non-Rust equality check also included the removed inter-method blank separator and stopped
before staging/commit. Corrected comparison preserves exact topology/replay bodies and trims only
that separator; no Rust failure or additional Rust run resulted. Staging then caught a terminal
blank line in the generated directedness excerpt. Audit text now shares the collector's log
whitespace normalization; the raw excerpt is unchanged. The corrected staged check passes.

Source verification confirms unchanged non-tree selection, topology/replay, value/rank and native
property helpers, writers, accepted fixtures, protected benchmark inputs/artifacts, foreign files,
restored lock and main. As-used lock SHA256 remains
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`; the unchanged
[existing lock patch](sources/adr0097-crossover-as-used-lock.patch) reproduces it. Restored lock is
`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.

## Ledger and closure

Raw evidence is preserved at `/tmp/spill-selected-a80-va1dsp1i/`.
One serialized cumulative monotonic600s ledger,120s non-final reserve, zero startup charge; all
Rust/canbench commands checked remaining time and all ledger executions finished without a
timeout/reset. No follow-on Rust validation turn occurred.
[Ledger](sources/adr0097-selected-properties-ledger.json),
[runner](sources/adr0097-selected-properties-runner.py),
[source/provenance](sources/adr0097-selected-properties-run.json).

The **23 measured commands used269.50622937385924s**. An administrative verification shell hit
its35s cap after source/reconstruction checks but before restaging or committing. Progress logging
was added; the same checks then completed in1.095s. The cause of the timeout is unproven, and the
partial output is retained. A **50s conservative administrative charge**, not measured startup or
Rust execution, is recorded after those23 commands in the same ledger. Total charged:
**319.50622937385924s /600s**, leaving **280.49377062614076s** unused. The final runner accounts for
that charge on any later admission; all original command records and the executed runner are
preserved. The [final structural validator](sources/adr0097-selected-properties-plan-final.txt)
passed for local plan `0338-selected-tree-property-reads.md`, the same bounded slice throughout.
The user's correction to the prior review changed no scope or validation budget. Source/lock and
red/mutation reconstruction,29 durable/raw pairs, exact changed-path/link checks,13 protected paths
and the previous150-entry raw manifest passed self-verification. Structural checks are supporting
evidence, not independent approval.

## Remaining boundaries

GAP001/003 live deletion/demotion, remaining004 matching mutation,002 header validation and005
obsolete maintenance remain open. Mutation requires a separately authorized bounded demotion
contract: joint live edge/value selection, preserved ordinals or complete movement publication,
combined-root reservation and prepare/publish/retire failure boundaries. A value filter or
assertion removal alone is not that contract.

A separate source-audited discovery is recorded as GAP-2026-09-22-007: the older public
`remove.rs::for_each_out_edges_by_directedness{,_unchecked}` wrappers still reach a slab-oriented
implementation and attachment helpers. They are not selected-read callers. The similarly named
deferred forward/reverse adapters already call the canonical inline-property visitor instead.
[Source audit](sources/adr0097-selected-properties-directedness.txt); no new runtime failure,
repair, deprecated-surface removal or validation turn is claimed.

No full suite, unchanged16K/native replay, complete/persisted benchmark refresh, new deep-tree or
late-fault matrix, live mutation, Graph/mate-cache transaction, PocketIC, space/lifecycle or landing
gate ran. Native tree performance and broader failure atomicity remain unvalidated. Canonical
benchmark artifacts still describe their historical source, not this final implementation.
