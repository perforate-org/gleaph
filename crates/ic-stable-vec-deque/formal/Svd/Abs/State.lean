/-
Stage-3' abstract state for the segmented block-ring layout (ADR 0086 as
built). The semantic mirror of the persisted deque:

- `blocks base off` is the entry stored at byte offset `base + off·slotSize`;
  `base` identifies a block. Unused offsets and blocks outside the directory's
  live range are unconstrained (A5-analog: stale bytes never leak into reads).
- `dir k` is the base address recorded in directory position `k` for
  `k < numBlocks`; entries beyond `numBlocks` are scratch.
- `headOff`, `virtCap = numBlocks · B`, `len`, `dirSlots`, `blockSlots = B`
  mirror the header; `free` is the intrusive free-block list, abstracted as a
  list of base addresses (head first).

The invariant bundle `Inv` carries the validated-header geometry plus directory
injectivity on live positions, free/live disjointness, and windowed occupancy.
-/
import Svd.Basic

namespace Svd.Abs

variable {α : Type}

/-! ## Abstract deque state -/

structure DequeState (α : Type) where
  blocks : Nat → Nat → Option α
  dir : Nat → Nat
  dirSlots : Nat
  numBlocks : Nat
  blockSlots : Nat
  headOff : Nat
  virtCap : Nat
  len : Nat
  free : List Nat

/-! ## Routing -/

/-- Directory position of logical index `i`. -/
def routeBlock (st : DequeState α) (i : Nat) : Nat :=
  ((st.headOff + i) % st.virtCap) / st.blockSlots

/-- Slot offset inside that block. -/
def routeSlot (st : DequeState α) (i : Nat) : Nat :=
  ((st.headOff + i) % st.virtCap) % st.blockSlots

/-- Entry observed at logical position `i`: two-level lookup through the
directory and the block contents. -/
def contentOf (st : DequeState α) (i : Nat) : Option α :=
  st.blocks (st.dir (routeBlock st i)) (routeSlot st i)

/-! ## Block-content update primitive -/

/-- Point update of the two-level content function at one `(block, offset)`
pair. This mirrors a single `slot::write_slot`. -/
def updBlock (g : Nat → Nat → Option α) (b o : Nat) (x : Option α) :
    Nat → Nat → Option α :=
  fun b' o' => if b' = b ∧ o' = o then x else g b' o'

theorem updBlock_self {g : Nat → Nat → Option α} {b o : Nat} {x : Option α} :
    updBlock g b o x b o = x := by
  simp [updBlock]

theorem updBlock_ne {g : Nat → Nat → Option α} {b o : Nat} {x : Option α}
    {b' o' : Nat} (h : b' ≠ b ∨ o' ≠ o) :
    updBlock g b o x b' o' = g b' o' := by
  simp only [updBlock]
  rcases h with h | h
  · simp [h]
  · simp [h]

/-! ## The stage-3' invariant bundle -/

/-- Everything stage 3 assumes and preserves for the block ring:
positive geometry from the validated header (`init`), virtual capacity equal to
the tracked block count times the block size, occupied extent within capacity,
front position in range (or zero when empty), injectivity of the directory over
live positions, disjointness of recycled bases from live ones, and occupancy of
every routed read below `len`. -/
structure Inv (st : DequeState α) : Prop where
  bsPos : 0 < st.blockSlots
  dsPos : 0 < st.dirSlots
  numLeDir : st.numBlocks ≤ st.dirSlots
  vcEq : st.virtCap = st.numBlocks * st.blockSlots
  lenLe : st.len ≤ st.virtCap
  headOk : (st.len = 0 ∧ st.headOff = 0) ∨ (0 < st.len ∧ st.headOff < st.virtCap)
  dirInj : ∀ k₁ k₂, k₁ < st.numBlocks → k₂ < st.numBlocks →
    st.dir k₁ = st.dir k₂ → k₁ = k₂
  freeDisj : ∀ b, b ∈ st.free → ∀ k, k < st.numBlocks → st.dir k ≠ b
  occupied : ∀ p, p < st.len → ∃ e, contentOf st p = some e

end Svd.Abs
