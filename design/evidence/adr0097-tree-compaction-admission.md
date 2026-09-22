# ADR 0097: tree siblings in slab-triggered compaction

**Status:** Bounded correctness repair implemented in `90757825c`; independent review open.
**Anchor:** 2026-09-22 06:38:47 UTC +0000.

## Result

Valid Tree-CSR siblings are reachable through slab-triggered whole-span maintenance. On base
`56d381b3f`, after seven legitimate slab moves:

- **Insertion, 33-byte properties:** slab finalization rewrites tree extent **1024 → 1020**
  at tick 8, although native tombstones and surviving high ordinals still occupy that extent.
- **Unordered, width zero:** the swap selector interprets slab bytes beyond the root region
  as tree edges and reports **label 3, slot 1023 → 3**. The observer rejects this bogus tree move.

Both runtime witnesses fail before the production repair and pass afterward. The repair admits
slab mode once in `compact_vertex_edge_span_one_step_with_span_policy`, before ordered/swap
selection, nonzero-cursor fallback and slab metadata finalization. Tree/tiny siblings return
`AdvanceBucket` unchanged. Physical completion may still relocate root bytes; it cannot change
tree logical coordinates.

Final scoped compaction validation is **86 passed / 0 failed**, not a full-library result.
No IC benchmark, full suite, persistence, PocketIC, process management, cold target or main merge
was performed. The approved bounded-read instruction comparison remains historical and unchanged;
this patch neither repeats it nor claims a new performance result.

## Reachability and shared ownership

Production source traced at `56d381b3f`, then checked against `90757825c`:

| Path | Admission and execution |
|---|---|
| `graph/insert.rs::insert_edge_skip_leaf_cascade` and public insertion adapters | Real slab/run insertion; existing promotion dispatch calls `promote_bucket_to_tree_mode` or the prepared bypass publisher. The fixture uses that promoter after real source deletions, not a forged tree flag. |
| `graph/promote.rs::transcribe_and_publish_tree` | Allocates actual LTB edge/property blocks and slab root arrays, publishes the native tombstone-inclusive extent. |
| `graph` facade `store/adjacency.rs` removal | Canonical forward, undirected counterpart and reverse removal paths call `maybe_enqueue_remove_side_compaction`, then drain maintenance. |
| Deferred `maybe_enqueue_remove_side_compaction` | Excludes a tree **trigger**, not tree siblings. Slab tombstones must exceed half its stored width; policies are captured for the entire vertex. |
| `mark_compact_vertex_edge_span` → `CompactVertexEdgeSpanV1` | Starts at bucket zero, regardless of the anchor label. Both orientations call the same one-step owner with `SlackMayBeDropped`; movements go to the observer. |
| `CompactDenseLabeledVertexMaintenanceV1` | After descriptor compaction, queues the same whole-span tag with captured policies. |
| `compact_vertex_edge_span_one_step` / synchronous whole-span driver | Delegate to the same owner with `Required`. The witness also directly checks the Required wrapper at tree bucket 1 with cursor 2. |
| `CompactVertexEdgeAndValueSpanV1` | Both orientations delegate through the Required wrapper. Its existing discarded slab-move results are unchanged, not validated as observer-safe here. |
| `Finished` and peer cascades | Existing physical-only publisher captures root bytes and preserves tree descriptor extent. The fixture proves actual root relocation, not merely permission to relocate. |

The duplicated tiny-only gates were in `bucket_allows_unordered_swap`,
`first_compaction_move_in_bucket`, and `finalize_bucket_slab_metadata`. They are removed;
the existing `BucketMode::from_bucket` decision now owns logical-compaction mode admission.
The remaining run/values eligibility rules are unchanged. Slab-specific helpers are private,
with names reflecting their admitted input. Neither scalar selector nor its movement applier
has a caller outside `compact.rs`; both policy paths and fallback converge under the new gate.
The accepted bounded-read implementation, its physical batching predicate, checked-address
ordering, buffer, lazy decoding and read extent are unchanged. That predicate controls read
granularity, not whole-span logical movement admission.

Tree writers/readers, bypass compaction and physical relocation retain their distinct owners.
No new production helper, parameter, mode flag, state, layout, threshold, policy or dependency
was introduced. No retained run is silently folded by this change.

## Real-backed witnesses and independent oracles

Tests in `labeled/bidirectional/deferred.rs`:

- `tree_sibling_compaction_insertion_preserves_native_tree_and_properties`
- `tree_sibling_compaction_unordered_preserves_native_tree`

They share one fixture constructor in `graph/test_support.rs`, using the existing four-byte
`InlinePropertyTestEdge` and `VectorMemory` graph types. Per test: one populated forward graph,
one empty reverse graph and the existing deferred queue. Across both tests: **four native graph
constructions, zero new graph specializations, zero PocketIC constructors/installs**.
Execution covers the forward storage/queue boundary. Reverse dispatch and facade integration
are source-audited, not runtime integration coverage. The observer is an independent contract
sink, not the full Graph sidecar/postings implementation.

The fixture constructs source vertex 0 with slab label 2 and tree label 3, plus untouched vertex
1/label 2. The 16-row slab prefix is established before introducing the tree. Tree targets
`1000 + slot` are inserted at slots 0..1023; real source deletes at 0, 3, 511 and 1023 are
transcribed by promotion as native tombstones. Extent is **1024**, live degree **1020**, above
the demotion threshold. The existing physical publisher places the real slab/root regions
adjacently; no descriptor extent or root bytes are fabricated.

For Insertion, width 33 gives one edge leaf and multiple independent property leaves. Payloads
encode full `u64(target) + 1`, not a repeating low-byte tag; tombstone properties are zeroed by
promotion. Unordered uses width zero so the baseline genuinely takes the swap-eligible path,
not the non-dense-property fallback to ordered selection. These are two deliberate cases, not
an all-width/all-policy Cartesian matrix or multi-depth tree coverage.

Actual forward slot-removal and remove-side admission calls delete slab slots 0..8. The queue
stays empty through the half-full boundary, then contains one whole-span job at degree 7/stored
16. The tree itself does not enqueue work. The drain visits the slab and then its tree sibling.

Every one-item maintenance tick checks:

- Tree descriptor equality except physical origin; full root bytes and distinct minted IDs.
- All minted LTB headers/payloads and allocation/free-list metadata unchanged; block kinds,
  levels and the edge leaf's exact four-tombstone count.
- **All 1024 native tree rows**, including tombstones and their property bytes, in ascending
  and reversed descending order. Tombstones are excluded only from external live identities,
  never from the native traversal oracle.
- Independent external `(owner, label, slot) → (target, identity)` keys initialized from the
  fixture specification. Only actual observer events rekey them; no final-state map reset.
- Exact live values/identities, untouched neighboring vertex/descriptor/rows, stable global
  live count **1028**, unchanged leaf live count, and active root/prefix ranges disjoint from
  the free-span store.
- At most one slab event per tick and actual bounded completion, not an indefinitely queued retry.

The final event vector is exactly `(label 2, 9+j → j)` for `j=0..6`. Slab stored width becomes
7. The tree root origin moves left **nine physical slots**, while extent, all native rows,
root IDs, LTB bytes and external tree coordinates remain unchanged. A following tick processes
zero work and emits no replay. Required-wrapper nonzero-cursor admission also preserves the tree.

### Finalization discrimination

A deliberate mutation permits tree descriptors past shared admission but makes the common
selector return no tree movement. **Both tests still fail** at tick 8: tree stored extent
becomes 1020 instead of 1024. Fixing selection alone is insufficient; finalization must remain
inside the shared slab boundary. The mutation was removed before final checks.

The valid fixture and test files are byte-identical across the real runtime red, green,
selection-only mutation and final execution. The later production-only visibility/comment
refinement was followed by final formatting, library clippy and all 86 selected tests.

## Setup findings, not compaction reds

The first draft failed before reaching maintenance; its source and log remain preserved:

1. Removing from an already-promoted width-33 tree hits a stale debug assertion in
   `tree_mode_remove_edge_at_slot`: `tree mode rejects inline-property buckets (promote is
   fail-closed)`, left 33/right 0. The final fixture uses the existing delete-before-promotion
   path; it does not claim native property-tree deletion is repaired.
2. The first root-header assertion expected label 3, but the real promoter writes zero
   label/owner/ordinal metadata. Final tests preserve the actual header bytes and verify
   root reachability, not the advertised owner/ordinal validation contract. No forged header
   metadata was inserted to bypass the mismatch.

These separate producer/writer contracts remain open as
[GAP-2026-09-22-001 and GAP-2026-09-22-002](../implementation-gaps.md#gap-2026-09-22-001--property-bearing-tree-deletion-still-asserts-width-zero).
They do not turn successful compaction preservation into general Tree-CSR acceptance.

## Validation and complexity

Repository cwd: `/Users/yota/dev/gleaph-spill`; ordinary configured target, sequential commands.
The [ledger](sources/adr0097-tree-compaction-ledger.json) and
[runner](sources/adr0097-tree-compaction-runner.py) record one cumulative 600-second budget,
checking remaining time and protecting 120 seconds before every non-final command. There was
no startup charge or budget reset in this slice. **15 recorded commands used
51.35271699994337/600 seconds; 548.6472830000566 seconds remained.** Final exact totals and
reserve checks are in [run metadata](sources/adr0097-tree-compaction-run.json).

The four witness/mutation runs all use
`cargo test -p ic-stable-lara --lib tree_sibling_compaction -- --nocapture`.
Final formatting uses `rustfmt --edition 2024 --config skip_children=true --check` on
`graph/compact.rs`, `graph/test_support.rs` and `bidirectional/deferred.rs` under
`crates/ic-stable-lara/src/labeled/`; full argv is retained in the ledger.

| Check | Exit | Command elapsed | Terminal result |
|---|---:|---:|---|
| Initial draft, `--lib tree_sibling_compaction` | 101 | 10.118s | 0/2, setup failures; not compaction reds |
| Corrected fixture, same filter before fix | 101 | 6.913s | 0/2, reachable finalization/bogus-movement defects |
| Shared-admission candidate, same filter | 0 | 10.165s | 2/0 |
| Selection-only mutation, same filter | 101 | 9.568s | 0/2, both fail extent preservation |
| Final scoped rustfmt check | 0 | 0.131s | Three touched Rust files |
| Final `cargo clippy -p ic-stable-lara --lib -- -D warnings` | 0 | 2.816s | Library-only lint |
| Final `cargo test -p ic-stable-lara --lib compact -- --nocapture` | 0 | 8.714s | 86/0, 580 filtered, 0.34s test runtime |

The two new tests are included in 86; totals overlap. Existing test-target warnings are retained,
not fixed or suppressed. Library clippy is not a claim of test-target warning cleanliness.
The last unfiltered library result remains the older 659/0 checkpoint. No unchanged 16K pair,
full library/workspace suite, canbench persistence, space/PocketIC/lifecycle or landing gate ran.
This is mode-admission correctness, not a slab algorithm change; benchmark evidence was not added.
The earlier approximately 726-second slice overrun remains disclosed in its prior evidence;
this separate ledger does not cure it or grant additional validation authorization.

[Lizard 1.24.0](sources/adr0097-tree-compaction-lizard.json), working-tree comparison against
`56d381b3f`, measures three source files. Directly touched declarations: 8 → 13; aggregate CCN
77 → 110 including new tests, maximum 27 → 28. The unchanged retained fixture is excluded after
exact-body comparison despite a diff-boundary overlap. Production-only touched aggregate CCN
is **76 → 74**, NLOC **253 → 252**: three old mode gates become one shared decision. The step
itself changes CCN 27→28/NLOC 90→98; the slab eligibility, selection dispatcher and finalizer
lose one branch each. The left-pack finder remains CCN 14/NLOC 52/tokens 365.

The shared step's existing six parameters (including `self`) are unchanged: owner, bucket/slot
continuation, policy resolver and span policy remain its existing contract. New fixture/check
helpers take one/two parameters; one local test observer owns the independent event map. The
long test is a linear fixture/drain/oracle sequence, not a new production abstraction. Existing
slab eligibility assertions only change the private helper name. Metrics and self-review are
supporting evidence, not independent approval or performance measurement.

## Source and artifact integrity

- Base: `56d381b3fe32d345c8f040fd4de833baee5d3a6b`.
- Final implementation: `90757825cc47cde1c97c4734a452a3be845f9294`.
- Main: `9a64049f47fc07542446d651d4b09421636b73c5`, unchanged and unmerged.
- Raw root: `/tmp/spill-tree-56d-brf3un5q/`, with original setup sources/logs, corrected witness
  patch, before/candidate/mutation/final sources, per-command locks, source hashes, Lizard
  snapshots, the final plan snapshot, verification and SHA256 manifest.
- [Run metadata](sources/adr0097-tree-compaction-run.json) maps preserved logs/patches and
  records transformations. Durable logs trim trailing line whitespace and terminal blank lines;
  raw logs remain untouched. Durable reconstruction patches use zero context and require `git apply --unidiff-zero`.
- The corrected witness patch plus selection-only production patch reconstruct that executed
  mutation from base; it is not a patch to stack on final source. Final committed source equals
  the final-check snapshots. No temporary diagnostic module was registered.
- Foreign brief/CSV, canonical benchmark YAML/config/sources and restored workspace lock match
  their starting hashes. Build-time lock churn is saved separately, not committed. Historical
  incomplete logs, prior manifests and accepted bounded-read measurements are not rewritten;
  the earlier report only gains bounded approval status and a correctness-follow-up link.

The structural final plan check passed (exit 0, 0.057s):
`python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py plans/0335-tree-compaction-admission.md --phase final`.
Its [terminal result](sources/adr0097-tree-compaction-plan-final.txt) is preserved separately
from the historical failing plan logs.
Remaining gates include independent review, tree writer/header-validation prerequisites,
quadratic-scan and other performance work, broader atomicity/space/PocketIC/lifecycle validation,
and final-source complete benchmark/all-name review. None is supplied by this bounded repair.
