# Value-prefix reclamation: the pre-repair compaction failure

**Status: Experimental — confirmed pre-repair defect, repaired in [0371](adr0097-value-span-retirement.md).**

This document preserves the0370 diagnosis and numeric observations. The analysis and source line
numbers below describe its frozen source, before the shared-owner repair; linked paths identify the
owners. The diagnosis itself introduced no production code change.

The old 32-slot value prefix becomes unreferenced when a fold relocates the bucket's values.
It is not returned to the free-span allocator, and explicit slab compaction does not recover it.
Native witnesses reproduce **256 lost reusable bytes at width8** and **1,056 at width33**.
Exact target/peer values remain correct; the released values spill run remains reusable.
This is a value-slab retirement defect, distinct from spill-run release or physical page shrink.

This resolves the reclamation question left open by
[0368 fold economics](adr0097-fold-economics.md) and
[0369 value-only folding](adr0097-value-only-fold-economics.md). Their recorded instruction counts
are unchanged and exclude the retirement repair. Repaired-source costs are recorded in0371.

## Minimal witness

Use the existing `FoldFixture` at 64 target edges, widths 8/33. Insert the one-edge peer after the
first 32 target edges to block in-place value-span growth. Fold values only, retaining the edge run.
Each width starts from a fresh graph. The probes use real graph/store APIs, without a mock allocator.

Before folding, every byte below the occupied tail is descriptor-owned and allocator free bytes are
zero. After folding, all 65 live values are slab-resident: exact rows are validated in both orders,
vertex/bucket edge geometry and edge-run identity/length are unchanged, and same-class allocation
reuses the released values-run ID. The target value offset has changed.

The witness then attempts exact allocation of the old prefix, queries the compaction predicate
for one value and for the whole prefix, invokes `compact_inline_property_bytes_slab` explicitly,
and retries exact allocation. Both attempts return false. A successful probe would immediately
return its allocation through `retire_byte_span`; neither failing witness executes that release.
No test-created retirement or registration masks the missing production operation.

| Width | Old prefix | Occupied tail before fold | Occupied tail after fold and compaction | Current reserved bytes | Allocator free bytes | Unaccounted bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 8 | 256 B | 264 B | 776 B | 520 B | 0 B | 256 B |
| 33 | 1,056 B | 1,089 B | 3,201 B | 2,145 B | 0 B | 1,056 B |

In both cases, automatic pressure checks return false and explicit compaction moves 0 spans/0 bytes.
The invariant `occupied_tail = descriptor_reserved_bytes + allocator_free_bytes` fails by exactly
32×width after compaction. It applies to this fresh, fully populated fixture with no pending
reservation; it is not asserted as a universal equation during reservation/rollback.
The exact-allocation failures establish that this is not merely an inaccurate statistics display.

Both tests reach that final invariant assertion and fail there: **0 passed, 2 failed, exit 101**.
They are diagnostic failures, not passing regression tests. The temporary native-only source and
patch are retained externally; the repository source is restored byte-for-byte.

## Why later compaction could not recover this prefix

1. [Value folding](../../crates/ic-stable-lara/src/labeled/graph/compact.rs#L2460) saves the rows,
   removes the values-run reference for slab admission, and calls
   [`ensure_bucket_inline_property_bytes_span`](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L1226).
   The non-tail relocation branch allocates/copies a new span and publishes its offset/length,
   but never calls `retire_byte_span(old_offset, had_bytes)`. The fold later releases the spill run,
   a different allocation.
2. [`reconcile_vertex_inline_property_bytes_allocated_bytes`](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L316)
   sums current bucket spans. It updates accounting; it neither retires the old prefix nor
   rediscovers unreferenced byte ranges.
3. [The allocator](../../crates/ic-stable-lara/src/lara/edge_inline_property.rs#L763) reuses registered
   free spans or appends at the occupied tail. Exact allocation also consults that free-span store.
   The obsolete descriptor no longer identifies the old range, and the allocator has no free record.
4. [Slab compaction](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L62) enumerates live
   descriptors and plans destinations from `free_byte_spans()`. It does not turn gaps between
   descriptors into free spans. Here the first live span is after the unregistered prefix; the
   planner finds no destination at cursor 0 and returns its zero-move result before mutation.
5. [The pressure predicate](../../crates/ic-stable-lara/src/labeled/graph/values.rs#L243) requires
   `free_bytes >= requested_bytes` and an insufficient largest free span. With zero recorded free
   bytes it cannot request compaction for any positive size. The
   [deferred work item](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs#L3662)
   invokes the same compactor; it is not a separate gap-discovery/reclamation path.

The retirement/occupied-tail call-site search found these other paths:

| Site | What it returns; why it does not recover this prefix |
| --- | --- |
| `values.rs:192` | Old source span of a compactor-planned move; no move is planned here. |
| `values.rs:853,942` | Newly reserved materialization spans on write failure; not successful-fold sources. |
| `values.rs:987,1122` | A bucket's currently referenced span or its deleted tail; neither retains the displaced offset. |
| `batch_write.rs:1867–1869` | Failed reservation's appended tail, followed by tail reset; not successful scalar/fold relocation. |
| `batch_write.rs:3101,3137,3436,3902` | Old value spans in batch Slab/SlabHoles/RelocatedSlab/ExpandedSlab publication paths. These have explicit retirement; the scalar span-admission path does not. |

The owning invariant is: **a successfully replaced value span must cease being live and become
allocator-reusable, without any live pointer reaching free bytes**. A repair belongs at the shared
value-span replacement boundary, starting with `ensure_bucket_inline_property_bytes_span`, not as
an extra release in the fold caller. The batch publication paths duplicate this retirement rule;
the required repair had to consolidate that ownership and preserve retirement-capacity preflight,
publication order and failure atomicity. That work is implemented and tested separately in0371.

## Controls, evidence and limits

On the restored source, the scoped `inline_property_bytes_compaction_` filter passes **3 tests**:

- `inline_property_bytes_compaction_moves_only_inline_property_bytes_spans`: registered gaps permit
  moving 2 spans/8 bytes while preserving the asserted remaining topology and live-byte total.
- `inline_property_bytes_compaction_needed_detects_contiguous_allocation_pressure`: registered
  fragmentation distinguishes requests that need compaction from those that do not.
- `inline_property_bytes_compaction_work_item_round_trips_and_runs_once`: existing work-item
  serialization, deduplication and one-item draining. This empty-graph control does not itself prove
  reclamation of a displaced prefix. The filter included it in addition to the two planned controls.

Evidence root (`E`): `$GLEAPH_EVIDENCE/adr0097/value-prefix-reclamation-0370`.
The source is HEAD `24467b17b375b2a7bc462818ef278553a52993fc` plus the preserved dirty spill worktree.
`before.json`, `protected.json`, `worktree-before.patch`, `source.json` and source snapshots bind the
observed state; `probe.rs`, `probe.patch`, `probe-result.json` and `reclamation-probe.log` preserve
both failing witnesses. `commands.json` records exact commands, exits and elapsed time.

```sh
cargo test --locked --offline -p ic-stable-lara --lib --features canbench value_prefix_reclamation_probe -- --nocapture
cargo test --locked --offline -p ic-stable-lara --lib --features canbench inline_property_bytes_compaction_ -- --nocapture
```

The first command requires the archived temporary patch; the second runs on the restored tree.
All Rust commands use the bounded `E/run.py` and verified common lock, restoring original lock bytes.
`registered-free-controls.log` records 3 passes. `format-final.log` and `completion-checks.log` record
formatting and final source/protected/index/lock/HEAD/scope/table/reference checks. The final patch
changes only this report and status text in the two preceding evidence documents; no Rust test or
implementation remains changed by 0370. No commit is made.

No canbench rerun, PocketIC, reopen/fault-injection test, degree 256 witness, repeated-fold workload,
main comparison or independent review is claimed. The one-relocation counterexample is sufficient
to disprove later-compaction recovery in this layout; it does not measure cumulative leakage or
validate a future repair. Existing strict lint debt remains open.

**Resolved in0371:** the shared value-span publication boundary now retires replaced sources.
[Repeated relocation, failure-boundary witnesses and fold remeasurement](adr0097-value-span-retirement.md)
cover fresh state. Existing orphaned prefixes are not repaired in place; no capacity policy is approved.
