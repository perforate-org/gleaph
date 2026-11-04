# ADR 0097 normal relocation, late reservation failure, and header reads

Implementation: `40faf5f59` (relocation and late-failure witness), `c2088b8e0` (header read batching), `0017ac655` (nonempty-prefix fixture strengthening), `6b144715d` (byte-exact late-failure prefix assertions). Measurements: `978c3ccdb`. Main remains unmerged; these are bounded corrections, not landing approval.

## Normal and bypass leaf owners

The owner is `rebalance_labeled_leaf_weighted_slide_in_block`: both its disjoint streaming and overlapping buffered arms must retain runs, initialized prefix widths, holes and logical ordinals. Both now use the existing non-folding materializer and commit. Explicit compaction still folds through its existing movement-notification path.

The placement defect was in `calculate_label_edge_span_positions_by_resident_slots`: admission summed physical widths, but cursor advancement used `stored_slots().max(degree())`. With retained runs this placed data beyond the admitted region. Placement now reuses the exact admitted physical widths. `bucket_resident_rows`, thresholds, reservation/headroom sizing and slack weights were **not redefined**.

The earlier nine failures were inspected and reproduced without changing resident sizing. Seven were insertion/placement/overlap failures: four mixed-hub tests, weighted-slack rewrite, hub capacity growth, and relocation/reopen. Correcting the placement cursor removed those seven without changing their tests. The remaining two were:

- `single_label_log_fold_reserves_edge_only_tail_headroom`;
- `tree_mode_leaf_actual_counts_slab_edges_only`.

Both exposed the normal insertion trigger's reliance on prefix growth from implicit folding. Promotion now uses the existing logical prefix-plus-used-run extent with the unchanged `T_PROMOTE` and upper cap. The duplicate prefix-only trigger in `tree_write::promote_bucket_if_needed` was removed. The first test was renamed to describe tree promotion and gained an explicit tree-mode assertion; its old extent, empty-run and headroom assertions remain. No threshold or expected count was relaxed.

`normal_and_bypass_relocation_retains_runs_and_values` covers widths 8 and 33, injective sidecars, holes at 0/3/31/63, independent edge/value prefixes, retained edge/value runs, two other normal labels, and a 67-row bypass peer. Setup explicitly materializes the first eight rows **before** creating the retained run and holes; this is not compaction during relocation. Preconditions establish holes in both the nonempty prefix and run, and exact source ordinals/sidecars. The test checks:

- unchanged run IDs and used lengths, descriptors except moved origins, and both traversal orders;
- a genuinely disjoint destination for streaming;
- the same pinned block plus a changed, overlapping bypass-prefix location for buffered copying;
- neighboring labels, counts and active-span/free-span disjointness.

The original old-code red failed when streaming folded the normal run. With the final strengthened fixture, independently restoring folding in either arm fails at that arm (`streaming=true` / `streaming=false`); both mutations were removed. The final library passes **653/0**. Intermediate results are preserved: the non-folding-only experiment had the same nine old failures plus a new-fixture label-lookup error; the corrected fixture and placement had **649/2**, and the admission correction had **651/0**. These are intermediate diagnostics, not final validation.

## Public failure after successful tree preparation

`public_capacity_bucket_reservation_failure_preserves_cached_peer` calls the actual `insert_forward_out_edge` on a full four-byte bypass with sixteen holes. A pre-existing normal peer is populated and its bucket lookup warmed. Fixture-owned allocations exhaust two-descriptor spans until the real bucket store reaches a page boundary.

The public call returns exactly:

```text
Forward(Store(ResizeFailed(GrowFailed { current_size: 5, delta: 5 })))
```

These are **64-KiB pages**. The store's existing four-page growth reserve explains the requested delta; no growth policy changed. Both target edge leaves have already been allocated when bucket reservation fails. The test proves both are returned, the complete root allocation is returned, canonical memory roles, raw source/peer prefixes, source rows and counts are unchanged, and the cached peer remains intact. Retry succeeds, moves the peer descriptor, resolves its lookup at the new address, preserves peer rows in both orders, and checks the source's full native tombstone-inclusive stream and counts. The earlier LTB-growth failure test remains.

No production cleanup fix was required for this witness. A diagnostic omission of one prepared-block release fails `free_count` (1 versus 2); it was removed. Initial fixture failures assumed one memory page, an insufficient fill bound, and a one-page growth delta. They are not production red evidence. The final bound derives from allocated memory bytes and descriptor width.

## One-factor mixed-hub result

Production baseline `40faf5f59` and candidate `c2088b8e0` differ here only in `EdgeSlabStore::read_header`: ten scalar reads become one bounded **48-byte** read. Existing offsets, current-field decoding, magic/version error order, initialization checks and reserved-byte validation remain. There is no cache, layout change, boundary-check removal, fixture change or policy change. A safety test passes on both readers and checks all current fields and error precedence; existing invalid-layout tests remain.

The identical committed [33×50 probe](sources/adr0097-mixed-native.rs) ran sequentially using the normal target. Both runs passed exact order assertions with 1,650 live decodes/callbacks and zero dead decodes; prefix distribution remains `[1, 0, …, 0]`. Only bucket-memory read-call counts changed:

| Direction | Bucket reads before → after | Bucket bytes, unchanged | IC instructions before → after |
|---|---:|---:|---:|
| Ascending | 737 → 134 | 7,670 | 896,221 → 790,432 (−11.80%) |
| Descending | 1,089 → 198 | 8,118 | 713,437 → 557,118 (−21.91%) |

Vertex, slab and run-memory reads/bytes are unchanged, including 132 four-byte run-header reads and 66 payload reads. Header reads become 67 and 99 reads of 48 bytes respectively. Native counts are not IC syscall counts and do not assign individual instruction costs. This improves the branch; it neither reruns nor optimizes the pinned main reference and does not recover all mixed-hub regressions.

[Exact comparison CSV](adr0097-header-read-comparison.csv), raw filtered CSVs [before ascending](sources/adr0097-header-before-asc.csv), [after ascending](sources/adr0097-header-after-asc.csv), [before descending](sources/adr0097-header-before-desc.csv), [after descending](sources/adr0097-header-after-desc.csv), and native output excerpts [before](sources/adr0097-header-native-before.txt)/[after](sources/adr0097-header-native-after.txt) are durable. CSV line endings are LF; numeric contents are unchanged. Raw canbench Δ columns compare against its preserved historical YAML, **not** the one-factor baseline; the table above compares absolute instruction fields.

Post-format measurements at `c2088b8e0` exactly reproduce both mixed-hub CSVs. The after-only nonzero-property traversal control `bounded_inline_promote_full` is **442,969** ([CSV](sources/adr0097-header-final-property-control.csv)); it measures traversal, not preparation, and is not independent evidence for the hybrid/batch change `0b6673a0d`.

## Validation and retained artifacts

- Final strengthened source: `timeout 120 cargo test -p ic-stable-lara --lib`, **653/0**, exit 0 (`$TMPDIR/spill-bucket-reserve/library-raw-prefix-final.log`).
- Final strict library clippy and five-file scoped rustfmt check, both exit 0 (`$TMPDIR/spill-header/clippy-latest.log`, `fmt-latest.log`). An earlier manual-range lint failure was corrected, not suppressed.
- Native probes: positive one-test runs before/after, exit 0. Focused canbench commands: `timeout 210 canbench <name> --csv`, all exit 0, **no `--persist`**.
- `3be28fceb` changes only CRLF to LF in the prior native-comparison CSV; all 65 records are identical. `git diff --check b9e6ada0a..16d25a3c1` failed on its old endings; the corrected actual `b9e6ada0a..HEAD` range passes.
- Full CSV SHA256 remains `5226fd785d56ba528ba5d5a50a08206cf62034e48a9b522cbe254c5c401ea567`; YAML remains `a3e2c76e91a05c7c1a7ccd6fea56d670939b50411d697645257eade4611d162b`; benchmark source remains `bb12331194546dbd91ae1ef5e0b5ba0d3fc141ffd93ba8b71ed956b4b76acbc7`. Generated lock changes were restored and foreign files preserved.

Raw logs, intermediate candidates, final fixtures, fault patches and SHA256 manifests are retained in `$TMPDIR/spill-normal/`, `$TMPDIR/spill-bucket-reserve/`, `$TMPDIR/spill-header/`, and `$TMPDIR/spill-range/`. Fault patches are diagnostics, not changes to stack onto the branch.

Broader relocation failure atomicity, wide-edge and arena exhaustion, other caller shapes, first/second canonical writes, remaining performance regressions, space/PocketIC gates and final full benchmark persistence/all-name review remain open. The independent hybrid/batch contract investigation is now recorded in the [pinned survey and actual-consumer follow-up](adr0097-current-survey-hybrid-batches.md); its correctness repairs and focused measurements do not imply broad recovery. The preserved full benchmark artifact still describes `3ea7dc739`.
