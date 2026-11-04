# 0100. One path per operation, owned by a view

Date: 2026-11-04
Status: Decided, not implemented
Last revised: 2026-11-04
Anchor timestamp: 2026-11-04 05:10:00 UTC +0000

Related: [ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md) owns the spill runs;
[ADR 0098](0098-the-vertex-slice-owns-its-growth-bound.md) owns the vertex slice's extent;
[ADR 0099](0099-inline-property-values-are-slot-addressed.md) is the first operation that already works
this way.

## Context

This session fixed twenty-six defects, and almost every one was the **same shape**: one operation had more
than one path, and the paths disagreed about an invariant. The evidence, all from this branch:

| Defect | The duplicated knowledge |
| --- | --- |
| Double release of a values run | "releasing a run clears the descriptor field" stated in two callers |
| A descriptor naming a released run | "a published descriptor names live runs" checked nowhere |
| Writing a tree bucket's value into the run | "a tree bucket's values live in the property root" restated at four insert sites |
| Empty run kept by an emptied bucket | "an emptied bucket keeps both runs" only in one branch |
| Bucket cache reads failing closed | the cache and the slow path each deciding what "found" means |
| Promotion growing into free space | the slice's extent derived in one place and consumed in another |
| `u32::MAX` sentinel as "no run" | 221 sites each knowing the encoding |
| `CollectAllocationOverflow` as a placeholder | ~300 sites each inventing a meaning for one variant |
| `GrowFailed { 0, 0 }` for a free-span rejection | one mapping hiding another layer's reason |

The three operations that stopped producing defects are the three that got a **view**: counterpart
resolution (`CounterpartResolver`), the value plane (`BucketValuePlane`) and descriptor publication
(`publish_bucket_descriptor`). After each, the bugs in that area stopped, because the invariant moved to a
place where it could only be stated once.

## Decision

Every mutating and reading operation on a labeled bucket gets **one entry point**, a borrowing view that
owns the operation's invariant and the storage-class dispatch:

```rust
BucketRowPlane<'a, E, M>      // insert / read / remove / update of one row
BucketDescriptorPlane<'a, E, M> // exists as `publish_bucket_descriptor`; absorbs the cache
BucketValuePlane<'a, E, M>    // already in place
```

Two rules make the view the single path:

1. **The storage-class dispatch lives inside the view**, not at the call sites. Tiny, slab, spill run and
   tree are four representations of one operation; a caller that knows which one it is has taken over an
   invariant it does not own. Four insert sites became one when `write_after_insert` took the dispatch.
2. **The view's constructor is the only way in.** If a caller can reach the underlying store helper
   directly, the invariant is advisory - which is exactly how `release_span` and
   `CollectAllocationOverflow` ended up meaning nine different things.

## Consequences

- **What it fixes structurally:** the class of defect where two paths disagree, which is every entry in the
  table above. It does not fix a wrong invariant; it fixes "the invariant is restated in N places".
- **Cost:** the views are thin and borrowing, so no new state and no allocation; the work is moving the
  dispatch, and the compiler finds the call sites.
- **Migration order, each independently green:**
  1. **`BucketRowPlane` first**, not the descriptor cache. Inspecting the cache after `6112e27c8`: its whole
     surface is ten lines in `bucket.rs` (`last_bucket_lookup` plus one indexed array) and the three reads
     that could fail closed already fall through, so a view there would add a layer without removing a
     duplicate - the exact thing this ADR is meant to avoid. The row plane is where the four paths
     (tiny / slab / spill run / tree) still dispatch at the call sites.
     - insert first (the value-side dispatch is already unified by `write_after_insert`; the row-side one is
       not), then read, then remove.
     - **Scale, measured before starting:** `insert.rs` is **2 509 lines** with **21** `is_tiny_mode()` /
       `is_tree_mode()` dispatch sites. This is not a rename; it is the module's central function being
       split along the class boundary. It should be done with a fresh budget, in the order
       "one operation, one branch, one test run", not at the end of a session.
     - **The first cut:** a `BucketRowPlane::insert_row(src, label, edge)` that returns
       `(slot, storage)` and owns the tiny / slab / run / tree choice, with the existing body moved into it
       unchanged; call sites shrink to the call. Nothing else changes in the first cut, so the diff is
       reviewable as a move.
  2. `BucketDescriptorPlane` only if a second descriptor path appears - a view is warranted by duplication,
     not by symmetry.
  3. Retire the store helpers the views subsume, so the old path cannot be called at all.
- **Acceptance:** the two ignored regressions become passing
  (`valueless_parallel_edges_promote_past_two_thousand_rows` is already un-ignored;
  `a_large_bucket_does_not_block_a_second_label` is the one this work must un-ignore), and the
  `GAP-GRAPH-TWO-LABELS-ON-A-BIG-VERTEX` diagnosis stops being reachable.

## Why this and not more tests

Every defect in the table was found by a test, and every one recurred in a sibling path that the test did
not cover. Tests find instances; a single path removes the family. The session's own record is the argument:
the areas with a view stopped producing defects, and the areas with call-site dispatch kept producing them
even after each individual site was fixed.
