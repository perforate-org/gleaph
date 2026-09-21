# ADR 0097: retained-run append and mixed-hub evidence

## Append lifecycle: reproduced and corrected

The preceding 631/0 result covered reads/deletes, not insertion after conversion. The new
real 67-row bucket-to-tail-bypass lifecycle test failed on its first graph-API append:
4-byte run length remained 62 instead of 63, and 10-byte run length remained 66 instead of 67
(`/tmp/spill-append/red.log`). This confirms the owning insertion decision chose slab space
instead of extending the retained run; the source trace was not treated as runtime evidence.

`c632f5fc5` fixes `EdgeStore::insert_edge_inner`: an active run disallows speculative slab
append regardless of capacity-window hints. It uses the existing checked spill producer and
class-growth path. Slab row growth is preflighted before writing; the spill producer preflights
row growth before modifying a published run. A rejected first-run row preparation releases
its unpublished allocation. Core `Vertex` fallible growth now checks both live and stored width
instead of inheriting saturating growth. No threshold, layout or compaction policy changes.

Writer sites checked: labeled `insert_homogeneous_bypass_edge` delegates to the same core
`EdgeStore::insert_edge`; bypass birth delegates to that helper too. The labeled dispatcher
promotes non-tail bypass rows before entering it. The two core growth sites are the slab branch
and `insert_into_spill_with_layout`, both using the row's checked growth contract. No second bypass-only
insertion rule or capacity-hint patch was added.

The durable 4/10-byte `retained_run_append_preserves_prefix_and_ordinals_*` tests use the real
producer and conversion, append before/after tombstones, cross the actual initial capacity
class (64 for the 4-byte shape, 128 for 10 bytes), and verify both scan directions, all old
and appended `(slot,target)` identities, unchanged initialized prefix and tail-run ownership.
They then fill the run to MAX_ROWS. The tail bypass API propagates the existing
`Store(SpillNeedsPromotion)` outcome; the test asserts exact error, unchanged degree/width/run,
unchanged global edge count and every surviving row. No automatic promotion or forced fold
was added to that bypass path. Core overflow preflight has a separate focused witness.
`CsrVertex::stored_degree` documentation now explicitly permits bucket prefix width below live
degree; it is not a uniform total-width contract.

Validation after append and mixed-reader changes: **634 passed / 0 failed**, touched-file
formatting checks and library clippy with `-D warnings` passed. The old 631/0 record is historical,
not evidence for this lifecycle. Main remains untouched and unmerged.

## Mixed-hub diagnosis: partial improvement, still open

The two benchmark bodies, `seed_mixed_label_hub`, `bench_graph`, BenchEdge and its trait
implementations, and the 33/50 constants are byte-identical to `9a64049f4`. The fixture uses
10-byte BenchEdge rows, two vertices, 33 labels starting at 10000, 50 Insertion rows per label
and uniform(1<<20). This is not the single-label 51-scan fixture. Parity hashes are retained in
`/tmp/spill-append/mixed-parity.json`.

Actual paths differ:

- `bench_l_mix_hub_asc_33x50`: `out_edges -> visit_label_out_edges_inner ->
  visit_all_labels_with_inline_property -> visit_edges_with_inline_property`; its slow
  per-edge loop invokes the property helper even for width zero.
- `bench_l_mix_hub_scn_33x50`: explicitly reads each bucket and invokes descending
  `visit_edges`, the topology visitor. Both ultimately use checked chunked spill replay.

Native fixture shape: prefix widths `[1, 0, ...]`, run lengths `[49, 50, ...]` across all 33
buckets. Each measured path has 132 header reads / 528 bytes, 66 payload reads / 16,490 bytes,
one prefix read / 10 bytes, and 1,650 live decodes/visits with zero dead rows. Baseline ascending
also has 35 vertex reads / 875 bytes and 737 bucket-memory reads / 7,670 bytes; descending has
33 / 825 and 1,089 / 8,118 respectively. These are Memory reads, not IC syscalls.

| Variant | ascending exact | descending exact |
| --- | ---: | ---: |
| Append-corrected baseline | 1,017,187 | 713,437 |
| Skip zero-width property helper | 904,624 | 713,437 |
| Delegate zero-width to topology visitor (rejected) | 996,543 | 713,437 |
| Main fresh console, rounded | 452,960 | 350,320 |

`84ac9203f` keeps only the first factor: avoid the proven no-op helper for width zero.
Reads/bytes/decodes/visits are unchanged; the improvement is execution work, not fewer payload
reads. Empty-property tests retain exact slots and early callback termination in both directions,
alongside nonzero sidecar tests. Topology delegation was rejected: it introduced 33 extra vertex
reads and 363 extra bucket reads through repeated lookup and was slower than the simple guard.
Both experiments and final production were separately measured without persistence. Durable
numbers: [mixed-hub CSV](adr0097-mixed-hub-diagnosis.csv).

The remaining approximately 2x differences against main are **not closed or attributed** by
these experiments. This is not the old scalar spill-read problem: payloads are already batched.
Do not infer that capacity is responsible or that all remaining reader regressions share one cause.

`bounded_inline_64_full` / `bounded_inline_promote_full` use `bounded_inline_bench` with eight-byte
values and `visit_out_inline_property_batches_for_label`; `bench_t_v_in_hyb_ip` uses the incoming
batch visitor with nonzero-width values. Those are different owning APIs from this width-zero
per-edge loop. Their batch/hybrid paths, including the separately unproven `0b6673a0d` guard
change, were not changed or validated by the mixed-hub experiment.

## Remaining gates

The all-label descending bypass concern was source-only at `359212b5f`. It is now reproduced
and corrected by `f142a45de`, as detailed below. This does not establish blanket read-surface
or insertion acceptance.

The reviewed full YAML at `3ea7dc739` is preserved byte-for-byte. Its 189-name comparison and
22 >+5% rows describe that source revision, not a new full run after these changes. No filtered
results were persisted; another complete unfiltered run is deferred while mixed-hub acceptance
and the broader investigation remain open. Space, PocketIC, structural/atomicity review and
other landing gates are not revalidated here. Commands, exact CSVs, counters, reproducible
probe and rejected patch are preserved in `/tmp/spill-append/` and `/tmp/mixed-hub/`.

## All-label read and reverse conversion: runtime correction

`f142a45de` replaces the two bypass arms of `visit_all_labels_with_inline_property` with
`visit_edges` plus label annotation, matching the topology sibling. No new traversal abstraction
was added. The 4/10-byte tests invoke the actual all-label API using the real 67-row conversion
fixture, with nonempty prefix (5/1) and spill (62/66). Their test edge observes label annotation
without adding bytes to its serialized row. Both pre-fix tests failed in descending order:
4-byte output began with 62 zero targets, and 10-byte output was all zero targets instead of
66 down to 0. Exact labels, both orders before/after tombstones, and `Break(0xfeed)` after exactly
three callbacks are asserted. The final tests also check the reverse conversion and zero-live
retained runs; they are not substitutes using only the per-label visitor.

Capacity investigation exposed a second runtime failure in the reverse conversion:
`commit_promote_bypass_to_bucket_mode` panicked with `SpillRunOnNormalRow`. It neither transferred
the run into the bucket nor cleared the vertex's reference, and passed total width as prefix.
`f142a45de` uses the existing initialized-extent owner to build the bucket, preserves its run,
and explicitly clears the old reference at commit. The existing typed setter rejects the sentinel;
a crate-local `without_bypass_spill_run` operation clears ownership without weakening that setter.
Allocation policy is unchanged. Zero live degree with a retained run no longer drops into the
empty/no-run conversion. Tests preserve prefix, used length, exact row ordinals and edge count.

## Checked-growth forwarding: bounded witnesses

`4451efbd3` forwards graph-kernel `Vertex::try_grow_packed_slab_by_one` to `LabeledVertex`.
The runtime red panicked in the infallible inner method instead of returning `Err(())`.
The final test covers live overflow, stored-width overflow, normal label-bucket-count overflow,
and successful forwarding. The wrapper's stale layout test separately failed (25 actual versus
21 expected); it now pins the existing 25-byte layout. No wire layout was changed.

The repository has four production `CsrVertex` implementations: core `Vertex`, `LabelBucket`,
`LabeledVertex`, and graph-kernel `Vertex`. Each now explicitly implements checked growth.
Core width-overflow tests remain; a small bucket test checks overflow and unchanged prefix/run
on spill-backed growth. The wrapper test also exercises the inner bypass and normal-row bounds.
These are concrete implementation witnesses, not a guarantee about arbitrary implementations
using the trait default. GraphStore's `vertex_row.rs` and `vertex.rs` convert the kernel wrapper
to/from `LabeledVertex`; LARA's production `VertexStore` owns `LabeledVertex`, not the wrapper.
The wrapper defect is therefore not claimed as the cause of the earlier native append failure.

## Capacity integration: blocked, not relabeled as success

The actual public `DeferredBidirectionalLabeledLaraGraph::insert_forward_out_edge` calls
`write_half_interruptible(MaterializeRetry::Surface)`. That handles inline-property materialization,
not `SpillNeedsPromotion`; the tail-bypass error is wrapped as
`Forward(Store(SpillNeedsPromotion))`. No promotion occurs. Public diagnostic witnesses used
real insertion and conversion, filled the run without changing thresholds, then verified exact
unchanged vertex/count/rows after rejection:

| Row bytes | Prefix | Used | Stored | Live | Tombstones | Public result |
| ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 4 | 5 | 1024 | 1029 | 1029 | 0 | Forward(Store(SpillNeedsPromotion)) |
| 4 | 5 | 1024 | 1029 | 1021 | 8 | same |
| 10 | 1 | 1024 | 1025 | 1025 | 0 | same |
| 10 | 1 | 1024 | 1025 | 1017 | 8 | same |
| 4 | 5 | 1024 | 1029 | 1013 | 16 | same |

The public-promotion assertion fails after recording the four zero/eight-tombstone cases.
The lower-level exact typed rejection and unchanged-state test is retained in the repository.
This is missing integration, not fulfillment of ADR 0097 §6.

After the transfer correction, invoking the existing 4-byte promoter directly exposed the
reason not to wire it blindly into the caller:

- With eight tombstones, promotion returns `Ok`, releases the run and publishes stored width
  1026/live degree 1021. The scan emits five prefix tombstone sentinels as targets and shifts
  target 8 from slot 8 to slot 5. The exact row assertion fails.
- With sixteen tombstones, promotion returns
  `AllocSpaceCapReached { current_alloc_space: 1018, cap: 1024, mode: Slab }` **after** releasing
  the run and shifting target 16 from slot 16 to slot 5. The unchanged-state assertion fails.

The inspected owner is `promote_bucket_to_tree_mode_impl`: Phase 0a folds the run through
`compact_run_into_prefix_with_growth_retry` before checking the resulting resident width against
`T_PROMOTE`. Used length can reach MAX_ROWS independently of live degree. This needs a promotion
prerequisite that preserves tombstone-inclusive ordinals and checks admission before mutation;
merely catching the signal, lowering `T_PROMOTE`, or forcing that fold is not a safe correction.
The regular bucket arm distinguishes 4-byte tree-capable rows from the wide-edge carve-out,
which uses explicit prefix compaction/growth rather than the four-byte tree. The bypass does
not currently reach either treatment. No wide-edge tree support, threshold, ordering or
compaction policy was introduced. Arena exhaustion and other outer-write atomicity remain unvalidated.

## Validation and comparison availability at 4451efbd3

- LARA library: **637 passed / 0 failed**; final strengthened transfer assertions also passed
  in both focused all-label tests after the full run.
- Graph-kernel library: **202 passed / 0 failed**.
- Strict library clippy for both crates, scoped formatting and diff checks passed.
- Non-persisted correctness-source benchmark checks: mixed ascending **904,471**, descending
  **713,437**, non-tail bypass insertion 256 **363,627**. These are not a main-gap attribution
  experiment; the small differences from previous branch figures are not assigned a cause.

Main-side counters are unavailable under the authorized reference constraint. The existing
`/private/tmp/lara-ntbp-main` at `9a64049f4` has modified `Cargo.lock` and `graph.rs`, plus an
untracked `tmp_ntbp.rs`. The reviewer main checkout is also dirty. Other listed worktrees are
at different revisions. Neither checkout nor its foreign files was modified. No new optimization
was attempted without the required same-fixture main comparison. The nonzero-value batch paths
and `0b6673a0d` remain separate and unvalidated by these measurements.

The complete YAML, workspace full CSV and benchmark source hashes are unchanged. Raw red/green
logs, diagnostic source, patches, caller searches, reference statuses and focused CSVs are in
`/tmp/spill-all-label/`. The capacity probes are deliberately outside production tests: they
remain failing counterexamples, not ignored tests or assertions weakened to bless rejection.
This slice was blocked on promotion correctness/integration, not ready for main landing.
The subsequent [bucket-promotion prerequisite](adr0097-promotion-prerequisite.md) corrects the
promoter in `15ab44efb`; combined public conversion/promotion/retry and main landing remain open.
