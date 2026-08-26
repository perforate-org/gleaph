/-
Growth operation (`grow_for_push`, vec_deque.rs): when the deque is full,
one block is acquired (from the free list or freshly allocated), the directory
is rotated by `headOff / B` positions and rebased so the front sits below one
block, and at most one block of boundary slots migrates into the newly acquired
block. No existing element is relocated beyond those boundary slots.
-/
import Svd.Abs.Ops

namespace Svd.Abs

variable {α : Type}

/-- Growth step for a full deque: acquire base `nb` (fresh or recycled),
rotate directory entries `[0, numBlocks)` by `headOff / B` positions, rebase
the front, migrate boundary slots into the new block, and grow `virtCap` by
exactly one block. Requires `numBlocks < dirSlots` (directory doubling runs
first in Rust) and `len = virtCap` (full). -/
def opGrow (nb : Nat) (st : DequeState α) : DequeState α :=
  let rot := st.headOff / st.blockSlots
  let bdy := st.headOff % st.blockSlots
  { st with
    blocks := fun b o =>
      if b = nb ∧ o < bdy then st.blocks (st.dir rot) o else st.blocks b o
    dir := fun k =>
      if k < st.numBlocks then st.dir ((k + rot) % st.numBlocks)
      else if k = st.numBlocks then nb
      else st.dir k
    headOff := bdy
    numBlocks := st.numBlocks + 1
    virtCap := (st.numBlocks + 1) * st.blockSlots
    free := st.free.erase nb }

/-- After growth, every routed reading below the old `len` is preserved:
non-boundary elements keep their slot (via directory rotation), and boundary
elements migrate into the freshly acquired block. This is the zero-movement
property at block granularity. -/
theorem grow_preserves_contentOf {st : DequeState α} (inv : Inv st)
    (hfull : st.len = st.virtCap) (hfresh : ∀ k, k < st.numBlocks →
      st.dir k ≠ nb) (i : Nat) (hi : i < st.len) :
    contentOf (opGrow nb st) i = contentOf st i := by
  sorry

end Svd.Abs
