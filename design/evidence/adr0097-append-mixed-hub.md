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

Source-only structural follow-up, not runtime-verified here: the bypass descending arm of
`visit_all_labels_with_inline_property` still constructs a slab-only iterator, unlike the
per-label iterator paths covered by the lifecycle tests. It needs its own retained-run witness;
no claim that all bypass read surfaces are validated is made.

The reviewed full YAML at `3ea7dc739` is preserved byte-for-byte. Its 189-name comparison and
22 >+5% rows describe that source revision, not a new full run after these changes. No filtered
results were persisted; another complete unfiltered run is deferred while mixed-hub acceptance
and the broader investigation remain open. Space, PocketIC, structural/atomicity review and
other landing gates are not revalidated here. Commands, exact CSVs, counters, reproducible
probe and rejected patch are preserved in `/tmp/spill-append/` and `/tmp/mixed-hub/`.
