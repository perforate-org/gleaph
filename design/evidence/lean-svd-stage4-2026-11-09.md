# vec-deque Lean fixture: Stage 4' zero-movement proof (2026-11-09)

Anchor timestamp: 2026-11-09 20:05:00 UTC +0900
Scope: plan 0378. Integrate the independently completed `grow_preserves_contentOf` proof, remove the
fixture's last `sorry`, and record the Stage 4' result. Parent commit:
`f561eddd` (plan 0377). No Rust source, CI workflow, checker or other fixture changed.

## Context

Plan 0377 restated Stage 1' (block-routing arithmetic) and Stage 2' (128-byte header validity) for
the ADR 0086 segmented block-ring, leaving the Stage 3' scaffold and its single `Grow` obligation
(`grow_preserves_contentOf`) open. An independent proof attempt completed that obligation; this
slice reviews and integrates it.

## Proof

`crates/ic-stable-vec-deque/formal/Svd/Abs/Grow.lean` (+167/−1):

- projection lemmas for the `opGrow` record (`opGrow_blocks/dir/headOff/blockSlots/virtCap`);
- routing bridge lemmas `routeBlock_eq`/`routeSlot_eq` (the proof targets the current
  `Svd/Abs/State.lean` definitions, which delegate to `Svd.BlockRing`);
- `mulAddModBlock` — `(B·m + s) % (N·B) = B·(m % N) + s` for `N > 0`, `s < B`;
- `grow_preserves_contentOf` — with `x = headOff % B + i < (N+1)·B`, split `x` into `(q, s)`:
  - `q < N`: the rotated directory entry is `st.dir ((q + rot) % N)` (the old block), and
    `hfresh` keeps `nb` out of live entries, so the old reading is preserved;
  - `q = N`: the new block `nb` receives the boundary migration for offsets `s < headOff % B`
    (proved from `bdy + i = B·N + s` and `i < N·B`), and `(rot + N) % N = rot` gives the old
    reading.

The file contains no `sorry`, `admit`, `axiom`, `unsafe` or `native_decide`; the reviewed proof is
kernel-checked in the current tree.

## Build and axiom transcript

```text
$ cd crates/ic-stable-vec-deque/formal && lake build
Build completed successfully (11 jobs).          # no sorry warning

$ lake env lean Svd.lean
'Svd.virtualIndex_lt' does not depend on any axioms
'Svd.virtualIndex_injective' depends on axioms: [propext, Quot.sound]
'Svd.virtual_pos_decomp' depends on axioms: [propext]
'Svd.blockOf_lt' depends on axioms: [propext, Classical.choice, Quot.sound]
'Svd.slotAddr_lt_memBytes' depends on axioms: [propext, Quot.sound]
'Svd.rotateIndex_injective' depends on axioms: [propext, Quot.sound]
'Svd.initialHeader_valid' depends on axioms: [propext, Quot.sound]
'Svd.Abs.grow_preserves_contentOf' depends on axioms: [propext, Classical.choice, Quot.sound]
```

The new guard is `#print axioms Svd.Abs.grow_preserves_contentOf` in `Svd.lean`; no `sorryAx`
appears. `Classical.choice` (also in `blockOf_lt`) is recorded as a cleanup item because
`lean-verify.yml` rejects it; the fixture is not wired into that gate yet.

## Fixture state

- Verified: Stage 1' (routing), Stage 2' (header validity), Stage 4' (zero-movement growth).
- Open: Stage 3' observable operation contracts and `Inv` preservation; Stage 5' failure atomicity.
- Freshness: manifest stays `pending-reanchor` (advisory), now because the Stage 3' contracts are the
  remaining blocker for `anchored`/blocking; the build is sorry-free.
- Checker: lara anchored-clean, lhm anchored-clean, svd pending-clean, exit 0.

## Documentation updates

- `formal/SCOPE.md`: Stage 4' verified, status/verified-properties list updated, `Classical.choice`
  cleanup recorded, Stage 3' identified as the anchoring blocker.
- `formal/REPORT.md`: restatement bullet/status updated and a Stage 4' completion subsection added.
- `SOURCES.sha256`: comment updated only (same `pending-reanchor` status and anchor).
- ADR 0086 and ADR 0104: Stage 4' implementation notes.

## Remaining obligations

1. Stage 3': observable contracts for `opGet/opSet/opPushBack/opPushFront/opPopBack/opPopFront` and
   `Inv` preservation (including `retireTop` and the free-list/directory bundle).
2. Remove the `Classical.choice` dependencies, wire the fixture into `lean-verify.yml`, and flip the
   manifest to `anchored` (blocking).
3. Stage 5' failure atomicity.
