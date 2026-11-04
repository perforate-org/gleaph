# ADR 0097: descriptor rejection and the 16K crossover

## Result and scope

`bc5c7d37f` fixes the in-place descriptor rejection boundary, accepted only at that owner boundary.
The pinned investigation reproduces **43,376,650 → 15,498,702,117 instructions** and establishes
canonical target/order agreement, different seeded representations, and bulk versus repeated
single-edge compaction. **It does not establish movement-contract parity:** the historical
`Finished` silently remaps **8,192 survivors without notifications**. Resetting the oracle to
packed rows concealed that defect; the earlier acceptance/classification is withdrawn.

[The movement-publication correction](adr0097-compaction-movement-publication.md) independently
reproduces retained-run completion on `d4c59ff8b` with valid backing and fixes it in `15ceaabf1`.
That small fixture is not a claim that the present 16K slab-only seed reaches the same defect.
No benchmark fixture/cursor or performance-recovery change is made. Main remains unmerged;
all nine historical increases and the wider validation/landing gates remain open.

## Descriptor insertion rejection

`LabelBucketStore::insert_label_bucket_at` must prepare a valid updated normal vertex before
shifting canonical descriptors. Its in-place branch formerly wrote the suffix before the existing
count/row checks. The fix moves that existing preparation before the write, while retaining full
old-row decoding/validation, checked insertion addressing and suffix-only serialization. The
segment-rewrite arm already checks the same `LabeledVertex`-owned count rule before rewriting.
No new guard, cap, layout, API, cache or slack rule is introduced.

`in_place_descriptor_count_rejection_keeps_backed_rows_unchanged` uses the owning
`MAX_VERTEX_LABEL_BUCKETS` (**65,536**), distinct existing descriptors, actual backing for one
additional descriptor, and a valid normal vertex with one slack slot. At indices **0 / 32,768 /
65,536**, it requires exact `RowDegreeOverflow`, identical encoded vertex bytes, and identical
bytes in all three backing memories. All keys are already occupied; duplicate insertion keys
intentionally exercise the store boundary, not a claimed public graph workload.

On the old implementation, all three rejected insertions modify backing bytes while leaving the
vertex unchanged. The corrected implementation passes, together with the unchanged successful
front/middle/tail and corrupt-unchanged-prefix tests. The latter retains its existing validation
panic before a write. An initial assertion did not compile because `LaraOperationError` lacks
`PartialEq`; that is not the runtime red witness.

- [Runtime red](sources/adr0097-descriptor-rejection-red.txt): two existing tests pass; the new
  witness reports mutated indices `[0, 32768, 65536]`.
- [Focused green](sources/adr0097-descriptor-rejection-green.txt): three pass.
- [Final library](sources/adr0097-descriptor-final-library.txt): **659 passed**, 2.73-second test
  runtime; [strict library clippy](sources/adr0097-descriptor-final-clippy.txt) and
  [scoped formatting](sources/adr0097-descriptor-final-format.txt) pass.
- [Working-tree Lizard comparison](sources/adr0097-descriptor-rejection-lizard.md), against
  `7735a5987`: production CCN **24 → 24**, NLOC **70 → 70**; the new test adds CCN 3. This is not
  independent review or general write-atomicity acceptance.

## Pinned workload and identity witnesses

The comparison uses fresh, unique `git archive` snapshots of **`3ea7dc739`** and **`5ad7f5a61`**,
not edits to existing reference checkouts. Neither snapshot contains the descriptor correction.
Benchmark source and configuration remain byte-identical to their pins. The fixture remains a
**ten-byte edge**, capacity `1 << 20`, 16,384 targets inserted through
`insert_edge_skip_leaf_cascade`, ascending even-slot removals, `Insertion` ordering,
`SlackMayBeDropped`, and the existing compaction loop's **zero slot cursor**.

The constructor and seeding are **outside `bench_fn`**. Their work is recorded to explain the
physical state, not added to the measured delete/compact/traverse total.

The [superseded reset-oracle probe](sources/adr0097-crossover-16k-finished-reset-probe.rs)
returned a passing test exit at both revisions and recorded the following facts. Its historical
pass is **not** movement-contract acceptance. In the tables below, “current” means the pinned
`5ad7f5a61`, not the latest branch source:

- all 16,384 seeded `(slot, target) = (i, i)` pairs, ascending and descending;
- every even-slot removal returns exactly that slot's target;
- all 8,192 odd survivors retain their original odd ordinals before explicit compaction;
- every compaction outcome and reported movement is recorded;
- both executions reach **`Finished`**, with **no stopping error**, no `OverflowRewrite`, no tree
  promotion, and no compaction-time growth;
- final traversal in both directions equals the exact packed rows `(j, 2*j+1)`, for `j=0..8191`.

`Finished` has no movement payload and the deferred observer does not remap sidecars on that
outcome. Event replay must therefore equal canonical ordinals at completion as well as before it.
The first historical `(0,1)` versus `(1,1)` failure was a valid signal of **unreported movement**,
not missing-target evidence and not an oracle mistake to normalize away. The later reset to
`(j, 2*j+1)` weakened that invariant. The traces prove canonical targets survived but external
slot-keyed sidecars/postings could remain at stale keys.

The [corrected probe](sources/adr0097-crossover-16k-probe.rs) keeps the event-replayed oracle
at `Finished`, distinguishes errors/pending work from successful completion, and additionally
compares zero and forwarded cursors on byte-identical backing. It was run on `15ceaabf1`, not
rerun on the historical reference trees. Original probe bytes, logs and traces remain preserved.

| State / work | `3ea7dc739` | `5ad7f5a61` |
| --- | ---: | ---: |
| Seeded mode | slab | slab |
| Vertex span width | 15,632 | 17,536 |
| Initialized prefix | 15,632 | 16,384 |
| Run id / used rows | 7,175 / 752 | no run / 0 |
| Seeded bucket degree / graph count | 16,384 / 16,384 | 16,384 / 16,384 |
| After removals: degree / graph count | 8,192 / 8,192 | 8,192 / 8,192 |
| Compactor calls | 2 | 8,194 |
| `EdgeMoved` events | 0 | 8,192 |
| `AdvanceBucket` / `Finished` | 1 / 1 | 1 / 1 |
| Rows repacked at completion without individual events | 8,192 | 0 |
| Final prefix / run / degree | 8,192 / none / 8,192 | 8,192 / none / 8,192 |

The prefix/run shapes do not change during removals. Leaf counts record `actual` 16,384 → 8,192
on both sides; their `total` remains 15,632 and 17,536 respectively, including after compaction.
Thus final logical rows and vertex widths agree, not all physical reservation metadata.

[Historical native output](sources/adr0097-crossover-16k-3ea-native.txt),
[current native output](sources/adr0097-crossover-16k-5ad-native.txt),
[historical steps](sources/adr0097-crossover-16k-3ea-steps.tsv),
[current steps](sources/adr0097-crossover-16k-5ad-steps.tsv),
[historical reported moves](sources/adr0097-crossover-16k-3ea-movements.tsv),
[current reported moves](sources/adr0097-crossover-16k-5ad-movements.tsv), and
[identical final ordinal rows](sources/adr0097-crossover-16k-final-ordinals.tsv).
The current movement trace is checked exhaustively: step `j` moves `2*j+1 → j`.

## Responsible phase and mechanism

[Phase totals](adr0097-crossover-16k-phases.csv) separate construction, seeding, removal,
compaction and each final traversal. Native arrays record `[read calls, read bytes, write calls,
write bytes, grow calls, requested 64-KiB pages]` for the sixteen memory roles. They are `Memory`
observations, **not IC syscall counts or per-read instruction prices**. Native elapsed times
include instrumentation and are not canister latency measurements.

| Phase | Historical read calls | Current read calls | Historical live/dead decodes | Current live/dead decodes |
| --- | ---: | ---: | ---: | ---: |
| Constructor (outside measurement) | 0 | 0 | 0 / 0 | 0 / 0 |
| Seed (outside measurement) | 523,983 | 214,290 | 58,145 / 0 | 18,432 / 0 |
| Remove even slots | 206,304 | 49,152 | 8,192 / 0 | 8,192 / 0 |
| Explicit compaction | 3,115 | 67,182,616 | 8,568 / 8,568 | 33,583,104 / 33,566,720 |
| Final ascending | 13 | 4 | 8,192 / 0 | 8,192 / 0 |
| Final descending (measured direction) | 13 | 4 | 8,192 / 0 | 8,192 / 0 |

The bodies of `compact_vertex_edge_span_one_step_with_span_policy`,
`compact_label_bucket_overflow_to_slab`, and `first_edge_slot_move_in_bucket` are identical at
the two pins; their hashes are in the run metadata. This is not a newly introduced scanner loop.

At the historical pin, appending the 376 live run rows to the existing 15,632-slot prefix would
need 16,008 slots, beyond the available 15,632. The internal fold attempt declines with
`CollectAllocationOverflow`. `SlackMayBeDropped` does not force relocation; the per-edge finder
skips run-backed buckets. `AdvanceBucket` is followed by the existing `Finished` whole-span
rewrite, which packs prefix plus run together. That internal declined fold is **not a stopping
error returned by the outer step API**. Exact final targets pass, so this is not skipped removal
or missing-survivor evidence; nevertheless the absent remaps violate the observer contract.

At `5ad7f5a61`, setup has produced a slab-only bucket. **The benchmark loop supplies zero on
each call and ignores `moved.new_slot_index`; the finder honors the cursor it receives.** Before
move `j`, this benchmark therefore rescans the packed prefix and intervening tombstones up to
old slot `2*j+1`. The 8,192 moves therefore require
`2 * (1 + ... + 8192) = 67,117,056` scan reads. Add the final 16,384-slot scan, 8,192 source reads
when applying moves, and one bulk read at finalization: **67,141,633 edge-slab read calls**,
matching the native role-5 counter exactly. Other metadata reads account for the remainder.

This establishes compaction, not removal or final traversal, as the benchmark's extra-work phase.
Canonical targets/order agree; notification semantics, physical representation and executed work
do not. Production whole-span and deferred callers already advance the cursor to
`moved.new_slot_index + 1`; the 67M-read derivation cannot be attributed to all production calls.
A [paired native diagnostic](adr0097-compaction-movement-publication.md#cursor-attribution)
separately measures forwarded-cursor work. It still scans tombstones repeatedly and does not
establish linear behavior or IC instruction recovery. The earlier insertion/relocation change
that first produced the seed-shape divergence remains unisolated.

### Same-name instruction reproduction

Both invocations use the unchanged benchmark and normal configured target, sequentially:
`canbench bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384 --csv`, **without
`--persist`**, each capped at 150 seconds including its build.

| Pin | Exact instructions | Exit | Command seconds |
| --- | ---: | ---: | ---: |
| `3ea7dc739` | 43,376,650 | 0 | 64.781 |
| `5ad7f5a61` | 15,498,702,117 | 0 | 63.893 |

[Historical CSV](sources/adr0097-crossover-16k-3ea.csv) and
[current CSV](sources/adr0097-crossover-16k-5ad.csv) reproduce the earlier totals exactly. They
contain one total plus ten/nine observed scopes respectively. An absent scope is not measured
zero, and nested scopes are not additive. Raw delta columns refer to preserved YAML. CSV
normalization changes only CRLF to LF, with parsed-record equivalence checked.

**Decision at this checkpoint:** the descriptor correction is retained only at its tested owner
boundary; the historical movement-parity claim is withdrawn. The later notification correction
is linked above. No performance recovery is claimed. Removing repeated work must preserve the
stepped compactor's progress/movement and interleaved-delete contracts. Changing the fixture's
cursor, forcing an implicit fold, or routing slab-only state through bulk completion merely to recover the historical number would change
the workload or step contract, not demonstrate a like-for-like recovery. No contract-preserving
correction has been measured here; that remains the blocking implementation decision for a later
slice. No cache, policy, threshold, layout or slack workaround was introduced.

## Provenance, validation and budget

[Run metadata and hashes](sources/adr0097-descriptor-crossover-run.json),
[complete cumulative ledger](sources/adr0097-descriptor-crossover-ledger.json),
[budget runner](sources/adr0097-descriptor-crossover-run.py), and
[toolchain versions](sources/adr0097-descriptor-crossover-versions.txt).

- Rust `1.100.0-nightly (574ff7d98)`, Cargo `1.100.0-nightly (7941be6fb)`, canbench `0.7.0`.
- Historical reset-oracle probe SHA256 (preserved under `adr0097-crossover-16k-finished-reset-probe.rs`):
  `17379d2b5993849c365f6969754e10f2d3c98d99755ee2c18460eb1ea15d904c`.
- Both pins' benchmark SHA256:
  `bb12331194546dbd91ae1ef5e0b5ba0d3fc141ffd93ba8b71ed956b4b76acbc7`.
- As-used lock SHA256 (both comparisons and final library):
  `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
  [Zero-context lock patch](sources/adr0097-crossover-as-used-lock.patch), applied with
  `git apply --unidiff-zero`, reconstructs it byte-for-byte from the pinned
  `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2` lock. The workspace lock
  is restored; the pinned/restored hash is not the as-used hash. Durable text-log copies remove
  terminal blank lines only; ledger log hashes describe the complete raw files retained below.

Archive mtimes exposed stale shared-target reuse on the first candidate run: 631 filtered tests
and historical header traffic, rather than the candidate's 653 filtered tests. That observation
is **invalid for the candidate** and excluded. New snapshot source timestamps were refreshed
without changing bytes or cleaning/changing the normal target. The common probe checks its
compiled manifest against the expected snapshot using canonical paths. An initial raw-string
manifest check rejected macOS `/tmp` versus `/private/tmp` aliases; those setup failures are not
production red evidence. Source-verified reset-oracle runs each exited **1/0**, but that does not
validate the historical movement contract. Canbench logs record
compilation from the intended snapshots; separate wasm hashes and exact CSVs are retained.

All Rust/toolchain/canbench execution, including failed diagnostics, compilation and final checks,
uses one serialized monotonic ledger: **259.983 seconds of 600**, with 120 seconds reserved until
final checks. No more observation is required for this slice. The prior survey's approximately
**726-second breach remains disclosed** in the previous report; this new budget does not erase it.

Raw complete logs, initial/corrected probes, snapshots, locks, wasm files, traces, Lizard CSVs,
patches and SHA256 manifest remain under `$TMPDIR/spill-p2-16k-7735.fmxSXg/`. Diagnostic snapshots and
patches are not changes to stack onto the branch. Workspace historical CSV/YAML, benchmark source,
foreign files and main remain unchanged. There is no new full survey/persistence, independent
review, space/PocketIC validation, broader atomicity acceptance, or landing approval.
