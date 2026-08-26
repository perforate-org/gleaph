# Formal Verification Scope — ic-stable-vec-deque

Anchor timestamp: 2026-08-26 00:01:33 UTC +0000
Target revision: git `d80cc4c603e2dd50bd26ee77e2679b13f70f6dc0` (`vec_deque.rs` is
clean in the working tree at this revision)

## Mode

**Audit mode against an existing implementation, run as a permanent fixture.**
`formal/` is a standing Lean project that is re-checked with `lake build` whenever
the deque changes. Findings are recorded in `REPORT.md`, refreshed alongside the
proofs.

## Location and form

- Lean lake project: `crates/ic-stable-vec-deque/formal/`
- Independent from the Cargo workspace (no `Cargo.toml`; the workspace lists
  members explicitly, so this directory is invisible to Cargo).
- Toolchain: Lean 4 core only (`leanprover/lean4:v4.33.1`). No Mathlib, no Std.
- Run locally: `cd crates/ic-stable-vec-deque/formal && lake build`.
- No CI wiring yet (repository has no `.github/workflows/`).

## Method

Hand-written Lean model plus proofs, adapted from the methodology of
`crates/ic-stable-linear-hash-map/formal/` (`Lhm_formal`) without importing its
code. Rust semantics are transcribed faithfully into Lean definitions; each
definition cites the source file and line range it mirrors. Automated
Rust-to-Lean extraction was considered and intentionally not used, for the same
reasons recorded in the LHM scope: stable-memory I/O is impure.

## Component breakdown (audit layers)

1. **Ring arithmetic** — `physical_index head logical cap = (head + logical) % cap`
   over an arbitrary positive capacity (the V1 ring is not power-of-two sized).
2. **Header validity** — the layout checks of `VecDeque::init` (magic / version /
   element bounds assumed as hypotheses) and validity of the fresh header written
   by `VecDeque::new`.
3. **Abstract list-spec layer** — `DequeState` (`slots : Nat → Option α` plus
   `len/head/capacity`), observable reading list, list-spec contracts for
   get/set/push_back/push_front/pop_back/pop_front, and preservation of `Inv`.

Stages 1–3 are **in scope now** (see Status). Stages 4–5 are planned follow-up
work listed here so the roadmap survives agent handoffs:
- **Stage 4 (grow linearization)** — `grow_if_full`'s cycle rotation implements
  `content'(p) = old((p + head) % cap)` with `head = 0` afterwards. GCD cycle
  decomposition over all `cap` slots; the hardest remaining obligation.
- **Boundedness → ADR 0086 (proposed)** — per-call cost of
  `push_back`/`push_front` on a full ring is O(len × slot_size) of element
  I/O plus a memory-grow whose page delta scales with the current data region,
  so pushes are only amortized-bounded ([ADR 0085] proposed removing the
  rotation and was **rejected by erratum**: relocating wrapped elements on
  capacity growth is structurally unavoidable for a contiguous ring).
  [ADR 0086] (`design/adr/0086-stable-vec-deque-segmented-block-ring.md`)
  resolves this by replacing the layout with a segmented block-ring (fresh
  state, no migration; `LAYOUT_VERSION` stays 1). Until that lands, this
  document describes the current contiguous-ring implementation; the formal
  stage plan will be restated for the new layout as specified in ADR 0086
  §Formal-layer impact.
- **Stage 5 (failure atomicity)** — after a successful `grow_if_full`, all writes
  land inside the grown extent, hence cannot fail (`safe_write` skips grow);
  therefore `Err ⇒ state unchanged`.

## Properties verified (stages 1–3 contract)

Stage 1 (ring arithmetic):
- P1-extent: under `0 < cap`, every logical index routes to a physical slot
  `< cap` (`Svd.physicalIndex_lt_cap`, `.physicalIndex_in_extent`).
- P1-injection: on the occupied window `[0, len)` with `len ≤ cap`, routing is
  injective — distinct logical positions never share a slot
  (`Svd.physicalIndex_injective_on_window`). Holds for arbitrary positive
  capacities, not only powers of two.

Stage 2 (header validity):
- P2-validity: `HeaderValid h slotSize memSize` mirrors every arithmetic check of
  `VecDeque::init`: `cap = 0 → len = 0 ∧ head = 0`; `cap > 0 → len ≤ cap ∧
  head < cap`; `len = 0 → head = 0`; `64 + cap * slotSize ≤ memSize`.
- P4-analog: the fresh header written by `new` satisfies `HeaderValid`
  (`Svd.initialHeader_valid`).

Stage 3 (abstract layer), all under `Inv` = {`0 < cap`, `len ≤ cap`, `head < cap`,
windowed occupancy}:
- P3-get: `get p` returns exactly logical position `p`'s routed reading, `none`
  out of range (`Svd.Abs.opGet_spec`).
- P3-set: with `p < len` (Rust asserts it), the observable list becomes the old
  list with position `p` overwritten by `some v` (`Svd.Abs.opSet_spec`).
- P3-pushBack: with `len < cap` (so `grow_if_full` is a no-op), the observable
  list gains `some v` as its new last element (`Svd.Abs.opPushBack_spec`).
- P3-pushFront: under the same precondition, the observable list gains `some v`
  as its new first element and every old reading shifts one position later
  (`Svd.Abs.opPushFront_spec`, `.contentOf_opPushFront_succ`).
- P3-popBack: returns the last entry; the new observable list is the old one with
  its final element removed (`Svd.Abs.opPopBack_value`, `.opPopBack_state`).
- P3-popFront: returns the first entry; the new observable list reads the old one
  one position later everywhere (`Svd.Abs.opPopFront_value`,
  `.opPopFront_state`).
- P3-preservation: each of set/pushBack/pushFront/popBack/popFront preserves
  `Inv` (`Svd.Abs.inv_set`, `.inv_pushBack`, `.inv_pushFront`, `.inv_popBack`,
  `.inv_popFront`).

Every headline theorem above is guarded by `#print axioms` in `Svd.lean`; the
build output must show no `sorryAx`.

## Assumptions (managed centrally here)

- A3 (fail-closed growth math): `capacity.saturating_mul(2)` saturation followed
  by the non-saturating byte computation
  `DATA_OFFSET + new_cap * slot_size` would wrap and release-wrap; declared
  unreachable because `grow_memory_to_at_least_bytes` fails closed first.
  Relevant only to stage 4+.
- A5 (slot-content opacity): unused slots, out-of-ring slots, and never-written
  bytes carry arbitrary content. Grow-time rotation permutes *all* `cap` slots
  including unused ones (vec_deque.rs L319-L349), so no default byte image may be
  assumed for them. Additionally, the byte layer is assumed consistent: `len`
  counts real entries, so every routed read below `len` decodes to an entry
  (`Inv.occupied`); `Storable` encode/decode correctness is out of scope.
- Element encoding, `slot.rs` length prefixes, memory-page mechanics, and `Iter`
  plumbing are out of scope.

## Status

- Stages 1–3: verified sorry-free against the **contiguous-ring implementation**
  (`lake build` green, 2026-08-26). **That implementation was replaced the same
  day** by the segmented block-ring per [ADR 0086] ("As built" amendments
  included), so this document currently describes superseded code: every stage
  must be restated and re-run against the block-ring layout before its results
  apply again.
- Stage 4 / boundedness: the block-ring removes rotation entirely; the planned
  GCD-cycle obligation is obsolete. New obligations per ADR 0086 §Formal-layer
  impact: block-routing arithmetic, V2-field header validity
  (`DATA_OFFSET = 128`), directory/free-list well-formedness in `Inv`, and the
  append-one-block preservation lemma.
- Stage 5 (failure atomicity): planned follow-up, not attempted.
