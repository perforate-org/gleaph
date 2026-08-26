# Audit Report — ic-stable-vec-deque (Lean transcription findings)

Anchor timestamp: 2026-08-26 00:01:33 UTC +0000
Target revision: git `d80cc4c603e2dd50bd26ee77e2679b13f70f6dc0` (`vec_deque.rs`
clean in the working tree)

Findings from hand-transcribing `src/vec_deque.rs` into
`crates/ic-stable-vec-deque/formal/`. Line numbers refer to `vec_deque.rs` at the
target revision. Nothing here is a defect report against production behavior;
items are observations about contracts, redundancy, and proof-relevant subtleties.

## Findings

### 1. Rotation permutes all capacity slots, including unused ones

`grow_if_full`'s cycle rotation moves *every* physical slot exactly once
(L319-L349), including slots never written (physical slots in `[len, cap)`).
Consequence for the model: no default byte image may be assumed for unused slots
(scope assumption A5). The abstract state therefore uses a total function
`slots : Nat → Option α` with arbitrary values outside the occupied window.

**Update 2026-08-26**: an attempt to remove the rotation ([ADR 0085]) was
**rejected by erratum** before implementation: relocating wrapped elements on
capacity growth is structurally unavoidable for a contiguous ring (a wrapped
element's new route is `slot + old_capacity`, not `slot`). [ADR 0086] now
proposes replacing the layout with a segmented block-ring, which removes
the rotation entirely. This finding remains an accurate description of the V1
code at the target revision and is retained as a record of the superseded
implementation.

### 2. Growth arithmetic relies on fail-closed growth before the byte math

`new_cap = cap.saturating_mul(2).max(len.saturating_add(1))` (L309) is saturated,
but the subsequent byte computation `DATA_OFFSET + new_cap * slot` (L311) is not.
If growth were skipped after saturation, the offset computation could overflow in
release builds. Unreachable today because `grow_memory_to_at_least_bytes(need)`
fails closed first (L312) — but only because `need` itself was computed with the
saturated `new_cap`; the safety of L311 rests on A3. Relevant to stage 4+; not
exercised by stages 1–3.

### 3. `init`'s empty-ring head check is partially redundant, harmlessly

The dedicated check `len == 0 && head != 0 → InvalidLayout` (L216-L218) is
implied by the `cap == 0` branch (L208-L211) whenever `cap == 0`, and by nothing
else when `cap > 0` — a persisted header with `cap > 0`, `len = 0`, `head ≠ 0` is
rejected only by this conjunct. It is therefore not dead code, but it duplicates
the `cap == 0` case. Modeled verbatim as `HeaderValid.emptyHead`; harmless.

### 4. `push_front` computes the new front as `(head + cap - 1) % cap`

vec_deque.rs L466. In `u64` this expression cannot underflow because `cap ≥ 1`
whenever the deque is live (`debug_assert!(cap > 0)` at L464; `init` enforces it).
In the model the positivity is carried explicitly (`Inv.capPos`). The written
slot coincides with the new front, so logical position 0 reads the new entry and
every old reading shifts one position later
(`contentOf_opPushFront_zero` / `_succ`).

### 5. `pop_back` / `pop_front` reset `head` to 0 when they empty the ring

L497-L499 and L525-L526. This is what keeps live states inside the layout
contract's empty-deque check (`head = 0` when `len = 0`) and is part of `Inv`
preservation (`inv_popBack` / `inv_popFront`). Note that `push_front` on an empty
deque deliberately leaves `head = cap - 1 ≠ 0`: the empty-head discipline holds
for states that *become* empty, not for all states that were empty.

### 6. `pop_front` reads raw `head` without modular reduction

L522 reads `slot_byte_offset(head)` directly rather than routing through
`physical_index`. This is in-range precisely because reachable states satisfy
`head < cap` (modeled as `Inv.headLt`). The model keeps the raw read
(`popFrontNonEmpty`) and carries `Inv` as the hypothesis making it safe — an
example of the invariant doing real work in the audit story.

### 7. `pop_front`'s `cap > 1` guard is redundant under the layout contract

L527-L529 advances the front only when `cap > 1`. Under `len ≤ cap`, any state
reaching this branch has new length `n ≥ 1`, hence old length `≥ 2`, hence
`cap ≥ 2`: the guard's false branch is unreachable in live states. The model
transcribes the guard faithfully and proves preservation through every branch
(`inv_popFront` derives `False` from the dead branch via `hle`).

### 8. `set`'s panic bound is load-bearing for the abstract invariant

`set` asserts `index < len` (L407) before writing. An out-of-window write would
route to `(head + p) % cap`, which at a full ring (`len = cap`) aliases logical
slot 0's slot — corrupting element 0 while leaving `len` unchanged. The stage-3
model therefore requires `p < len` as a hypothesis of both the list-spec contract
(`opSet_spec`) and preservation (`inv_set`); the injection lemma cannot rule out
the aliasing without it.

### 9. `grow_if_full`'s no-op window makes the push contracts exact

`grow_if_full` returns immediately when `len < cap` (L302-L304). The stage-3 push
contracts carry exactly that condition as a hypothesis, so they describe the Rust
code path with no gap: under `len < cap`, `push_back`/`push_front` are fully
captured by the modeled writes. The grow path itself is stage-4 material and is
not approximated here.

**Update 2026-08-26**: the grow path's cost structure (rotation O(cap × slot),
page delta ∝ size) is the crate's main per-call boundedness gap. [ADR 0085]
(rotation-free doubling) was **rejected by erratum** (see finding 1); [ADR 0086]
resolves the gap by replacing the layout with a segmented block-ring whose
per-call work is strictly bounded.

## Verification status

`lake build` green at the anchor timestamp; all headline theorems are
`sorry`-free per the `#print axioms` guards in `Svd.lean` (dependencies limited to
`propext`, `Quot.sound`, `Classical.choice`). Stage 4 (grow linearization) and
stage 5 (failure atomicity) remain open; see SCOPE.md Status.
