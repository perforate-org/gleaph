# 2026-11-07: the bucket lookup family, measured

ADR 0100 lists bucket lookup as the next remaining operation family: "`(src, label) -> bucket` with the last-lookup
cache, descriptor-row origin, create/destroy and slack". This is the measurement a cut should start from.

## What exists

| Measure | Count |
| --- | --- |
| `find_bucket` callers | 206 |
| `find_bucket_slot` callers | 69 |
| `read_label_bucket_slot` callers | 230 |
| Direct `write_label_bucket_slot` callers | 39 |
| Manual cache-invalidation sites | 9 |
| Bucket-row mutation call sites | 7 (6 in `bucket_store`, 1 graph-level entry in `insert`) |

## The lookup itself is already one function

`find_bucket` is a single ~300-line implementation with layered fast paths: the last-bucket cache, a per-label cache
array, `degree == 1` and `degree == 2` special cases, a bulk contiguous read plus binary search above
`BULK_BUCKET_SEARCH_MIN_DEGREE`, and a slow scan. There is **no second implementation of the lookup**, which is why
this family is smaller than its caller count suggests. What it does have is a large invariant surface.

## The duplication is the cache rule, not the search

The caches are correct only while they agree with the vertex's bucket row. Nine sites answer that by hand:

| File | Sites |
| --- | --- |
| `graph/insert.rs` | 3 |
| `graph/bypass.rs` | 2 |
| `graph/bucket.rs` | 1 |
| `graph/promote.rs` | 1 |
| `graph/test_support.rs` | 1 |

Each is a row or descriptor rewrite followed by `invalidate_bucket_lookup_caches_for_bucket_segment`. The rule is
stated in one place (`bucket.rs`) and applied in nine, which is ADR 0100's shape exactly: an operation ("the bucket
row changed") whose consequence ("the caches that described it are void") is every writer's job to remember.

One of the nine is conditional - `insert_label_bucket_at` reports whether it rewrote the segment, and only then does
the caller invalidate - which is the signal that a single helper must keep that bit rather than invalidate
unconditionally on the hot path.

## The other half: create and destroy

`BucketSearch::Missing { insert_index }` is what the create path consumes: it computes the index, calls
`insert_label_bucket_at`, then re-reads the vertex, the descriptor index and the bucket. Destroy mirrors it. This is
the part with the most coupling to slack (`bucket_row_and_slack`), and it is why the family is medium rather than
small: the row, the descriptor and the vertex's slack are three views of one change.

## The cut

A `BucketLookupPlane` whose one entry answers `(src, label) -> BucketSearch` and which owns the caches, plus the
single operation "the following row/descriptor rewrite happened, reconcile the caches":

1. `resolve(src, label) -> BucketSearch` - the current `find_bucket`, unchanged in behaviour.
2. `rewrite_row(vid, rewrite) -> Result<T>` - performs the rewrite and drops the caches that described the row,
   keeping the conditional case (`insert_label_bucket_at`'s flag) as an argument rather than a caller's `if`.
3. creation and destruction go through the same plane, so `insert_index` and the re-read after the insert are its
   business rather than each caller's.

The number to beat: **manual invalidation sites 9 -> 1** (the plane's own `rewrite_row`). A cut that leaves any
caller calling `invalidate_bucket_lookup_caches_for_bucket_segment` has not cut this family.

## Why this is a session of its own

206 lookup callers must keep compiling while the plane is introduced, and the create/destroy half touches slack
accounting, which the promotion fixtures already exercise. The Visit family took nine slices for the same reason:
the type had to exist before the callers could name it. The order that worked there applies here - introduce the
plane and its entries, prove them with tests, then move callers in batches.

## Slice 1: the plane exists, and one caller moved

`BucketLookupPlane` (`graph/lookup_plane.rs`) now owns the family's two questions:

| Entry | What it answers |
| --- | --- |
| `resolve(src, label)` | where the bucket is, or where it would go - the graph's own `BucketSearch`, unchanged |
| `rewrite_row(vid, rewrite)` | performs a bucket-row rewrite and voids the caches that described it |

The design detail that mattered: `rewrite_row`'s closure returns `(value, moved)` rather than taking `moved` as an
argument. The first attempt passed the bit in and the insertion site had to pass `true` because the answer only
exists *after* the rewrite - which is precisely the `if` this plane is meant to remove. With the bit in the return,
the writer states it where it is known and cannot perform the rewrite without answering it.

Three tests pin the plane: `resolve` agrees with the lookup it owns (found and missing), a failing rewrite publishes
and invalidates nothing, and a successful one hands back what it published. The conditional insert site
(`insert_label_bucket_at` reporting whether it rewrote the segment) is the first caller, so the plane's reason for
existing is exercised in production code rather than only in tests.

| Measure | Before | Now |
| --- | --- | --- |
| Manual invalidation sites | 9 | 8 (one moved behind the plane) |

The next slices are the remaining eight sites, then creation and destruction through the same plane. The number to
beat stays **9 -> 1**.

## Slice 2: every cache-affecting writer now goes through the plane, and the number is 1

All eight remaining manual sites moved behind the plane, each as the rewrite it actually is:

| Site | How it reads now |
| --- | --- |
| promotion | `rewrite_row(vid, |_| { ready.publish(); counts.write(); Ok(((), was_default)) })` |
| segment compaction (insert) | `rewrite_row(vid, |g| { g.buckets.compact_vertex_segment…; Ok(((), true)) })` |
| bypass, two handoffs | `rewrite_row(src, |_| { …publish…; Ok(((), true)) })` |
| mode handoff (insert) | `rewrite_row(src, |_| { …publish…; Ok(((), true)) })` |
| fixture installing a tree bucket | `rewrite_row(vid, |_| Ok(((), true)))` |

Clearing a row joined the plane too: `clear_row(vid)` is `rewrite_row` with no survivor, and the graph's
`clear_vertex_label_buckets_for_segment` - which already coupled the two - is gone, so the coupling has one home
rather than two.

Promotion's site is the interesting one: it invalidates only when the vertex *was* default-edge labelled, because
that is when the row it searched changes. The plane takes that as the rewrite's own answer, which is the same shape
the conditional insert needed.

| Measure | Before this record | Now |
| --- | --- | --- |
| Manual invalidation sites | 9 | **0** |
| Places that invalidate | 9 | **1** (`lookup_plane::rewrite_row`) |

The number to beat was 9 -> 1, and it is 1: `invalidate_bucket_lookup_caches_for_bucket_segment` is called only by
the plane, and every writer that changes a bucket row names the plane's operation instead.

What is left in this family is creation and destruction - `BucketSearch::Missing { insert_index }` and the re-read
that follows an insert - which the plane does not yet own.

## Slice 3: the create/destroy half is smaller than the family's name suggests

The family's description says "create/destroy and slack", which sounds like a second half of equal size. Measured, it
is not:

| Path | Production call sites |
| --- | --- |
| `insert_label_bucket_at` (create one label's row) | **1** (`insert.rs`, already behind the plane) |
| `clear_vertex_label_buckets` (destroy a vertex's row) | **1**, through the plane's `clear_row` |
| `remove_label_bucket` (destroy one label's row) | **0** - only its own test calls it |

So creation and destruction are already one path each, and the one that looks like a third path - per-label removal -
has no caller. That is not a family to refactor but a dead method to delete, which is the pre-production rule in
AGENTS.md: one canonical current layout, no superseded paths, and a store API whose only caller is its own test is
exactly the kind of second implementation this series removes.

## Where this family stands

| Measure | Before the family | Now |
| --- | --- | --- |
| Lookup implementations | 1 | 1 |
| Manual cache-invalidation sites | 9 | **0** |
| Places that invalidate | 9 | **1** (`lookup_plane::rewrite_row`) |
| Production create paths | 1 | 1 (behind the plane) |
| Production destroy paths | 2 (`clear`, `remove_label_bucket`) | **1** (`clear_row`), once the dead one goes |

The plane owns the family's questions: `resolve`, `rewrite_row`, `clear_row`. What remains is deleting
`remove_label_bucket` and its test, after which the bucket lookup family is complete and the next family is inline
value schema.

## Deleting `remove_label_bucket` is not mechanical, and that is itself a finding

The deletion was attempted and reverted. Cutting the method out of `bucket_store.rs` leaves the crate failing to
build, because its neighbours in that region are `#[cfg(test)]`-gated: `plan_enable_default_edge_bypass` sits right
after it behind `#[cfg(test)]`, and `insert.rs`'s mode-handoff path reaches it. A naive range delete takes an
attribute with it and the failure surfaces far from the cut.

So the deletion needs the surrounding `cfg` boundary read first, and the method's test - which exercises segment
rewrite, slack and descriptor rows - has to go with it. That is a small, careful change rather than a line in a
bigger commit, and it is the last thing this family needs before it is complete.

Recorded rather than forced: the tree is green at the state where the family's real work is done, and the removal is
a named, bounded follow-up instead of a half-applied edit.
