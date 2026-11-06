# 0100. One path per operation, owned by a view

Date: 2026-11-04
Status: Accepted (2026-11-05) - implemented for rows, a bucket's span, a vertex's space and a tree's roots,
with direct tests for every view; the remaining operation families are listed in Progress
Last revised: 2026-11-06
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

## Progress (2026-11-04)

The first three cuts landed, each as a move with every test unchanged:

| Cut | What moved | Evidence |
| --- | --- | --- |
| `d30946552` | row insert (400 lines, 21 storage-class dispatch sites) into `BucketRowPlane::insert_row` | 705 / 1110 / 642, tests untouched |
| `c19184d8b` | row remove (the tiny / tree / slab dispatch) into `BucketRowPlane::remove_row` | same |
| `23a1a65a3` | row read (`read_row_state`, `read_row`) into `BucketRowPlane` | same |

`BucketRowPlane` now owns the whole row lifecycle, so the three operations that decide what a row is
cannot grow separate ideas of it. The graph's entry points delegate.

The value-read slice is also in: `read_value_row` / `read_value_row_with_chains` / `read_value_range`
resolve the storage class once (tree property root, slab prefix, spill run) and state the zero-width rule in
one place, replacing the sixteen callers of `read_bucket_inline_property_bytes_for_slot`
(`babf32061`, `179ad1d46`). The dense slab read keeps its place as **the** batch fast path with a stated
precondition - an optimisation inside the plane, not an entry point a caller picks. Both the fold benches
and the expand benches report no significant change.

**Step 3, as far as it goes** (`fdc348b99`): `insert_edge_skip_leaf_cascade_impl` - the internal wrapper the
cut left behind - is gone, and the graph's two insert entry points plus the tiny-mode helper that re-enters
after promotion ask `BucketRowPlane::insert_row` themselves. The view's own retry calls itself instead of
bouncing back through the graph.

Step 3 finished (`c2abc0254`): the graph exposes `row_plane()` and the in-crate callers
(`bidirectional/deferred` 5, `bidirectional/counterpart` 3, `traverse` 4, `remove` 2, `tree_write` 2) reach
insert, remove and read through it. That retired the delegating twins -
`read_edge_state_internal`, `remove_edge_at_slot_with_move` and the internal insert wrapper - so there is one
implementation and one door rather than a door beside a duplicate path.

## Unit two: a bucket's span (2026-11-05)

`BucketLayout` (`8507e1be`, `bf568410`, `fe6bd36f`) takes over what happens to a bucket's physical span:
`release_span`, `ensure_room_for`, `fold_head`, `rebuild_run`, `promote_to_tree`, `demote_to_slab`. The
placement (`LeafArena`), the ledger (`FreeSpanStore`) and the tree transcription (`tree_write`) stay in their
own layers; the view composes them so an operation says "release this", "make room for this much", "demote"
instead of restating the span rules.

Two rules moved inside, because both were being answered at every site:
- **The descriptor is published with the row, never apart from it.** `finish_stored_row` (`ff6c3fc2`) writes
  the block and publishes in one operation. Its first draft returned early for a width-0 bucket and took the
  publication with it - 81 tests failed immediately, which is why the publication is now stated as
  unconditional in the code.
- **Promotion hands back what it published.** Promotion rewrites the descriptor's residency, so a caller
  holding the pre-promotion `LabelBucket` can publish a slab descriptor over a tree root. The view consumes
  the stale value and returns the new one; the four-line re-find that used to follow every promotion
  (twice, identically, in the row plane) is gone.

## Unit three: a vertex's space (2026-11-05)

`VertexLayout` (`12db7ecf`, `72445c52`, `2d7d59dc`, `57957c90`, `326a4834`) owns what one vertex holds in
its leaf. It replaced the shape where a caller captured an `Option<LeafArena>` and then held a `LeafSlice`
that `Deref`-ed to the whole arena - a handle naming one vertex that could answer arena-wide questions.

| Entry | What it answers |
| --- | --- |
| `place`, `place_leaf_wide`, `grow_region`, `allocate`, `claim_placement` | placement inside the vertex's own space |
| `ensure_room_for`, `reserve_span` | growth that may move the leaf (the tree plane's entry) |
| `extent`, `regions`, `contents`, `parts_outside_extent`, `extent_contains` | the geometry questions the release paths ask |
| `rewrite_span`, `rewrite_span_with_policy` | re-laying the vertex's tiling |
| `compact_span`, `compact_span_with_moves` | packing its buckets together |
| `validate`, `containment_errors` | the arena's own consistency checks, scoped |

Three findings worth keeping:

- **The capture is lazy.** `compact_span` never reads the extent, so a view that required a capture would
  force callers to invent a leaf - and silently skip the operation for a vertex without one (fixtures build
  those). `has_leaf()` makes that an explicit question instead.
- **`bucket_index` was never an input.** Both compaction entries took one and began with `let _ = bucket_index;`.
  The operation compacts *all* of a vertex's buckets; the view does not carry the parameter.
- **`Deref` was load-bearing.** Removing it meant giving `LeafSlice` an explicit `extent` accessor and
  spelling `self.arena.graph` in its methods; the alternative was leaving one handle answering two
  granularities.
- **The rewrite's body stayed put.** `rewrite_vertex_edge_span_impl` remains in `compact.rs` (nine internal
  callers, its span-resolution policy beside it); the cut gave it a single external door rather than moving
  two hundred lines to buy nothing the door did not already buy.

Residual: the `leaf_arena` module stays `pub(crate)` because `labeled/bench.rs` reads
`labeled_leaf_vertex_edge_quota`, a pure geometry helper with no arena state.

## Units four and five: a tree's roots, and the view rule's own tests (2026-11-06)

`BucketTreePlane` ([ADR 0102](0102-one-view-owns-a-trees-roots.md)) is the fifth view and the first one this ADR's
rule forced into existence *before* a change rather than after a defect: six functions wrote a tree bucket's two
roots, each composing its own image, ordering its own publication and carrying its own rollback. Once
[ADR 0101](0101-tree-addresses-live-in-their-own-sides-region.md) split the roots across two stores, those six
would each have had to learn a second store and a second address space.

The view turned that into one entry. `BucketTreePlane::publish` writes the roots and the descriptor they belong
to; the caller states only the descriptor's *shape*. Two consequences are worth recording because they are what
the rule buys:

- **ADR 0101 landed inside one module.** The split changed `publish`'s body, `value_child`/`value_children`, and
  the geometry functions - no write path learned about the value store.
- **`grep` replaced "by convention".** Root publication is in `tree_plane.rs`; what remains in `tree_write.rs`
  are two rollback restores, and in `promote.rs` a test fixture.

### The view rule needed a test side too

The rule in AGENTS.md says a view tests each entry, not just that the graph works. Running it as an audit found
**every view had zero direct tests** except `span_size` and `leaf_arena`. That is not a coverage number: a view
owns a contract, and the graph's tests exercise the contract through callers that happen to agree.

Closing it produced two findings that the indirect tests could not:

- **`ensure_value_root` never released a store's first span.** It treated a recorded offset of 0 as "no value
  root", but 0 is also the first span's address. Whether a value root exists is a question the geometry answers.
- **A vertex's slice is bounded by the next vertex**, and a tiny bucket owns no leaf at all - both facts a
  leaf-bearing fixture has to honour, and both invisible when the fixture is written through the graph.

Every view now has direct tests: `span_size` (8), `leaf_arena` (11), `tree_plane` (6), `vertex_layout` (4),
`layout` (3), `row_plane` (3), `value_plane` (3).

### Remaining operation families

Not yet done, in the order the duplication measurement suggested:

| Family | Why it is a unit | Size |
| --- | --- | --- |
| Visit (row iteration) | 35 public operations and 44 storage-mode branches in `traverse.rs`; callers pick among single/batch/with-values/log-chain entry points | largest remaining |
| Bucket lookup | `(src, label) -> bucket` with the last-lookup cache, descriptor-row origin, create/destroy and slack | medium |
| Inline value schema | width declaration, 0-to-w migration, materialisation scheduling | medium |

Status as of 2026-11-07: **Visit is done and has a record of its own**
([investigation](../investigations/2026-11-06-visit-family-duplication.md)); bucket lookup and inline value schema
have not started.

What Visit ended with, measured rather than assumed:

- **Row iteration has one walker.** `visit_rows(owner, label, order, selection, visibility)` chooses the storage
  class once. Functions owning a `BucketMode` dispatch: **8 -> 4**, mode-branch sites 34 -> 13, and each of the four
  that remain owns its question - the row walker, the slab span funnel, the at-slot bulk read, and a selected-slot
  walk - rather than repeating another's operation.
- **A walk's answer is a type, not a parameter.** `graph/row_view.rs` holds `LiveRow<E, P>` (no hole exists in it)
  and `SlotRow<E, P>` (`SlotState::Hole`), and the entries that yield them are `visit_live_rows`,
  `visit_slot_rows` and their `_with_values` forms. A helper's signature names the question, and mixing the two is a
  type error.
- **Row visibility reached every storage class.** `lara::edge::RowVisibility { LiveRows, Positions }` is honoured by
  the slab scan, the tiny walk and the tree walk; three of the six defects this family produced were one class
  ignoring what another honoured.
- **A slot's value address is one question.** `BucketValueSource { None, Slab, Tree }` answers it, which took the
  at-slot with-values entry from eight mode branches to four and the batch cursors out of the dispatch count.
- **The walk-level guarantees are the same both ways**: a windowed tree walk stops at the break position, and a
  corrupt tree root fails closed with the same typed error as the whole-row walk.

The remaining work in Visit is cleanup, not duplication: the flat entries (`visit_edges`, `visit_edge_positions`,
`visit_edges_with_inline_property`) are adapters kept for their callers, and they go once those callers name a typed
entry.

### Where the unit stops, and why

**Vertex-level work does not belong here**, measured rather than assumed:
`compact_vertex_edge_span_with_moves` compacts *all* of a vertex's buckets and its callers pass a bucket
*index*, not a bucket; `rewrite_vertex_edge_span` re-lays the vertex's tiling. Folding either into a
bucket-scoped view would make it answer two granularities and force callers to invent a descriptor they do
not have.

The planned "hide the physical-layout choreography" cut was **dropped after checking its premise**: every
caller of `prepare_physical_layout` (14) is inside `compact.rs`, and the only production callers of
`rewrite_vertex_edge_span` outside it are two sites in `bucket.rs` with identical arguments. There is no
choreography leaking across a boundary, so a view would only rename it. A vertex-scoped view is available if
that changes; it is not warranted today (ADR 0100's criterion: duplication, not symmetry).

One latent defect surfaced while moving the cover recompute: it measured a neighbour with
`edge_start + stored_slots_raw`, which is a logical row count for a *tree* bucket, so a promoted neighbour
was measured as its row count. It now reads `bucket_content_region`, the SSOT for that geometry.

What stays is **API, not a path**: `insert_edge_skip_leaf_cascade`, `remove_edge_at_slot` and `read_edge`
remain as the graph's public surface, each one line onto the view. The migration's goal was one
*implementation*, not one *name*, and those three names are what the facade and other modules call.
