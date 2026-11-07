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
