# 2026-11-06: the Visit family's duplication, measured before it is cut

ADR 0100 lists Visit (row iteration) as the largest remaining operation family: "35 public operations and 44
storage-mode branches in `traverse.rs`". This is the measurement behind a cut, and the one wrong cut it already
ruled out.

## What the numbers are now

| Measure | Count |
| --- | --- |
| Entry points (`pub`/`pub(crate) fn` in `traverse.rs`) | 55 |
| `visit_*` entry points | 20 |
| `BucketMode::` branch sites | 34 |
| Functions that own a mode dispatch | 8 |

The dispatch is not spread evenly:

| Function | Mode branches |
| --- | --- |
| `visit_edges_at_with_inline_property` | 8 |
| `visit_slab_bucket_span_edges` | 4 |
| `visit_edges_window` | 4 |
| `visit_edges` | 4 |
| `visit_edges_for_label_impl` | 4 |
| `visit_out_inline_property_batches_for_label_next` | 3 |
| `visit_out_edge_inline_property_batches_for_label_next` | 3 |
| `visit_edges_with_inline_property_impl` | 3 |

Two of them are already funnels: `visit_edge_slots` and the slot-only readers delegate to `visit_edges` /
`visit_edges_with_inline_property` rather than reading storage themselves, so the valueless family has **two**
dispatchers, not twenty.

## The cut that looked obvious and is wrong

`visit_edges` and `visit_edges_window` both dispatch tiny / slab / tree, and their tiny and slab arms are
identical. The natural consolidation is "`visit_edges` is `visit_edges_window` with an unbounded window" -
one dispatcher instead of two.

Row iteration has four contracts that the two differ on, and the tree arm is where they show:

1. **Corruption propagation.** The windowed tree walk validates the blocks it skips *inside* the window
   (`property_tree`, `tree_visit_break_stops_topology_reads`, `point_state_tree_native_boundaries_and_errors`).
   The unbounded walk does not have a window to validate against.
2. **Prefetch and Break ordering.** Both stop the callback on `Break`, but the windowed walk has already
   decided which leaf blocks to open; the unbounded walk opens them one at a time.
3. **Tombstone positions.** Both yield tombstone-inclusive positions, but the windowed walk's "skipped positions
   are entirely inside the window" rule changes which ones it visits at the edges.
4. **The window itself.** `window.is_empty()` is a cheap early answer the unbounded walk does not need.

Measured, not assumed: making `visit_edges` delegate to the windowed walk with `TraversalWindow::unbounded()`
compiles, and then three tests fail - `tombstoned_tree_bucket_visit_preserves_logical_positions`,
`tree_visit_break_stops_topology_reads`, and
`point_state_tree_native_boundaries_and_errors_preserve_all_memories`. The two entries are two operations
because their guarantees differ; a view that merged them would have to weaken one.

That is the ADR 0100 criterion applied honestly: duplication is *one operation written twice*, not *two
operations that look alike*. These look alike and are not.

## Where the duplication that remains actually is

The **with-values** family, because it repeats the whole dispatch instead of delegating:

- `visit_edges_at_with_inline_property` (8 branches) implements slot-selected iteration with values. Its tiny
  arm reuses the valueless selected topology (width 0), its tree arm reads values per selected slot, and its
  slab arm has to build the selected-slot value batch. None of that is shared with
  `visit_edges_with_inline_property_impl` (3 branches), which walks a whole row.
- `visit_edges_from_slot_with_inline_property` is a third entry over the same question.

The cut, when it is made, is a view whose one entry takes the *request* (slot selection **or** window, values on
or off, order) and owns the mode dispatch once; the current entries become adapters. That is a real change of
shape, not a delegation, which is why it wants its own session with the four contracts above written down as
the tests that must keep passing.

## What this record changes

- It removes the tempting wrong cut, with evidence.
- It leaves the number to beat: **8 functions owning a mode dispatch; 3 of them belong to the with-values
  family.** A cut that does not reduce that count has not cut the duplication.

## Addendum: the contract that could unify them, and the two places the code departs from it

The windowed and unbounded tree walks *look* like one operation with different ranges, and there is a contract
that would make them one:

> **Visit the tombstone-inclusive positions in a slot range `[lo, hi)`, in the requested order. Resolve a block
> when you reach it, validate its header when you scan it, yield the positions inside the range, and stop
> touching storage the moment the callback breaks.**

Unbounded is `[0, stored)`. Under that contract both entries are one walker with one dispatcher. Two departures
stop it today, and both are in the **windowed** walk:

1. **A corrupt root entry trips an assertion instead of failing closed.** The walker reads a block header
   straight from the store, whose contract is "never read unbacked ids" - so a root entry naming a block past
   the tail panics in debug and reads garbage in release, where the unbounded walk returns
   `LtbBlock(NotMinted { id })`. **Fixed** (`traverse.rs`, both order arms now check the id against
   `ltb().tail_next()` and return the typed error).
2. **A break does not stop the block it lands in.** The windowed walk hands `scan_tree_block_range_desc` a
   closure that records the break value but returns unit, so the scan finishes the whole block and only then
   stops. The unbounded walk stops at the position. That is a real difference in the guarantee "no read after
   Break", and it is why the unbounded entry cannot simply delegate: the poison in
   `tree_visit_break_stops_topology_reads` sits in the next block's root entry, which the windowed walk reaches
   while finishing the current block.

The remaining work is therefore item 2: let `scan_tree_block_range_desc` / `_asc` carry a `ControlFlow` out of
the callback so the scan stops at the position. With that, the contract above holds for both, and `visit_edges`
becomes `visit_edges_window` with an unbounded window - one walker, and one entry fewer.

The measurement to beat is unchanged: **8 functions owning a mode dispatch; 3 belong to the with-values family.**

### The third departure, and why it is the one that stays

Item 2 is now done: `scan_tree_block_range` / `_desc` carry a `ControlFlow` out of the callback, so the windowed
walk stops at the break position instead of finishing the block it landed in. That is a strict improvement -
"no read after Break" now holds at the position in both entries - and `visit_edges` still cannot delegate,
because a third difference remains and it is **semantic**:

| Entry | Rows it reports |
| --- | --- |
| `visit_edges` (whole row) | tombstone-inclusive **positions**: a deleted slot is reported as the sentinel edge it holds (ADR 0088 §2) |
| `visit_edges_window` (a window) | live rows only, at their positions |

Both are asserted, by name:

- `tree_visit_break_stops_topology_reads` removes the row at the break slot and expects the walk to report it
  as `EDGE_TOMBSTONE_SENTINEL`; the break lands on a tombstone, so a walk that skipped tombstones would not
  break there at all.
- `visit_edges_window_tree_matches_slab_tombstone_positions` expects a window over the same bucket to report
  `[(1, 101), (3, 103)]` - the hole at 2 absent - and that is what the slab window reports too, which is the
  point of the test: a window names the same live rows whichever storage class holds them.

So the difference is **what a window means**, not how it is implemented: pagination wants live rows, topology
wants positions. Making them one entry would have to change one of those contracts - and the slab window's, to
keep the storage classes interchangeable.

That is the answer to "is there a simpler contract to unify under": there is for the *walk* (a slot range,
lazy resolution, validate on scan, stop at break - all now true for both), and there is not for the *reporting*.
The two entries are one walker with two row-visibility contracts, and the duplication that remains is the
with-values family, which repeats the dispatch rather than sharing this one.

## Resolution: the difference is row visibility, and it is now a type

The tombstone question was not a contract between a window and a whole row - it was **inconsistent between
storage classes under the same entry**. `visit_edges` on a tree bucket reported the holes at their positions; on
a slab bucket the row scan hid them. Nothing in either name or signature said which you would get, and three
tests happened to encode the tree answer while every slab reader relied on the other.

Two callers want two answers for a real reason:

| Purpose | Needs | Examples |
| --- | --- | --- |
| Reading | live rows only: a tombstone is not a row | pagination, scans, a caller that wants edges |
| Mutation / topology | every position, tombstones included | removal, compaction, tombstone reuse, a rewrite that has to keep positions |

So the choice is now a value, `lara::edge::RowVisibility { LiveRows, Positions }`, threaded through the row scan
(where the tombstone is still skipped at the source for `LiveRows`), and the graph exposes the two purposes as
two entries:

| Entry | Contract |
| --- | --- |
| `visit_edges` | whole row, **live rows only** (the read shape) |
| `visit_edge_positions` | whole row, **every position including tombstones** (the mutation shape) |
| `visit_edges_with_visibility` | the one walker both call, for a caller that has the choice as a value |

`visit_edges` is uniform now: a slab bucket and a tree bucket answer the same rows under it. The three tests that
encoded the tree answer - `tree_visit_break_stops_topology_reads`,
`tombstoned_tree_bucket_visit_preserves_logical_positions`, and the native-visitor half of
`point_state_tree_native_boundaries_and_errors_preserve_all_memories` - now ask for `visit_edge_positions`,
which is what they were actually testing: that mutations see the holes.

The windowed entry keeps its own contract deliberately: a window is a page over live rows, and
`visit_edges_window_tree_matches_slab_tombstone_positions` asserts that both storage classes agree on it. The
unification that a single walker would buy is therefore not blocked by the walk any more - it is blocked by
`visit_edge_positions` having no windowed form yet. That is the next slice: the tree window walker yields
tombstones when asked, `visit_rows(owner, label, order, selection, visibility)` becomes the only dispatcher, and
`visit_edges` / `visit_edge_positions` / `visit_edges_window` become adapters over it.

## Done: one dispatcher, four shapes

`visit_rows(owner, label, order, selection, visibility)` is now the only walker that chooses a storage class for
whole-row and windowed iteration. The three public entries are adapters over it:

| Entry | Selection | Visibility |
| --- | --- | --- |
| `visit_edges` | `WholeRow` | `LiveRows` (read) |
| `visit_edge_positions` | `WholeRow` | `Positions` (mutation) |
| `visit_edges_window` | `Window(..)` | `LiveRows` (page) |

Measured after the change: the functions owning a `BucketMode` dispatch went from **8 to 7**, and the two that
merged (`visit_edges`, `visit_edges_window`) were the pair this record started from. Three properties made the
merge safe rather than hopeful:

1. The block scan carries a `ControlFlow` out, so a break stops at the position (elsewhere in this record).
2. A corrupt root fails closed as the same typed error in both (elsewhere in this record).
3. Row visibility is a parameter, and the tree scanners report a hole when asked - which is what let the whole-row
   `Positions` walk keep its answer while the windowed `LiveRows` walk kept its own.

What is left of the family's duplication is the with-values paths: `visit_edges_at_with_inline_property` (8 mode
branches), `visit_edges_with_inline_property_impl` (3), and the two batch cursors (3 each). They are the same cut
one level up - a request that says selection, visibility and whether values are attached - and the count to beat
is 7.

## The with-values family, first slice: the mode match was a value-source question

`visit_edges_at_with_inline_property` carried eight mode branches. Reading them apart, most were answering one
question: *where does this slot's value live?* Tiny has none (its width bytes are target payload), slab reads its
inline property bytes span or run, tree reads its property tree. Every with-values entry spelled that out again,
which is why one of them grew to eight branches and another to three.

`BucketValueSource` (`value_source.rs`) now answers it once:

| Resolution | Meaning |
| --- | --- |
| `None` | no values at this width - tiny, or any class at width 0 |
| `Slab { width }` | the inline property bytes store, by slot |
| `Tree { width }` | the property tree, by slot |

`read_slot_into` is the one reader: it names the class-specific path once. The at-variant's mode match is gone -
its tree arm reads through the source, and the class decision now happens in one place.

Measured: the at-variant went from **8 mode branches to 4** (the slab batch path keeps its own because it reads a
whole value sequence in one call, which is what makes a hot scan cheap; collapsing it into per-slot reads would
trade a correctness-neutral refactor for a regression). Functions owning a dispatch: 7 -> 7, but the one that
carried the most now carries half as many, and the *answer* it kept was moved to the owner.

Next: the same treatment for `visit_edges_with_inline_property_impl` (3 branches) and the two batch cursors (3
each), then the slab batch path becomes the only thing an entry still says about its class.

## The whole-row with-values entry cannot be cut yet, and the tests say why

The next shape after the at-variant is `visit_edges_with_inline_property_impl` (three mode branches). Its
dispatch is the same value-source question, so replacing the mode match with `BucketValueSource` looks mechanical.
It is not, because this entry's **row visibility is currently class-dependent**, and the tests encode that:

| Bucket | Rows this entry reports today | Pinned by |
| --- | --- | --- |
| tree | positions, holes included | `promotion_retained_eight_holes`, `promotion_retained_sixteen_holes`, the `public_bypass_capacity_*` group |
| slab | live rows only | `tiny_parallel_counterparts_keep_slots_after_holes`, `delete_purge_keeps_neighbor_survivors_order_preserving`, the compaction group |

Both experiments were run. Forcing `Positions` makes the slab-shaped tests fail; forcing `LiveRows` makes the
tree-shaped ones fail. That is the same inconsistency `visit_edges` had before `RowVisibility` existed, one entry
over - and it is why the mechanical replacement cannot land on its own.

The cut, when it is made, is:

1. give this entry the same visibility split the valueless one has (a read entry, and a positions entry that
   attaches values);
2. move the tree-shaped tests to the positions entry and the slab-shaped ones stay on the read entry;
3. then the mode match becomes a `BucketValueSource` resolution, as it did in the at-variant.

Two facts from this attempt are worth keeping: the width-0 path of the entry reports positions for a tree bucket
(the width bytes of a tiny bucket are payload, so width 0 is not "no values" for every class), and the slab dense
batch path must stay - it reads a whole value sequence in one call, and collapsing it would be a regression with
no correctness gain.

Measured count to beat, unchanged: **7 functions owning a storage-class dispatch.**

## The whole-row with-values entry now has both shapes, and the legacy name kept its answer

Step 1 of the three above is in. `visit_edges_with_inline_property_impl` takes a `RowVisibility` and applies it
**once**, at the callback boundary, so an arm that reports holes (a tree walk) and one that never does (a slab
dense read) answer the same question the same way without touching either arm.

Two entries front it:

| Entry | Contract |
| --- | --- |
| `visit_edges_with_inline_property` | unchanged: every position its storage class can report (a tree reports holes, a slab cannot) |
| `visit_live_edges_with_inline_property` | **new**: live rows only, uniform across storage classes |

The first keeps its historical answer rather than adopting one of the two: it has ten callers on both sides of the
question, the tests on both sides pin today's behaviour, and renaming the answer under them would be a semantic
change dressed as a refactor. The new entry is the read shape the deferred skip-then-visit path now uses, so the
uniform answer exists and has a caller while the legacy name waits for its callers to be sorted one by one.

That leaves step 3 - replacing the mode match with a `BucketValueSource` resolution - still open for this entry,
because the arms are now separated by more than the value source. The order that works is: move a caller to the
entry that names its shape, then let the dead arm go.

## The batch cursors dispatched on the wrong thing

Both batch cursors matched on `BucketMode` to answer one question: does this bucket have values at all? Tiny has
none (its width bytes are target payload), and a bucket at width 0 has none either - which is exactly
`BucketValueSource::None`. They now match on the source, so the class decision lives in `value_source.rs` and the
cursors only branch on "values / no values".

That took the two cursors out of the dispatch count entirely: they no longer name a storage class.

| Measure | Start of this record | Now |
| --- | --- | --- |
| Visit entry points | 20 | 21 (two named shapes added) |
| Functions owning a `BucketMode` dispatch | 8 | **6** |
| Mode-branch sites in `traverse.rs` | 34 | 26 |

What is left, and why each is not the same cut:

- `visit_rows` (4) - the single row walker; this one *should* dispatch, it is the owner.
- `visit_slab_bucket_span_edges` (4) - the slow span funnel, which is slab-specific by definition; its branches
  are about span shapes, not storage classes choosing themselves.
- `visit_edges_at_with_inline_property` (4) - the at-slot value attach; the remaining branches are the slab batch
  path whose bulk read is the point.
- `visit_edges_for_label_impl` (4) and `visit_edges_with_inline_property_impl` (3) - the legacy value-attaching
  funnels, waiting on their callers being moved to the entry that names their shape.
- `visit_bucket_edge_slots` (1) - a selected-slot walk with one slab/tree difference.

## A defect found by reading the funnel: the tree arm yielded no values

`visit_edges_for_label` is the legacy callback-`E` API. Its slab arms attach the row's inline property bytes to the
edge they yield (`with_stored_inline_property_bytes`); its tree arm yielded the bare edge. So a caller reading
`edge.edge_inline_property_bytes()` got that row's values from a slab bucket and an empty slice from a tree bucket -
the same class-dependent answer this record keeps finding, this time in **data**, not in row visibility.

The arm now reads through `BucketValueSource`, as the slab arms already did, so the two classes answer with the
same bytes. No test caught this one; it was found by reading the arms side by side while looking for the cut. The
fixtures that build a tree bucket with real property values live in `tree_write`'s test module, so a parity test
for this funnel would have to cross module boundaries - the honest statement is that the arms are now uniform by
construction (both go through the one reader) and that no test pins the parity itself.

This is the shape of the remaining work in this family: the duplicate is not always a duplicated *walk*, sometimes
it is an arm that quietly answers a different question than its siblings, and the way to find those is to read the
arms against each other rather than to trust that the tests cover them.

## The fixture gap is a lookup gap, measured

Two tests were wanted: the tree half of the three row shapes, and a parity test for the funnel's value attachment
(the fix that made the tree arm read through `BucketValueSource`). A `seeded_tree_bucket` fixture was written - it
mints edge and property leaves, fills their payloads from a per-slot closure, and installs the descriptor through
`install_tree_bucket`.

It could not reach the walk. `install_tree_bucket` writes the **bucket descriptor** at a slot; a graph-level entry
(`visit_edge_positions`, `visit_edges_for_label`) first resolves the label through the **vertex's descriptor row**,
and installing a bucket at slot 0 does not update that row. The probe is unambiguous: slot 0 holds a tree
descriptor (`buckets().read_label_bucket_slot(0).is_some()` is true, and it reports `is_tree_mode()`), while
`find_bucket(vid, vertex, label)` answers `Missing` for the same vertex and label.

So the fixture gap and the "no parity test" note above are the same gap: a tree bucket reachable through the graph
needs its vertex row installed as well as its descriptor. That is what `install_tree_bucket` should do next - take
the vertex and label and write the row entry too - and both tests follow from it.

The attempt is reverted rather than left half-working: a fixture that cannot reach the code it is meant to exercise
is not progress, and the record above is worth more than a dead helper.

## A reachable tree fixture, and the three things it took

The fixture gap is closed enough to be useful: `install_reachable_tree_bucket` writes the vertex's bucket row as
well as the descriptor, and `seeded_tree_bucket_on_vertex` mints edge and property leaves with payloads from
per-slot closures. The trees' layout stays `install_tree_bucket`'s business.

Three things were needed, and each was a real difference between "a descriptor exists" and "the graph can see it":

1. **The vertex's bucket row.** A descriptor alone is invisible: every graph-level entry resolves the label through
   the vertex's row first, so a fixture must create the row (`try_with_bucket_row(0, 1)`) when the label has none.
2. **The lookup cache.** The segment keeps a last-bucket cache; a descriptor written without
   `invalidate_bucket_lookup_caches_for_bucket_segment` is not seen by the next lookup.
3. **The leaf header's tombstone count.** A tree walk verifies its scan against the block header, so a fixture that
   writes a tombstone into a payload must write the count too - otherwise the walk fails closed with
   `CountMismatch`, which is the check working, not the fixture.

With those, `the_shapes_hold_for_a_seeded_tree_bucket` pins the three shapes on the one class that can report a
hole: the mutation shape names it at position 3, the read shape hides it, and a page over it reports only live rows.

## Closed: the funnel parity test

The parity test - that the legacy label funnel attaches the same values to a tree row as to a slab row - passes.
The earlier failure was not the fixture's reachability, which the probe settled in one line: the seeded bucket
reported `w=4 stored=8 tree=true` and the lookup returned exactly that bucket. It was the **edge type**: `TestEdge`
does not store inline property bytes at all, because the trait's `with_stored_inline_property_bytes` default returns
`self`, so the tree side of the comparison was reading a type that had nowhere to put the value. Both sides now use
`InlinePropertyTestEdge`.

That is the same lesson as the funnel defect itself, one layer down: a row's values are only a row's values if the
edge type can hold them, and the default trait method is a silent no-op that makes the bug look like a data
problem. Two facts worth keeping from the attempt: `TestEdge` cannot express a value-bearing row, and a fixture's
edge-type choice decides whether a value goes anywhere.

## The legacy funnel is a read, so it has one shape now

`visit_edges_for_label` is the callback-`E` API the deferred iterators and the out-edge iterators use - all reads.
Its mode match is gone: the value source answers where a value lives, `visit_rows` answers the topology when there
are no values, and the tree arm filters the holes the tree walk reports (this funnel's callers read rows, not
positions). The dense slab read stays, because reading a whole value span in one call is the reason that path
exists.

| Measure | Start of this record | Now |
| --- | --- | --- |
| Functions owning a `BucketMode` dispatch | 8 | **5** |
| Mode-branch sites in `traverse.rs` | 34 | 22 |

What is left, and why it is not this cut:

- `visit_rows` (4) - the owner; it should dispatch.
- `visit_slab_bucket_span_edges` (4) - the slow span funnel, slab-specific by definition; its branches are span
  shapes.
- `visit_edges_at_with_inline_property` (4) - the at-slot value attach; the remaining branches are the slab batch
  path.
- `visit_edges_with_inline_property_impl` (3) - the last legacy funnel. Its row visibility is still class-dependent
  (a tree reports holes, a slab cannot) and its ten callers sit on both sides, so it needs the same two-entry split
  the valueless one got before its match can go.
- `visit_bucket_edge_slots` (1) - a selected-slot walk with one slab/tree difference.

## The last funnel: which answer becomes the default is the whole question

`visit_edges_with_inline_property_impl` is the one entry left with a mode match. Two attempts to remove it failed,
and both failed on the same point rather than on the dispatch: **which row visibility the entry's name should
answer**.

Attempt one kept the legacy answer (positions) and replaced the match with a value-source structure. Sixteen tests
failed - not because the dispatch was wrong, but because a slab bucket can now report the holes it holds, and
callers that had been reading through this entry saw rows they never saw before.

Attempt two moved the read callers to `visit_live_edges_with_inline_property` first (four in `deferred`, one in
`counterpart`), which is correct and kept the suite green, and then unified the impl. It failed the same way from
the other side: the mutation callers that remain - promotion and compaction - do want positions, and their tests
assert them, so making the width-0 path obey the caller's visibility changes what the *slab* arms report.

So the work is not the dispatch. It is:

1. Decide the entry's default answer and say it in its name (the valueless family did this: `visit_edges` reads,
   `visit_edge_positions` is the mutation shape).
2. Move promotion and compaction to a positions entry, and their tests with them.
3. Then the match becomes a value-source resolution, as it did in the at-variant and the other funnel.

The read-side migration from attempt two is worth redoing as step 1 of that sequence; it is mechanical and it
passed. The rest waits on the naming decision, which is a contract question rather than a code-shape one.

## The decision, made: the with-values entry's default is live rows

Evidence from the code, not taste:

- **Promotion needs live rows.** Its callback fills the gaps itself (`while next < slot { emit(tombstone) }`),
  emits the visited row, and then asserts `live == bucket.degree`. A tombstone row in the callback would
  over-count `live` and transcribe a sentinel as data.
- **Every production caller is a read**: promotion above, the deferred iterators, the reversed counterparts, and
  this module's own read helpers.
- **The calls that expect holes sit in tests**, and their rows are width 0 - so they need *positions*, not values.

So `visit_edges_with_inline_property` answers live rows, uniform across storage classes, and a positions entry
exists for the other question. That is the same shape the valueless family already has, which is the point: one
family, one naming rule.

## What the migration actually costs, and why it is not a batch rename

The one-line change of default plus a mechanical move of the hole-expecting call sites was attempted and produced
six failures of a specific kind: a test that reads the same promoted bucket **twice** asserts different visibility
at the two sites - holes at one, live rows at the other - so a per-file substitution moves a call site that was
already right. `promotion_retained_eight_holes` is the example: its first read wants the transcription (positions)
and its second wants what a reader sees (live).

That makes the remaining work a per-call-site classification with a decision at each: read the assertion, ask which
question it is, name the entry accordingly. It is mechanical but not batchable, and it is the correct remaining
step for this family.

## Correction: the migration's unit is the observation helper, not the call site

The per-call-site story above is incomplete, and a run through it showed why. Flipping the default and moving
individual call sites reached 744 of 746; the last two failures were inside **shared test helpers**, not inside
tests:

- `rows_for_owner` in the promotion tests serves assertions that want live rows *and* `assert_tree`, which wants the
  stored slot stream including holes;
- `retained_compaction_rows` is the same question one module over;
- the compact tests read rows through a local closure whose two assertions disagree the same way.

A header check (`assert_eq!(header.tombstone_count, holes)`) forces the helper to see holes even where the row
assertion next to it wants live rows, because the header counts holes by definition. So one helper can legitimately
need both views.

That means the migration is: give each observation helper **two** entries (`rows` and `rows_with_holes`, or a
visibility parameter), have each assertion name the one it means, and only then flip the default. That is a bigger
change than a rename and it is the honest unit - the helpers are where the two questions were being conflated, which
is the same defect this whole family keeps producing at every layer.

## The remaining work, enumerated

The helper split was implemented and works: `rows_for_owner` / `rows_with_holes` in the promotion tests and
`retained_compaction_rows` / `retained_compaction_rows_with_holes` in `test_support`, with `assert_tree` taking the
holes view because its header check counts holes by definition. Flipping the default after that left five failures,
all of them call sites that still ask the live question while asserting the hole answer:

| File | Call sites on the observer |
| --- | --- |
| `bidirectional/deferred.rs` | 16 |
| `graph/promote/spill_test.rs` | 46 (three helpers: `rows`, `rows_for_owner`, `rows_with_holes`) |
| `graph/compact.rs` | 2 local closures |

Sixteen call sites are not the obstacle; the obstacle is that each needs its assertion read before it can be named,
because the same test can ask both questions about the same bucket - and the two `compact.rs` sites are a local
closure that does exactly that.

That is the whole of what is left in this family: rename each observation to the question it asks, then let the mode
match go. The dispatch itself has no remaining obstacle, which is what this record set out to establish - the count
went 8 -> 5 and the five that remain are the owner (`visit_rows`), the slab span funnel, the at-slot batch path, this
one entry, and a selected-slot walk with one class difference.

## One level up again: the helpers are shared by tests that disagree

Driving the migration to green was attempted twice more with the helper split in place, and each attempt stopped at
the same place, one layer further up: a **shared observation helper used by two tests that ask different questions**.

`tree_sibling_compaction_*` read their rows through a helper around `deferred.rs:5747`, not directly; switching that
helper to the holes view fixes one test and breaks its sibling, and switching it back does the reverse. The pattern
repeats what the record already found at the call site and then at the helper: the two questions were conflated at
every layer, so each attempt to name one of them surfaces the conflation one level up.

That is a real conclusion about the work, not a reason to keep trying the same edit: the remaining migration is a
**partition of the observation surface** - every observer, helper and assertion gets the question it asks - and it is
a session of its own, not a tail of this one. The dispatch has no remaining obstacle, which is what this record set
out to establish, and the state it left is green.

## The structural fix, first slice: the answer is a type

Every attempt to name the two answers with a parameter landed on the same defect one layer up, so the answer is now
a property of what a walk returns (`graph/row_view.rs`):

| Type | What it can be |
| --- | --- |
| `LiveRow<E> { slot, edge }` | a live row; there is **no value of this type that is a hole** |
| `SlotRow<E> { slot, state }` with `SlotState<E> { Live(E), Hole }` | every stored position; the hole is a *variant* |

Two entries yield them - `visit_live_rows` and `visit_slot_rows` - and the flat entries (`visit_edges`,
`visit_edge_positions`) are now adapters over them, so the typed form is what a helper can take in its signature.
A helper that returns `Vec<LiveRow<E>>` cannot be compared against a `Vec<SlotRow<E>>`: mixing the questions is a
type error rather than a re-read of the test.

One decision inside the slice: `slot_row` is still the single place where the flat `(position, edge)` pair is
interpreted, because every walker arm already produces that pair. The sentinel has not disappeared from the walk yet;
what has changed is that it no longer has to leak into a caller that did not ask for it.

The remaining slices, in order:

1. observation helpers return the typed vectors, so their signatures name the question;
2. the shared helper that two tests disagree over is split by type - it can no longer be a parameter;
3. the sentinel leaves the read paths entirely (`SlotState::Hole` needs no edge), which removes the encoding that
   made the three defects possible;
4. the flat entries go, once their callers name a typed one.

## The with-values family is unified, and the count is 4

The typed row plus the named observers did what three earlier attempts could not: the last legacy funnel's mode
match is gone. `visit_edges_with_inline_property_impl` now asks `BucketValueSource` where a slot's value lives and
applies the caller's visibility once, at its callback boundary; its slab fast path, slab batch path and tree walk
are three shapes of one answer rather than three storage classes with their own.

What that took, in the order it worked:

1. the typed rows (`LiveRow` / `SlotRow`) with a payload, so an observer's signature names its question;
2. `retained_compaction_rows_including_holes` and the promotion tests' `rows_with_holes`, so the readers that need
   positions say so at the call site;
3. the default flipped to live rows - which then *held*, because every reader that needed holes had already named
   itself;
4. the harness checker and the compact test moved to `visit_slot_rows_with_values`, which is the typed position
   entry rather than a flat one.

| Measure | Start of this record | Now |
| --- | --- | --- |
| Functions owning a `BucketMode` dispatch | 8 | **4** |
| Mode-branch sites in `traverse.rs` | 34 | **13** |

The four that remain are each the owner of its own question: `visit_rows` (the row walker), the slab span funnel
(span shapes), the at-slot batch path (a bulk read), and `visit_bucket_edge_slots` (a selected-slot walk with one
class difference). None of them is a second implementation of another's operation.
