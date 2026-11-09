# Formal Verification Scope — ic-stable-vec-deque

Anchor timestamp: 2026-11-09 19:45:00 UTC +0900
Target revision: git `997debcebd059d92c77c79054d9e60c25b98d6c8` (restatement started; Stage 1'–2' verified against
`src/vec_deque.rs` at this revision)

## Mode

**Audit mode against an existing implementation, run as a permanent fixture.**
`formal/` is a standing Lean project that is re-checked with `lake build` whenever
the deque changes. Findings are recorded in `REPORT.md`, refreshed alongside the
proofs.

The fixture is **restating** the ADR 0086 segmented block-ring. The contiguous-ring
results in `REPORT.md` are historical (that layout was deleted); the block-ring
stage plan below replaces them.

## Location and form

- Lean lake project: `crates/ic-stable-vec-deque/formal/`
- Independent from the Cargo workspace (no `Cargo.toml`; the workspace lists
  members explicitly, so this directory is invisible to Cargo).
- Toolchain: Lean 4 core only (`leanprover/lean4:v4.33.1`). No Mathlib.
- Run locally: `cd crates/ic-stable-vec-deque/formal && lake build`.
- Freshness gate: [`SOURCES.sha256`](./SOURCES.sha256) pins `src/vec_deque.rs` at
  anchor `997debce…`; `scripts/check-lean-sources.py` runs in CI
  (`.github/workflows/lean-freshness.yml`). Status is `anchored` since plan 0379:
  the model transcribes the shipped block-ring for Stage 1'–4' and any source drift
  is **blocking**. Re-anchor only by re-reading the diff, updating the model and
  refreshing the manifest hashes together.

## Component breakdown (block-ring stage plan)

1. **Stage 1' — block-routing arithmetic (verified)** — `Svd/BlockRing.lean`:
   `r = (headOff + i) % virtCap`; block `k = r / B`, slot `k' = r % B`, address
   `dir k + k'·slotSize`; routing extent/injection, block/slot decomposition and
   bounds, the address bound, and directory-rotation injectivity
   (`rotate_directory`, vec_deque.rs L471-L485).
2. **Stage 2' — header validity (verified)** — `Svd/BlockHeader.lean`: the
   128-byte `HeaderV1` field set, `HeaderValid` mirroring `init`'s geometry checks
   (blockSlots, directory power-of-two and entry bounds, `virtCap = numBlocks·B`,
   `len ≤ virtCap`, front position, `dirBase ≥ DATA_OFFSET`, free-list head and
   directory extent), and `initialHeader_valid` for the fresh header written by
   `new`.
3. **Stage 3' — abstract list specification (verified)** —
   `Svd/Abs/{State,Transfer,Ops}.lean` define the abstract state, `Inv`, the
   content/list transfer lemmas and the operations; `SpecSet.lean`/`SpecPush.lean`/
   `SpecPop.lean` prove the observable-list contracts and `Inv` preservation.
   The `Inv` hypothesis on `set`/push is part of the contract: without it a window
   longer than `virtCap` aliases two logical positions onto one physical slot
   (machine-checked counterexample in the spec modules' docstrings).
4. **Stage 4' — zero-movement growth (verified)** —
   `Svd/Abs/Grow.lean::grow_preserves_contentOf`: rotating the directory and
   migrating at most one boundary block preserves every routed reading below the
   old `len`. This is the ADR 0086 replacement for the deleted contiguous-ring
   cycle rotation; the fixture has no `sorry` any more.
5. **Stage 5' — failure atomicity (planned)** — grow-then-write ordering as in the
   original plan; not attempted.

## Properties verified

Stage 1' (`Svd.lean` axiom guards):

- `virtualIndex_lt` — every routed virtual position is inside `virtCap`.
- `virtualIndex_injective` — on `[0, len)` with `len ≤ virtCap`, distinct logical
  indices never share a virtual position.
- `virtual_pos_decomp` — `B·(r/B) + r%B = r`.
- `blockOf_lt` — a routed position names a live directory entry when
  `virtCap = numBlocks·B`.
- `slotAddr_lt_memBytes` — a routed address is inside memory when its block base
  plus one block of slots fits.
- `rotateIndex_injective` — directory rotation is injective on the live range.

Stage 2':

- `initialHeader_valid` — the header written by `new` passes the modelled
  `HeaderValid` checks given the allocator facts (128-byte header and 16-entry
  directory fit; every directory slot is inside memory).

Stage 3' (all guarded in `Svd.lean`):

- `opGet_spec` — `get` is the flattened reading of the observable list.
- `opSet_spec` — under `Inv` and `p < len`, `set` is `putAt` on the observable
  list; `inv_opSet` preserves `Inv`.
- `opPushBack_spec` / `opPushFront_spec` — under `Inv` and `len < virtCap`, the
  observable list appends/prepends `some v`; `inv_opPushBack`/`inv_opPushFront`
  preserve `Inv`.
- `opPopBack_none`/`opPopFront_none` and `opPopBack_spec`/`opPopFront_spec` —
  empty pops are no-ops; otherwise the value is the last/first reading and the
  list is `dropLast`/`tail`; `inv_opPopBack`/`inv_opPopFront` preserve `Inv`
  through `retireTop` and the head reset.

Stage 4':

- `grow_preserves_contentOf` — after a full-deque push, the directory rotation and
  the at-most-one-boundary-block migration preserve `contentOf` for every
  `i < len` (the zero-movement property).

The verified subset is intentionally narrower than the old contiguous fixture: the
list-level op contracts are the next increment, and the freshness gate stays
advisory until they land.

## Assumptions (managed centrally here)

- **A3' (overflow-free geometry)** — header/directory arithmetic is modelled over
  `Nat`; the u64 fields are assumed not to overflow within the checked ranges
  (`init` rejects malformed geometry and the allocator fails closed). Byte-level
  encode/decode, `Storable` bounds, page mechanics and `slot.rs` prefixes are out
  of scope.
- **A5' (slot-content opacity)** — unused slots and blocks outside the live
  directory range carry arbitrary content; reads below `len` are assumed to decode
  to entries (`Inv.occupied` in the Stage 3' scaffold). The free-list links live in
  the first 8 bytes of drained blocks and are abstracted as a base-address list in
  Stage 3'.

## Status

- Stage 1', Stage 2', Stage 3' and Stage 4': **verified sorry-free** (`lake build`
  green, 2026-11-09; no `sorry` anywhere in the fixture).
- Stage 5': planned (failure atomicity; grow-then-write ordering).
- Freshness manifest: `anchored` since plan 0379 (blocking on source drift).
  `lean-verify.yml` builds this fixture in its matrix with the same axiom discipline
  as lhm (no `sorryAx`/`Classical.choice`); the proof tree is choice-free since
  plan 0380.
- Historical: the contiguous-ring Stage 1–3 results in `REPORT.md` describe deleted
  code (ADR 0086) and are retained only as a dated record.
