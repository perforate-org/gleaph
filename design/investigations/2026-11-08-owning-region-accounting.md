# Two ledgers, one question: what does a vertex actually own?

Dated record, 2026-11-08. Measured while investigating `GAP-LARA-TINY-GEOMETRY`. The finding is smaller and more
structural than the gap's name suggests: both the edge side and the value side keep an *accounting* of what a vertex owns,
and in both cases that accounting is derived from something narrower than what the vertex actually occupies.

Figure 1-3 are measurements on this tree (the reproduction is
`crates/ic-stable-lara/src/labeled/graph/bucket.rs::a_promoted_tree_keeps_its_edge_root_inside_the_vertex_cover`,
`#[ignore]`d because it asserts the defect); figures 4-7 generalise them.

## 1. The two ledgers cover different stores

```
┌─────────────────────────────────────────────────────────────┐
│ leaf block (edge slots, 1 slot = E::BYTES = 4 B)             │
│                                                             │
│  slot  256        257   258   259   260                     │
│       ┌─────────┬─────┬─────┬─────┬─────┐                   │
│       │         │     │     │     │     │                   │
│       └─────────┴─────┴─────┴─────┴─────┘                   │
│        └── cover ──┘  └─ edge root ──┘                       │
│          [256,257)      [258,260)                            │
│          vertex claim   actually written                     │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│ value store (inline property bytes)                          │
│                                                             │
│  byte   0        8                      20                   │
│       ┌────────┬──────────────────────┐                     │
│       │        │  value root [8,20)   │  12 B = 3 leaf ids   │
│       └────────┴──────────────────────┘                     │
│        vertex accounting: 0                                  │
│        bucket claim:      0                                  │
└─────────────────────────────────────────────────────────────┘
```

ADR 0101 puts the edge root in leaf slots and the value root in the value store, and each has its own ledger:
`LabeledVertex::stored_slots` (edge slots) and `LabeledVertex::inline_property_bytes_allocated_bytes` (value bytes).

## 2. Edge side: the cover does not grow to the root (measured)

```
        cover (claim)         edge root (actual)
        ┌────┐                ┌────┬────┐
slot    256 │ 257            258 │ 259 │ 260
        └────┘                └────┴────┘
         1 slot                2 slots

        claim is 1, need is 2  ->  the "promotion is a shrink" premise fails
        the placement got 258, new_alloc stayed old_alloc -> the cover never heard
        -> slots 258,259 belong to no cover: neither released nor reusable
```

Measured on a width-8 bucket promoted at `T_PROMOTE + 2` rows: `cover = [256, 257)`, edge root `= [258, 260)`. The same
fixture at width 0 gives `cover = [256, 261)` (5 slots) with the root written in place at `256..258`, so the width does not
cause the defect - it changes the *prefix* the vertex kept before promotion.

## 3. Value side: the derivation omits the value root (measured)

```
value store:  [8 ──────── 20)   12 B really allocated
vertex:       (nothing)          0 B accounted
bucket:       (nothing)          0 B claimed

        -> the 12 B is neither released nor reused: a permanent leak per promotion
```

Measured on the same bucket after promotion: `vertex_alloc = 0`, `slab_slots_allocated = 0`,
`value_root_offset = 8`, `value_root_bytes = 12` (three property leaf ids at four bytes each, depth 1).

## 4. Root cause: both ledgers derive from less than what is occupied

```
              "how much does this vertex own?"
                          │
        ┌─────────────────┴─────────────────┐
        │                                   │
   edge side                            value side
        │                                   │
  cover = a number the caller passes    alloc = slab_slots_allocated × width
  (92 sites write with_stored_slots)     (values.rs::bucket_resident_inline_property_bytes_slots_for)
        │                                   │
   actual = bucket [base, base+resident)  actual = value span
            + edge root (when in slots)             + **value-root span**   <- missing
        │                                   │
        └────  neither "actual" is the whole truth ────┘
```

The edge side's ledger is written by its callers rather than derived; the value side's ledger *is* derived, but from a
predicate that does not know about the value root. Two shapes of one question.

## 5. The fix is derivation, not reconciliation

```
[now]
  placement ──► (returns nothing)
  cover     ──► computed by each caller and written separately     (two homes)

[after]
  placement ──► PlacedRegion { start, len }
                      │
                      ▼
             derive: cover = max(existing end, placed.end) - base
                     alloc = value span + value-root span
                      │
                      ▼
             publish (the owner writes; callers cannot)
```

The earlier idea of `new_alloc = max(old_alloc, root_end - cover_start)` at the promotion's call site is the same
anti-pattern the repository has been deleting all along: it patches the one site that drifted and leaves the other 91
callers free to drift the same way.

## 6. When the edge side breaks (measured condition)

```
prefix (slots kept before promotion)  >=  edge root length  ->  written in place, inside the cover   OK
prefix                                <   edge root length  ->  placed outside, cover not grown      BAD

  width 0:     prefix 5, root 2  -> OK
  width 8:     prefix 1, root 2  -> BAD   (values push rows into the run earlier, shrinking the prefix)
  run-backed:  prefix 0          -> BAD   (no values needed)
```

So this is an **edge-side** defect: it is about `stored_slots` and the edge root array. Inline property is a *trigger* in
this fixture (it changes the prefix), not the subject. The value side has its own, separately triggered violation
(figure 3), which happens to appear in the same promotion because that is when a value root is created.

## 7. The value side's fix is one term

```
reconcile_vertex_inline_property_bytes_allocated_bytes()   <- already derive-then-write
        │
        └─ source: bucket_resident_inline_property_bytes_slots_for()
                        │
                        └─ now:  slab_slots_allocated
                           fix:  slab_slots_allocated + value-root span (tree only)
```

Because that path already recomputes the total from the buckets and only writes when it differs, adding the missing term
closes the value-side hole without touching its callers.

## 8. The drift inventory, and why the obvious invariant cannot land yet

The plan for the edge side starts by *finding* which of the cover's writers drift, so the naive ratchet was tried: assert in
`invariants::assert_labeled_layout_invariants` that a vertex's cover backs the region its buckets occupy
(`bucket_span_region_len`) - the rule the promotion case violates. It fired on **six existing fixtures**, not one:

| fixture | vertex | cover | bucket content reaches |
| --- | --- | --- | --- |
| `compact::tests::fold_growth_stays_mate_disjoint_across_span_growth` | 1 | `[0, 1)` | 76 |
| `compact::physical_layout_tests::mode_footprint_tree_append_preserves_pin_and_retirement` | 1 | `[266, 271)` | 345 |
| `headroom_screen::tests::shared_hot_with_cold_preserves_slots_spill_and_reopen` | 0 | `[256, 265)` | 3301 |
| `leaf_arena::tests::labeled_heavy_relocation_survives_reopen_without_corruption` | 3 | `[391, 711)` | 789 |
| `promote::spill_test::root_geometry_normal_promotion_keeps_cover_and_pin` | 0 | `[1048591, 1048687)` | 1048903 |
| `bucket::tests::last_bucket_successor_respects_descriptor_order_cover` | - | fired | - |

That is a result about the *invariant*, not only about the code: "cover ⊇ every bucket's occupied region" is too strict as
written, because at least one of these is a deliberate state. `fold_growth_stays_mate_disjoint_across_span_growth` is the
GAP-2026-09-20-005 regression fixture, whose whole point is a fold that **grows a span and then relocates** - a window where
the cover and the content legitimately disagree until the relocation publishes. The same shape explains the reopen fixtures:
a reopened graph derives the cover from the pin rather than from the buckets.

So the ratchet needs a stated contract first: which windows are allowed to disagree (mid-operation, and reopened-from-pin), and
whether the cover is the *content* union or the *allocated* region (the entry's own witness had cover `[256, 273)` against slab
`[256, 272)`, one slot more, which suggests allocated). Without that decision the assertion measures three different things at
once - and a ratchet that fires on intended states is not a ratchet, it is noise.

**The experiment was reverted rather than landed red.** It cost one suite run and produced the table above, which is the
inventory the plan asked for; the next slice starts from the contract question, not from the assertion.

## 9. The contract decision

The measurements above force a choice about what a **vertex cover** is, and the repo already answers most of it - in
`bucket::tests::last_bucket_successor_respects_descriptor_order_cover`, which states the rule as:

```
cover := [first_non_tiny.edge_start, + vertex.stored_slots)
cover ⊆ leaf pin
for every non-tiny bucket: !tree_mode && edge_start + initialized_slab_slots(bucket) <= cover.end
covers pairwise disjoint
```

Three things follow from reading it against the findings:

- **The cover is the *allocated* region, not the content union.** `stored_slots` includes growth slack; the check against it
  uses *initialized* rows precisely so slack is allowed to exist inside a cover. So "cover = content" is wrong, and the
  failed experiment's first form was measuring the wrong quantity in half its cases.
- **Trees are excluded by name** (`assert!(!bucket.is_tree_mode())`), which is the hole this record measures: a tree's edge
  root occupies leaf slots when it is not run-backed, and nothing claims them.
- **The check belongs at commit boundaries.** Two of the eight fixtures that fired are the fold-then-relocate window and the
  reopen-from-pin path - states that are mid-publish or derived from the pin rather than from buckets.

**Decision, for the edge side:**

1. The cover is the vertex's **allocated leaf region**: it must contain every bucket's initialized rows **and a tree's edge
   root when that root lives in leaf slots** (a run-backed root is in the edge run and needs no cover entry).
2. The invariant is checked at **commit boundaries**, not mid-operation: the two windows above are exempt by construction
   (their covers are published by the relocation and by the pin, not by the buckets).
3. A cover that fails (1) is a defect even if every individual step looked right - which is what the promotion case is.

With that, the naive assert becomes implementable as "occupied ⊆ cover", using `initialized_slab_slots` for slab and
`bucket_span_region_len` for a slots-rooted tree. Re-running it under decision (2) is the next measurement: the eight fixtures
that fired included both real drift and exempt windows, and separating them is exactly the work decision (2) enables.

## 10. Correction: figures 2 and 8 measured two address spaces

**The edge-side violation in figure 2 does not exist as stated.** `LabeledVertex::base_slot_start` is an address in the
**bucket** store; a vertex cover is a region of the **edge** store, anchored at the vertex's first occupied bucket
(`edge_start`). Subtracting the first from the second produced a "violation" that was an artefact of the comparison, and the row
it was read from stores only a *width* - so the claim it makes is `[edge_base, edge_base + stored_slots)`, which does back the
root at `[258, 260)`.

Corrected measurement (`bucket::tests::a_promoted_tree_keeps_its_edge_root_inside_the_vertex_cover`, now passing for width 0 and
width 8): the root is inside the cover, and it stays inside because the append republishes the cover - removing that call makes
the same test fail (`cover [258, 259)` against a root at `[258, 260)`), which is the first evidence that the republish is
load-bearing.

Consequences for this record, stated rather than quietly fixed:

- **Figure 8's drift inventory is invalid as measured.** It used the same `base_slot_start` arithmetic on six (then eight)
  fixtures, so those firings may be artefacts too. They must be re-measured against an edge-anchored base before any of them is
  called drift.
- **Figure 6's condition** (`prefix < edge root length`) described the promotion's own arithmetic, not a cover violation; the
  promotion derives a one-slot root and is right about it. The statement stands as an observation, not as a defect mechanism.
- **Figure 3 (the value side) is unaffected.** It never used the base: the accounting was zero against a real 12-byte span, and
  that fix (`71dadf4a`, `47ca7e71`) is independent of this correction.
- What remains true from the record: the two ledgers exist, the value-side derivation was incomplete, and the types now state the
  retention rule (`SlackRule::NeverShrink`) with an oracle (`VertexLeafCover::monotone`).

The lesson is the one this repository keeps teaching: a derived value compared against a *differently derived* one is a
measurement bug waiting to happen, and "I measured a violation" needs its own address space checked before it is believed.

## 11. What the correction built: three address spaces, one of them typed

Section 10's mistake was possible because edge slots, bucket-descriptor slots and value-store bytes are all `u64`. The first
slice of the fix retypes the seam where the mistake happened: `BucketLeafSpan` and `VertexLeafCover` now start at an
`EdgeSlot`, with an explicitly named conversion (`from_raw`, and **no** `From<u64>` impl - an implicit conversion would restore
exactly the mixing the type prevents).

Deliberately not done yet: `LabelBucket::edge_start()` (234 call sites) and `LabeledVertex::base_slot_start()` (155) still
return plain numbers, and `inline_property_bytes_offset()` (41) likewise. The compiler can enumerate those sites, but each one
needs its space *read* rather than guessed - which is the difference between this slice and the mechanical one that follows.

## 12. The release-side clip: one arm is real, the other was never a placement

`parts_outside_extent` clipped two kinds of overrun, and it was worth asking which actually happen before designing a fix for
either:

| arm | value | verdict |
| --- | --- | --- |
| head (`start < extent_start`) | `start=256` against `extent_start=384` | **real** - a version of the span predates the current block |
| tail (`end > extent_end`) | `[272, 275)` against a block ending at 272 | **not from a placement**: it comes from the split's own test, which constructs a straddling span on purpose |

The tail arm was the one that suggested storing the allocated length in the descriptor so the release would not re-derive a
bigger number. Measuring first avoided that: no descriptor field is needed, and the arm stays only because the split's contract
accepts an arbitrary span.

What *was* worth fixing is the thing the tail measurement exposed: `reserve_with` accepted a slice allocation past the block's
tail (`[272, 275)` on a block ending at 272) as a placement, which then forced the release path to clip a span the arena had just
granted. Every step of the ladder now requires placement to land inside the block, so that case falls through to "make room" and
the ladder fails closed instead.

The head arm is a different question - a span whose version predates the current block, i.e. a *stale address* carried across a
relocation - and it is the one that needs the ownership answer, not a size change.

## 13. Correction: the head clip is not staleness, it is the relocation being handled correctly

Section 12 said the head arm was "a stale address that needs an ownership answer". Attribution of the *specific* occurrence says
otherwise, and the numbers make it plain:

```
release target  [256, 385)
current block   [384, 1408)
clipped part    [(256, 128)]        <- everything before the block
```

The old version of the span lived at `[256, 384)`; the block then moved to `384`. That prefix is now **free space outside the
block** and belongs back in the free-span store; the one slot at `384` is inside the block, is live, and the clip correctly
leaves it alone. `parts_outside_extent` is therefore doing the right thing on this arm - it is the mechanism that keeps a
relocation from either leaking the old area or double-freeing a live slot.

Two consequences:

- **There is nothing to fix here.** The "stale address" framing was mine, from reading a clip without checking what the clip
  removed. Referring to the arm as a defect would have produced a fix for correct code.
- **The depth-2 descriptor re-read was unnecessary.** It was added on the assumption that the clip came from the property-deepen
  path; site marking showed the occurrence comes from the depth-1 edge tail append (`CLIPSITE A`), whose release runs after the
  same operation's relocation. Reverted: no measured benefit, and the re-read after publish would have been wrong for the reasons
  recorded in section 12's B-2 attempt.

The still-true part of section 12: the *tail* arm is never produced by a placement, and `reserve_with` now requires placements to
land inside the block. That requirement was the real fix in this area, and it stands on its own.

## 14. The untouched prefix: initial capacity the append-only allocator never hands out

The leak-accounting identity needed one allowance - a gap before the first owned range - and the measurement says exactly what it
is:

```
initial edge capacity   256 slots
first leaf pin          [256, 272)          <- allocated *at* the capacity end, so capacity grew to 272
free spans              none
=> [0, 256)             never owned, never free
```

`EdgeStore::allocate_span_avoiding` is **append-only**: it reuses a best-fit span from the free list when one exists, otherwise it
allocates at `elem_capacity` and grows. It never hands out the initial capacity from the bottom. So the 256 slots a fresh store
is created with are unreachable until something is retired *into* them, and a store that has never retired anything leaves them
stranded.

Consequences, stated plainly:

- It is not a leak by the definition this record uses (nothing stopped being owned or free), and the accounting test pins that it
  does not **grow**: a higher prefix after an operation would mean a slot stopped being either.
- It is real committed memory: 256 slots x `E::BYTES`. For 4-byte edges that is 1 KiB per store per orientation - small, but it
  is the one genuinely unused region this investigation found.
- The fix, if it is worth doing, is not in the release path: either the allocator should hand out the initial capacity from the
  bottom (best-fit over `[0, elem_capacity)` instead of appending past it), or a fresh store should start at capacity zero and
  grow on first use. Both are allocator-policy changes with their own measurement, and neither is needed for correctness.

## 15. Option A tried and reverted: seeding the free list breaks the fixtures that hand-place spans

A was implemented the cheap way - on fresh init, register `[0, elem_capacity)` as one free span so the allocator's first-fit
takes it before appending past the end - and it worked at the allocator level (`take_best_fit` splits correctly). What it broke
was **50 tests**, in a way that says who depends on the old shape:

| module | failures |
| --- | --- |
| `tree_write::tests` | 21 |
| `tree_read::tests` | 16 |
| `promote::tests` | 6 |
| `compact::physical_layout_tests` | 5 |
| others | 2 |

The cause is not the allocator but the fixtures: whitebox layout tests **place spans by hand** and then assert on the free list
(`free_byte_spans()` contains exactly these ranges, the prefix is untouched, releases do not overlap). With a seeded span they
place inside it, and the release reports `OverlapPrevious { previous: [0, 256), inserted: [48, 8) }` - a correct complaint about
a fixture that assumed the free list starts empty.

So A's real cost is a fixture-by-fixture rewrite of the layout tests, and its benefit is *logical*: the declared capacity becomes
usable memory. It buys **no committed bytes**, because the prefix sits inside the page the first write already commits
(section 14). That trade is not worth taking now: 50 fixture rewrites and a changed allocator shape, for capacity semantics
nobody is currently blocked on.

Reverted, with the measurement recorded so the next attempt starts from it rather than from the idea.

## 16. Why "use a shared fixture constructor" only half-solves option A

The proposal was right in direction and is now in the code: `test_support::place_slab_bucket` places a fixture bucket **through the
allocator** (`ensure_labeled_leaf_block_pinned` -> `find_free_labeled_leaf_edge_base` -> `publish_bucket_descriptor` -> counts), so
a fixture states a degree and a label instead of an address. New fixtures should use it.

It does not, on its own, unblock A, and the reason is worth stating before someone spends a day on it: the fifty failing fixtures
are not sloppy - they are **whitebox geometry fixtures**. `compact::physical_fixture`, for instance, deliberately places two
buckets at `100` and `68` inside a block pinned at `64` so that a hole exists between them; the tests then assert on that hole.
Driving them through the allocator would place the buckets contiguously and delete the thing under test.

Under a seeded free list those fixtures also cannot simply *claim* their ranges: `FreeSpanStore` can take a prefix
(`take_prefix_at`) or a best fit, but it has **no operation that carves an explicit `[start, start+len)` out of the middle of a
free span**. So there are exactly two ways forward, and both should be chosen deliberately:

- **Rewrite the fifty fixtures** to allocator-driven geometry. Cheap mechanically, but each becomes a weaker test: the exact hole
  or neighbour it was written to exercise is no longer specified.
- **Give `FreeSpanStore` a `reserve_range(start, len)`** that splits a middle range out of whatever free span covers it, and have
  the fixture helpers call it for the spans they hand-place. This keeps the geometry exact *and* makes the store able to express
  "this range is reserved", which is a real allocator capability that seeding any initial capacity needs anyway.

The second is the smaller and more truthful fix, and it is its own slice: a new store operation, its own tests (split at the
front, in the middle, at the tail, across two spans), and only then the seeding. A stays reverted until that exists.

## 17. What the claim-per-fixture route actually requires

`FreeSpanStore::reserve_range` landed (a real capability: claim a range the caller did not obtain from the allocator) and
`compact::physical_fixture` was given a claim for each of its two hand-placed spans. With the declared initial capacity seeded,
the remaining failures state the requirement precisely:

```
OverlapPrevious { previous: [0, 68), inserted: [48, 8) }
OverlapPrevious { previous: [70, 30), inserted: [88, 4) }
free_byte_spans().iter().all(|span| span.end <= 64 || span.start >= 80)     // a fixture asserting its own layout
```

- A fixture must claim **every** range it hand-places **or releases** - not only the ones it writes descriptors for. The failing
  ranges above (`48`, `88`, the `[64, 80)` window) are spans the tests *release* into a layout they built by hand.
- The only way to discover that list is to run with the seeding **on**. With it off a claim is a no-op, so the migration cannot
  be done green-first the way the allocator-driven constructor was.

So option A is not blocked by design any more - both primitives it needs exist - it is blocked by a fixture-migration session
that must run red-first: seed the capacity, let the failure list enumerate the unclaimed ranges, add the claims fixture by
fixture, and finish when the suite is green. That is a session of its own, with the seeding committed only at its end.

Until then: A stays reverted, `reserve_range` stays (it is useful and tested on its own), and the untouched prefix stays the
one measured piece of genuinely unused memory - inside a page that is already committed, so it costs no additional pages.

## 18. A closed: the fixtures describe layouts by releasing, not by claiming, so seeding is not worth it

The red-first attempt was made with the strongest general fix available - **a descriptor publication claims the span it names**:

```
publish_bucket_descriptor -> free_span_store.reserve_range(edge_start, stored_slots)
```

That is not a test hack: a descriptor that names live slots while the free list still believes they are free describes two owners
of one range, which is precisely what a later release reports as `OverlapPrevious`. It is a net worth having in principle.

It did not fix the failures, and the residue says why:

```
OverlapPrevious { previous: [0, 68), inserted: [48, 8) }
OverlapPrevious { previous: [71, 29), inserted: [88, 4) }
```

Those released ranges were **never named by a descriptor**. The whitebox fixtures do not build a layout by publishing spans that
cover it; they build one by *releasing* spans at chosen offsets into an empty store. Under a seeded free list those releases are
releases of ranges the store still considers free - the fixture's own double-free, not a missing claim.

So A would require every one of those fixtures to **declare its whole layout up front** (claim every range it will place or
release), which is fixture-by-fixture work with no general shortcut - and its benefit is still zero pages (§14), because the
prefix sits inside a page the first write already commits.

**Closed as not worth it**, with the primitives kept where they are independently useful: `FreeSpanStore::reserve_range` (a
store capability, tested on its own) and `test_support::place_slab_bucket` (how new fixtures should place a bucket). The one
measured piece of genuinely unused memory stays: 16 KiB of declared initial capacity for 4-byte edges, inside an already
committed page, pinned against growth by the accounting test.

## 19. Re-measuring through the shared invariant found the append path's value accounting

Section 10 invalidated the drift inventory because figures 2 and 8 measured two different address spaces. The re-measurement did
not need a new probe: `assert_labeled_layout_invariants` already answers the question on the edge-anchored arithmetic, so the
promotion-shape test was routed through *it* instead of this record's hand-rolled cover copy. It fired immediately, with a
different finding:

```
vertex 0: inline_property_bytes_allocated_bytes must equal sum of resident bucket value spans
  left: 8      <- the vertex still charged the promoted root's width
  right: 12    <- the derivation, from a value root that now holds three leaf ids
```

The fixture promotes at `T_PROMOTE` (1024 rows, two property leaves = 8-byte root), then appends two more edges. The first append
crosses the property-leaf boundary, so the value root is **replaced** by a wider one - and the append path never reconciled the
vertex, while promotion did. The number therefore depended on which path last touched the vertex.

The fix keeps the rule where it belongs. The reconcile is not copied into the two append sites: a tree plane built by
`tree_plane_for(vid, ..)` derives the vertex's resident value bytes from the **published** buckets once the descriptor write and
the replaced span's retirement have succeeded. Every publication site builds its plane that way; readers keep the plain
`tree_plane(bucket)`; promotion's hand-placed reconcile call collapses into the same entry. Removing the reconcile reproduces
`left: 8, right: 12`, so the test is not vacuous.

Two things this teaches about the method, not just the fix:

- The invalid inventory was worth re-measuring, but the re-measurement is worthless if it re-uses the instrument that was wrong.
  Routing the shape through the *shared* invariant made the difference visible.
- `tree_mode_property_deepen` took `_vid` and ignored it. A parameter that is unused is a rule that has not been found yet; once
  the publication owns the reconcile, the deepen path needs its vertex.

## What this record does not claim

- No measurement of the other 91 `stored_slots` writers: which of them drift is unknown until the invariant test from
  figure 5 exists. The edge-side fix therefore starts by *finding* the drift, not by fixing the promotion.
- No instruction cost: these are correctness and space findings, not performance ones.
- No claim that the two violations are the only ones: they are the two this promotion produces, measured.

## Reproduction

```
cargo test -p ic-stable-lara --lib -- --ignored a_promoted_tree_keeps_its_edge_root_inside_the_vertex_cover
```

The diagnostic values in figures 2-3 were printed from that test (a temporary `eprintln!`, removed before the commit that
added the reproduction). Figure 6's width-0 and width-8 rows are the two iterations of the same fixture; the run-backed row
is the condition stated by the code path (`prefix = 0` after `promote_tiny_to_run_backed_slab`), not a separate run.
