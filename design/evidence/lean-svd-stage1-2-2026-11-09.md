# vec-deque Lean fixture: block-ring Stage 1'–2' restatement (2026-11-09)

Anchor timestamp: 2026-11-09 19:45:00 UTC +0900
Scope: plan 0377. Restate the vec-deque fixture's routing arithmetic and header validity for the
ADR 0086 V1 segmented block-ring, delete the superseded contiguous-ring modules, and move the
freshness manifest from `obsolete` to `pending-reanchor` (advisory). Parent commit:
`997debcebd059d92c77c79054d9e60c25b98d6c8`. No Rust source, test, CI workflow or other fixture
changed.

## Context

ADR 0086 replaced the contiguous ring with the segmented block-ring; the fixture's old
`Svd/Ring.lean`/`Svd/Header.lean` modelled deleted code and the manifest was `obsolete`. A prior
scaffold (`9b52754a`) had already added the Stage 3' abstract state/`Inv`/transfer/operation
definitions and an unfinished growth obligation (`Svd/Abs/Grow.lean`, one `sorry`). This slice
restates the layout-critical layers and records the fixture as **restating**, not verified.

## Delivered

New Stage 1' (`Svd/BlockRing.lean`, verified):

- `virtualIndex` (`(headOff + i) % virtCap`), `blockOf`, `slotOf`, `slotAddr`, `rotateIndex`;
- `virtualIndex_lt` (extent), `virtualIndex_injective` (routing injection on `[0, len)`),
  `virtual_pos_decomp`, `blockOf_lt`, `slotOf_lt`, `slotAddr_lt_memBytes`,
  `rotateIndex_injective` (directory rotation injects the live range).

New Stage 2' (`Svd/BlockHeader.lean`, verified):

- 128-byte-header constants (`DATA_OFFSET = 128`, `TARGET_BLOCK_BYTES = 256 KiB`,
  `INITIAL_DIR_SLOTS = 16`, `FREE_LIST_NIL = 2^64 − 1`), `blockSlotsFor`, `HeaderV1`;
- `HeaderValid` mirroring `init`'s geometry checks (blockSlots, power-of-two directory, entry count
  and bounds, `virtCap = numBlocks·B`, len/head ranges, element-bound fields, directory extent,
  free-list head, per-entry memory bounds);
- `initialHeader_valid` for the header written by `new`, given the allocator facts.

Ownership/DRY: the routing formula now has one owner (`BlockRing`); `Svd/Abs/State.lean`'s
`routeBlock`/`routeSlot` were rewritten to call `blockOf`/`slotOf` over `virtualIndex` instead of
re-deriving `(headOff + i) % virtCap` locally. The superseded `Svd/Ring.lean` and `Svd/Header.lean`
were deleted.

## Build and axiom transcript

```text
$ cd crates/ic-stable-vec-deque/formal && lake build
warning: Svd/Abs/Grow.lean:38:8: declaration uses `sorry`   # pre-existing Stage 3'/4' obligation
Build completed successfully (11 jobs).

$ lake env lean Svd.lean
'Svd.virtualIndex_lt' does not depend on any axioms
'Svd.virtualIndex_injective' depends on axioms: [propext, Quot.sound]
'Svd.virtual_pos_decomp' depends on axioms: [propext]
'Svd.blockOf_lt' depends on axioms: [propext, Classical.choice, Quot.sound]
'Svd.slotAddr_lt_memBytes' depends on axioms: [propext, Quot.sound]
'Svd.rotateIndex_injective' depends on axioms: [propext, Quot.sound]
'Svd.initialHeader_valid' depends on axioms: [propext, Quot.sound]
```

Known cleanup item: `blockOf_lt` currently pulls `Classical.choice` (the other new theorems use
only `propext`/`Quot.sound`). The fixture is not in the `lean-verify.yml` axiom gate, so this is
recorded rather than hidden; remove it before wiring the fixture into that gate.

## Freshness gate

`crates/ic-stable-vec-deque/formal/SOURCES.sha256` now:

```
# status: pending-reanchor
# anchor: 997debcebd059d92c77c79054d9e60c25b98d6c8
# reviewed: 2026-11-09
cc15d5be…  crates/ic-stable-vec-deque/src/vec_deque.rs
```

Checker transcript:

```text
$ python3 scripts/check-lean-sources.py
fixture crates/ic-stable-lara/formal/SOURCES.sha256 [anchored …]  ok: 2 source(s) unchanged
fixture crates/ic-stable-linear-hash-map/formal/SOURCES.sha256 [anchored …]  ok: 4 source(s) unchanged
fixture crates/ic-stable-vec-deque/formal/SOURCES.sha256 [pending-reanchor anchor 997debce…]
  ok: 1 source(s) unchanged
summary: 3 manifest(s), 0 blocking, 3 advisory/anchored-clean, 0 obsolete
```

The fixture is no longer `obsolete`, but it is deliberately **not** `anchored`: the Stage 3'
contracts and Stage 4' zero-movement proof are open, so drift is advisory. Flipping it to
`anchored` (and wiring `lean-verify.yml`) is the final restatement slice.

## Remaining obligations

1. Stage 3': observable-list contracts for `opGet/opSet/opPushBack/opPushFront/opPopBack/opPopFront`
   and `Inv` preservation (including `retireTop` and the free-list/directory bundle).
2. Stage 4': `grow_preserves_contentOf` (directory rotation + at most one boundary-block migration
   preserves every routed reading), currently the only `sorry`.
3. Stage 5': failure atomicity.
4. Remove the `Classical.choice` dependency from `blockOf_lt`, then wire the fixture into
   `lean-verify.yml` and flip the manifest to `anchored` (blocking).

## Documentation updates

- `formal/SCOPE.md` rewritten to the block-ring stage plan with the verified-vs-open boundary.
- `formal/REPORT.md`: top layout note marks the contiguous-ring findings historical; new
  block-ring restatement section records the delivered layers and open obligations.
- ADR 0086: formal-layer implementation note. ADR 0104: vec-deque moves from `obsolete` to
  `pending-reanchor` while lara/lhm stay anchored.
