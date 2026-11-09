# vec-deque Lean fixture: Stage 3' operation contracts and Inv preservation (2026-11-09)

Anchor timestamp: 2026-11-09 20:15:00 UTC +0900
Scope: plan 0379. Complete the block-ring Stage 3' layer: observable-list contracts for
get/set/push_back/push_front/pop_back/pop_front, `Inv` preservation including retirement/head reset,
axiom guards, and flip the fixture's freshness manifest to `anchored`. Parent commit: `9d73f9d4`.
No Rust source, CI workflow, other fixture or checker changed.

## Contract correction (important)

The unconditional list contracts for `set`/push are **false** without `Inv`. Machine-checked
counterexamples from the independent proofs:

- `set`: `blockSlots = virtCap = 1, headOff = 0, len = 2`, directory non-injective ⇒ positions 0 and
  1 route to the same slot, so setting 0 also changes 1 (violates `Inv.lenLe`/`Inv.dirInj`).
- `push_back`/`push_front`: a non-injective directory over a live window (e.g. `dir ≡ 0`,
  `numBlocks = 2`, `blockSlots = 2`, `virtCap = 4`, `len = 2`) makes the written slot alias an old
  reading, so the appended/prepended list shape fails.

The contracts therefore carry `inv : Inv st`; routing injectivity on the live window is exactly what
`Inv.dirInj`/`Inv.lenLe` supply, so this is a contract requirement, not a proof convenience.

## Modules and theorems

- `Svd/Abs/SpecSet.lean`: `opGet_spec`; `opSet_spec` (Inv + `p < len`); `inv_opSet`; helpers
  `specSet_contentOf_opSet_self`/`_ne`.
- `Svd/Abs/SpecPush.lean`: `opPushBack_spec`/`opPushFront_spec` (Inv + `len < virtCap`);
  `inv_opPushBack`/`inv_opPushFront`; routing/`prevHead` arithmetic helpers.
- `Svd/Abs/SpecPop.lean`: `opPopBack_none`/`opPopFront_none`; `opPopBack_spec`/`opPopFront_spec`
  (value + `dropLast`/`tail`); `inv_opPopBack`/`inv_opPopFront` through `retireTop`, the
  pop-front drained/wrapping cases and the head reset.

All are imported by `Svd.lean` and guarded with `#print axioms`. The integrated `SpecPush` needed
mechanical fixes (a malformed `specPush_vpos_ne_of_lt` call, list-length/`flatRead` rewrite order)
and the contract rename from `_of_inv` to the canonical names; the owner then finished the module on
top of that edit (final SHA-1 `96af05e58980cbb312b2122bbaf7150bea3de633`, 17,968 bytes) and confirmed
the bare names with the `Inv` hypothesis.

## Build and axiom transcript

```text
$ cd crates/ic-stable-vec-deque/formal && lake build
Build completed successfully (14 jobs).          # no sorry warning

$ lake env lean Svd.lean
... opGet_spec / opSet_spec / inv_opSet / opPopBack_spec / inv_opPopBack /
    opPopFront_spec / inv_opPopFront / opPushBack_spec / opPushFront_spec /
    inv_opPushBack / inv_opPushFront ... all depend on
    [propext, Classical.choice, Quot.sound]; the *_none theorems use no axioms;
    no guard prints sorryAx.
```

`Classical.choice` is inherited from the Nat/routing lemmas; `lean-verify.yml`'s axiom gate rejects
it, so removing those dependencies is the remaining prerequisite for wiring this fixture into that
workflow.

## Freshness gate

`crates/ic-stable-vec-deque/formal/SOURCES.sha256` flips from `pending-reanchor` to `anchored` at
anchor `997debce…` (hash of `src/vec_deque.rs` unchanged). Checker transcript:

```text
$ python3 scripts/check-lean-sources.py
fixture crates/ic-stable-lara/formal/SOURCES.sha256 [anchored …]      ok: 2 source(s) unchanged
fixture crates/ic-stable-linear-hash-map/formal/SOURCES.sha256 [anchored …]  ok: 4 source(s) unchanged
fixture crates/ic-stable-vec-deque/formal/SOURCES.sha256 [anchored …] ok: 1 source(s) unchanged
summary: 3 manifest(s), 0 blocking, 3 advisory/anchored-clean, 0 obsolete
```

All three Lean fixtures are now anchored; drift in any cited source is blocking.

## Documentation updates

- `formal/SCOPE.md`: Stage 3' verified, Stage 5' only remaining proof work, manifest anchored,
  `Classical.choice`/CI note.
- `formal/REPORT.md`: restatement/status text updated and a Stage 3' completion subsection added.
- `SOURCES.sha256`: `anchored` + restatement-complete comment.
- ADR 0086/0104: Stage 3' completion and anchored status notes.

## Remaining work

1. Stage 5' failure atomicity (grow-then-write ordering).
2. Remove the `Classical.choice` dependencies from the spec/Inv proofs, wire the fixture into
   `lean-verify.yml`, and (optionally) do the same for lara if its axiom set is acceptable.
