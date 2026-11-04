# 0098. The vertex slice owns its growth bound

Date: 2026-11-03
Status: Accepted (2026-11-03) - enforced at the two growth sites
Last revised: 2026-11-04
Anchor timestamp: 2026-11-03 00:10:16 UTC +0000

Related: [ADR 0088](0088-tree-csr-mode-for-high-degree-label-buckets.md) defines the leaf-layout
contract this ADR enforces; [ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md)
owns the spill runs that sit outside it.

## Context

ADR 0088 already states the ownership this ADR is about, and it is worth quoting because the whole
decision follows from it:

> - **E4** The slice is the vertex's exclusive space; `stored_slots` is a content width and never
>   defines it.
> - **E5** Placement happens inside the vertex's own slice; a slice that is too small grows the
>   extent or relocates the leaf, it never borrows a neighbour's.
>
> Ownership is split along the same line: `LeafArena` owns the extent (creation, growth,
> relocation, and it is the only caller of the free-span store), while `LeafSlice` owns one vertex's
> slice and can only report that a request does not fit.

The contract is not being met. A bucket that carries inline property values grows its slab prefix
past the start of the next vertex, and the rows of the two vertices then share physical slots.

Measured on the reproduction `facade::store::inline_update_fixture::undirected_valued_insert_sequence_keeps_neighbours`
(three vertices, valued undirected self-loops, 25 rows in the first vertex's batch):

```
low   = (edge_start 4096, stored_slots 11, degree 17, spill_run 33)
high  = (edge_start 4097, stored_slots  0, degree 34, spill_run 51)
guard = (edge_start 4098, stored_slots  1, degree  1)
```

Low's span is `[4096, 4107)`. It contains high's base (4097) and guard's (4098), so guard's single
row is written at physical 4098 - low's slot index 2. Reading low back returns that row with low's
value (`2002`, from low's ordinal-addressed values span) and guard's neighbour (`VertexId(2)`). The
corruption is stable across reads, so the row is stored that way, not mis-mapped on the way out.

## Problem

Two paths decide how far a bucket's slab may grow, and neither asks the slice:

- `bucket_successor_start_after_bucket` (`bucket.rs`) bounds a bucket by the next *bucket of the same
  vertex*, and for the last bucket returns `first_edge_start + vertex.stored_slots` - so the vertex's
  own content width is used as if it defined the space, which E4 forbids.
- `slab_window_exclusive_end` (`row_layout.rs`) derives the end from the next vertex's
  `base_slot_start`. That is the correct physical bound for the *write* path, but it is a second,
  independent derivation, and it is computed with `v_ord = 0` for synthetic bucket accesses, so it
  answers for the wrong vertex in exactly the case that matters.

The fold (`compact_label_bucket_overflow_to_slab`) uses the first bound; the write path uses the
second. When they disagree, one of them writes into a sibling. Nothing owns the answer.

## Existing Architecture Assessment

The concepts and the ownership already exist:

| Concept | Owns today |
| --- | --- |
| `LeafArena` | the leaf extent: creation, growth, relocation, free-span store |
| `LeafSlice` | one vertex's slice: `contents()` (what the buckets wrote), `regions()` (the space granted), `place()` / `place_leaf_wide()` |
| `SpillRunStore` | per-bucket spill runs, addressed `(run, index)`, power-of-two classes |
| `labeled_leaf_occupied_spans` | the leaf-mates' spans, already used to place a new vertex without overlapping |

`LeafSlice` is the owner E4 and E5 name. What is missing is not a concept but a *consumer*: the two
growth bounds above never consult it, and `LeafSlice::place` reports "does not fit" only to the
placement path that already asks.

The `stored_slots`-as-bound shortcut is the E4 violation in one sentence: a content width is being
read as a capacity. E5 is violated where the fold grows into a neighbour because the neighbour's
start was never part of its question.

## Alternatives

**A. Clamp the fold at the next vertex's base (minimum change, measured).** Cap
`bucket_successor_start_after_bucket` at `vertices[v_ord + 1].base_slot_start()`. This does remove
the overlap, but it cannot land on its own: `deferred::tests::compaction_cursor_packs_alternating_tombstones_across_steps`
builds nine vertices one slot apart and folds eight rows into `src`'s span, so with the cap its fold
has about one slot of room, errors, and the drain recurses into a stack overflow. A long-green test
therefore *relies* on the overlap, which means the suite has been passing with the defect latent
because nothing reads those rows back afterwards. Rejected as a standalone change; its diagnosis is
the input to the alternatives below.

**B. Make `LeafSlice` the single authority (chosen).** Add one question - *does `need` slots fit in
this vertex's slice?* - answered by the slice from the extent it belongs to, and route both consumers
to it: the write path's `have_space_on_slab` and the fold's room check, plus the bucket successor
that both feed. `stored_slots` stops being an input to the answer and becomes an output constrained
by it.

**C. A new `VertexArena` layer between `LeafArena` and `LeafSlice`.** This is the shape the problem
suggests if one starts from the solution: a small object that owns "the vertex's slice" and answers
capacity questions. It is rejected because that is `LeafSlice`'s stated ownership in ADR 0088, and a
second owner would leave two places that believe they own the same invariant - the failure mode this
ADR exists to remove. The complexity that makes `LeafArena` hard to read is not a reason to give its
per-vertex half a second name; it is a reason to route callers through the name it already has.

**B′. Use the derived slice (`LeafArena::slices`) as the bound (measured, insufficient).** This is what
`LeafSlice::fits` does today, and it cannot work on its own: `slices()` derives each slice from the
vertex's **first-region starts**, so the bound is a function of the placement it is supposed to
constrain. Logging every width change on the reproduction shows a run-backed bucket at
`edge_start = 4096` growing `stored_slots` 1 -> 2 -> ... -> 12 while its degree grows in step, and
`fits` allows all of it because the slice partition is derived from the vertices that *have* a
first region: a sibling whose rows all live in its run contributes no boundary at all, so the
previous vertex's slice stretches to the next sibling that does have one. The reproduction states it
plainly - `region of VertexId(0) 4096..4107 outside its slice 4096..4098`, where 4098 is the third
vertex's start, not the second's. The check is circular, not wrong.

**D. Persist a slice width on the vertex record.** Explicit data would make the bound checkable from
anywhere, but it adds a field that must be kept equal to what the arena actually granted - a
duplicated source of truth updated at every growth point. Rejected while the extent already answers
the question; revisit only if a caller genuinely cannot reach the slice.

## Decision

`LeafSlice` is the single owner of the vertex slice's growth bound, and every path that decides
whether a slab row fits asks it:

1. `LeafSlice` exposes one predicate and one query - "does `need` slots fit" and "how many slots
   remain" - derived from the extent it belongs to, never from `stored_slots`.
2. `bucket_successor_start_after_bucket` stops treating `first_edge_start + stored_slots` as the
   vertex's space; the vertex's contribution to a bucket's successor is the slice's end.
3. `slab_window_exclusive_end` keeps the synthetic-access handling it needs for reads, but the answer
   it must agree with is the slice's, and a disagreement is a bug in one of the two rather than a
   tolerated difference.
4. The fold takes its room from the same predicate. A row that does not fit stays in its run - the
   fail-closed behaviour the spill design already provides - rather than borrowing a neighbour's
   slot.

`stored_slots` remains the content width (E4). Its update is paired with the placement decision in
one publish, so no reader observes a prefix longer than the slice.

The bound must come from something **independent of region placement**, because the derived slice is
not: either the next vertex's `base_slot_start` (which the arena assigns and which no region growth
moves), or a recorded allocation. `LeafSlice::fits` stays the API the callers use, but its answer has
to be sourced from that independent bound rather than from `slices()` alone.

**What the bound is, and why it is a flag rather than an array.** Deriving each vertex's allocation
from its buckets is correct - it is what `validate` does - but it is not affordable where placement
needs it: doing so on the insert path fails `g6_tiny_insert_reads_no_edge_bytes`, which exists to keep
that path out of edge bytes. Recording a per-vertex `(start, width)` array in the leaf's metadata
would work, but the fast path does not need the widths - it needs to know whether the fixed-quota
layout is still intact. That is exactly the condition `labeled_leaf_has_oversized_vertex` approximates
today by reading each vertex's record width, which is the bypass width for a labeled vertex and
therefore the wrong number (measured 1 against 11).

So the leaf records the fact the tiling pass already knows: **whether its block has been tiled**. The
rewrite and relocate that tile the whole leaf are its only writers, so there is one writer, not the 78
that write bucket widths. A tiled leaf skips the fixed-quota fast path and takes the placement that
reads the real spans; an untiled leaf is by construction in the layout the fast path assumes.

The flag does not fit as cheaply as it first looked: `physical_start` is read or written at **46
sites** outside `span_meta.rs` (17 in `compact.rs`, 12 in `row_layout.rs`, 8 in `leaf_arena.rs`, 3 in
`tree_write.rs`, plus `bench.rs` and `init.rs`), and several treat it as a plain slot index, so
stealing a high bit would put a value they do arithmetic on out of range. The cheaper shape is
therefore to give `SegmentSpanMeta` its own field and grow `ENTRY_SIZE` from 8 to 16 bytes: there are
only 16 constructions of the struct (13 of them inside `span_meta.rs`), and the row count is one per
leaf segment. Either way it is a persisted-layout change that needs fresh state, which the
pre-production rule allows - but the 46 read sites are the reason it is a unit of its own rather than
a footnote to the placement fix.

### Spill's place in this

Spill rows are not in the slice and this ADR does not move them. `SpillRunStore` owns its own memory
and its own class-based allocation; a run row is addressed `(run, index)` through the bucket's
`spill_run`, so no vertex slice contains it and no vertex's growth can collide with it.

What the slice bound changes is only the *decision* and the *pairing*:

- **Decision.** "Slab or run" becomes the slice's answer. If the row fits, it is a slab row and
  `stored_slots` grows; if it does not, it is a run row and only `spill_used_len` grows. That branch
  already exists in `try_grow_packed_slab_by_one`; the ADR makes its condition authoritative instead
  of derived.
- **Pairing.** `stored_slots + spill_used_len` is the bucket's resident row count, and the reader
  splits at `stored_slots` (`i < stored_slots` is slab slot `edge_start + i`, otherwise run row
  `i - stored_slots`). Because the split is the only coupling, the two counters must be published
  together: a stale `stored_slots` after a shrink makes the reader take run rows from the slab, and a
  stale value after growth makes it read uninitialised slots.

## Consequences

- E4 and E5 become enforceable rather than aspirational: `stored_slots` can no longer exceed the
  slice, because nothing uses it as capacity.
- The write path and the fold can no longer disagree, because they ask the same object.
- A vertex whose rows do not fit spills, which is a defined, observable state rather than silent
  corruption, and the existing fold brings them back when the leaf has room.
- The `v_ord = 0` hazard for synthetic accesses becomes a narrower question: the slice knows its
  vertex, so the derived window no longer has to guess from an ordinal that belongs to a different
  vertex.

## Trade-offs

- Callers of `bucket_successor_start*` need the vertex identity. The signature grows by one argument
  at roughly eight call sites; the alternative is each caller deriving the bound itself, which is the defect.
- Fixtures that place several vertices one slot apart and then fold many rows into one of them are
  now wrong by contract, not merely fragile. The stack-overflowing test above is one; the migration
  below treats the fixture population as part of the work rather than as fallout.
- A bucket that used to grow silently now spills, so some shapes move rows between the slab and a run
  across maintenance passes. That is visible in placement statistics and in the benches that read
  them.

## Migration

No persisted layout changes: the slice bound is derived from the extent the arena already owns, so no
fresh state is required and no decoder changes.

The work is in the fixtures and in the two bound sites:

1. Route `bucket_successor_start_after_bucket` and the fold's room check through `LeafSlice`.
2. Fix the fixtures that rely on overlap, starting with
   `compaction_cursor_packs_alternating_tombstones_across_steps`, by giving the vertex under test a
   slice wide enough for its span before folding into it.
3. Re-enable the reproduction's non-overlap assertion and confirm the 25-row case places the three
   spans disjointly.

## Outcome

The defect was closed by bounding the two paths that grow a slab prefix, without changing the slice
partition:

- **The fold.** `fold_bucket_edge_spill_head_into_span` took its room from the bucket's *region* end,
  and the region is derived from the regions that exist - so a leaf-mate whose rows all lived in its
  run contributed no region and the region stretched over its edge start. The fold now stops at
  `next_leaf_vertex_edge_start`, measured and named by `#[track_caller]` on `with_stored_slots`.
- **The rewrite.** A widened span that would run past the next leaf-mate keeps the span the vertex
  already holds, which is what the `SlackMayBeDropped` callers accept.
- **The fast path.** It now requires the leaf's recorded `fixed_quota_intact` flag instead of
  inferring the layout from a vertex record width that is the bypass width (measured 1 against 11).

Not needed, and not done: making `slices()` include every vertex, and the eleven-fixture migration
that change required. The narrow bound removed the corruption without changing which layouts the
partition considers valid, so `LeafSlice::fits` stays derived and keeps its current callers
(`validate`, `grow_region`).

Result: lara 707/0, graph 1106/0, including the reproduction
(`inline_update_fixture::undirected_valued_insert_sequence_keeps_neighbours`, which asserts the
arena's layout contract) and the original fixture.

## Design Documentation Impact

- [ADR 0088](0088-tree-csr-mode-for-high-degree-label-buckets.md): record that E4/E5 now have an
  enforcement point and name the two paths that bypassed it.
- [ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md): record that the slab/run
  decision is taken from the vertex slice and that `stored_slots` and `spill_used_len` are published
  together.
- `design/implementation-gaps.md`: close `GAP-GRAPH-SIX-RED-FACADE-STORE-TESTS` once the reproduction
  is green, and keep the failed clamp experiment as the reason a bound-only change is not sufficient.

## Amendment 2026-11-04: a slice is an owned extent, not a derived gap

**Status: decided, not implemented.** The free-span handling lands with the implementation.

E4 says the slice is the vertex's exclusive space. It is not: `LeafArena::slices` derives each slice as
`[the vertex's first bucket base, the next vertex's first base)` (the last vertex running to the leaf
extent end), so a slice is the **gap between neighbours**. `LeafSlice::place` and `grow_region` carve
allocations out of that gap without calling the free-span store, so space that is still on the free list
is handed out once to this bucket and again to whoever takes that free span.

Measured: promotion of a **valueless** bucket at ~2 049 rows panics in
`promote_bucket_to_tree_mode` because releasing the old prefix's tail is refused with

```
OverlapPrevious { previous: FreeSpan { start_slot: 4103, len: 1017 },
                  inserted: FreeSpan { start_slot: 4104, len: 1018 } }
```

- **Decision:** a slice becomes an **owned extent** allocated from the free-span store when the vertex is
  placed. `place` and `grow_region` then carve inside space the vertex already owns, and a released tail
  can no longer overlap a free span the store still holds.
- **Why owned rather than "take on demand":** `place` is a pure, infallible function today (it answers
  "does it fit" from resident spans). Making it consume the free store would make every placement
  fallible and would still leave two questions to answer at each call site - what happens when the take
  fails, and who releases on rollback. An owned extent answers both once, when the vertex is placed.
- **Free-span view, in the same unit:** every take and release goes through one owner (`LeafArena`, which
  ADR 0088 already designates as the only caller of the store) rather than through `EdgeStore` helpers that
  each decide their own policy. The current failures come from a rule being restated at three layers.
- **Acceptance:** value-less parallel edges promote past 2 048 rows; the 10 000-row
  `expand_skewed_noise_10k_a_500b` fixture builds; `canbench --persist` completes.
- **The promotion is the victim, not the culprit.** Reading the addresses back: the failing release is
  `[4104, 5122)` and the free span it overlaps is `[4103, 5120)`, so the promoted bucket's prefix is
  roughly `[3086, 5122)` - about 2 036 slots, i.e. the 2 048 rows - and the free span sits **inside** it.
  The prefix therefore grew *over* a free span during the inserts (the growth path
  `rebalance_vertex_edge_span` → `rewrite_vertex_edge_span` → `prepare_physical_layout`), long before the
  promotion ran. Promotion only trips over it because it is the first caller that releases part of that
  space. So the take belongs where the span is **rewritten/placed**, and the promotion needs no change.
- **And the rewrite allocates nothing:** `PhysicalLayoutOperation::VertexRewrite` prepares with
  `PhysicalSpanSelection::NoAllocation` plus the retired spans, i.e. it only retires - the new space is
  expected to be already owned by the vertex. So the space a bucket grows into always comes from the leaf
  arena's per-vertex placement (`LeafSlice::place` / `grow_region`), which is the one place a take is
  missing. That closes the search: the fix is the "declare the cover, take it, then place inside it"
  order at `LeafArena`/`LeafSlice`, exactly as this amendment says, and nothing in the rewrite or the
  promotion changes.
- **Second naive variant measured: claiming inside `place` breaks 22 tests.** Making `LeafSlice::place`
  take the range it hands out (`take_prefix_at(cursor, need)`, skipping candidates the store cannot give)
  fails 22 tests, because some placements deliberately land inside the vertex's own **tile slack**, which
  is not a free span at all. So neither end of the pair can be fixed alone: the cover must be taken
  *once* when it is declared, and `place` then carves inside space the vertex already owns.
- **The naive swap was measured and does not work:** replacing the derived end with the vertex's tile
  cover in `LeafArena::slices` (`first_base + tile_width_with(first_base, 0)`, clamped to the extent)
  breaks **24 tests**. The reason is the direction of the dependency: today `place` may put a region
  anywhere in the gap and the cover is then *recomputed* from the regions it produced, so making the
  cover the authority requires the cover to be **grown first** (a store take) and `place` to carve only
  inside it - i.e. the order "declare, take, place" instead of "place, derive". Doing it in one line
  inverts the order without providing the take, so the smaller cover simply stops admitting placements
  the layout depends on.
