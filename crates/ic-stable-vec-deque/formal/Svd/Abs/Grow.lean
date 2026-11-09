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

/-! ## Projections of `opGrow`

The growth record is a structure update, so each field of `opGrow nb st`
reduces definitionally to the corresponding expression in `st`. -/

theorem opGrow_blocks (nb : Nat) (st : DequeState α) (b o : Nat) :
    (opGrow nb st).blocks b o =
      (if b = nb ∧ o < st.headOff % st.blockSlots then
        st.blocks (st.dir (st.headOff / st.blockSlots)) o
      else st.blocks b o) := rfl

theorem opGrow_dir (nb : Nat) (st : DequeState α) (k : Nat) :
    (opGrow nb st).dir k =
      (if k < st.numBlocks then
        st.dir ((k + st.headOff / st.blockSlots) % st.numBlocks)
      else if k = st.numBlocks then nb else st.dir k) := rfl

theorem opGrow_headOff (nb : Nat) (st : DequeState α) :
    (opGrow nb st).headOff = st.headOff % st.blockSlots := rfl

theorem opGrow_blockSlots (nb : Nat) (st : DequeState α) :
    (opGrow nb st).blockSlots = st.blockSlots := rfl

theorem opGrow_virtCap (nb : Nat) (st : DequeState α) :
    (opGrow nb st).virtCap = (st.numBlocks + 1) * st.blockSlots := rfl

/-! ## Routing in arithmetic form

`routeBlock`/`routeSlot` are the block-ring decomposition of the virtual
position; these bridge lemmas expose that decomposition without unfolding the
routing definitions at every use. -/

theorem routeBlock_eq (st : DequeState α) (i : Nat) :
    routeBlock st i = ((st.headOff + i) % st.virtCap) / st.blockSlots := by
  unfold routeBlock virtualIndex blockOf
  rfl

theorem routeSlot_eq (st : DequeState α) (i : Nat) :
    routeSlot st i = ((st.headOff + i) % st.virtCap) % st.blockSlots := by
  unfold routeSlot virtualIndex slotOf
  rfl

/-! ## Block arithmetic -/

/-- Reducing a position `B * m + s` with `s < B` modulo `N * B` keeps the
offset and reduces only the block index modulo `N`. -/
theorem mulAddModBlock {B N m s : Nat} (hN : 0 < N) (hs : s < B) :
    (B * m + s) % (N * B) = B * (m % N) + s := by
  have hr : m % N < N := Nat.mod_lt m hN
  have hsplit : B * m + s = (N * B) * (m / N) + (B * (m % N) + s) := by
    have h := Nat.div_add_mod m N
    calc B * m + s = B * (N * (m / N) + m % N) + s := by rw [h]
      _ = B * (N * (m / N)) + B * (m % N) + s := by rw [Nat.mul_add]
      _ = (N * B) * (m / N) + (B * (m % N) + s) := by
            rw [← Nat.mul_assoc, Nat.mul_comm B N, Nat.add_assoc]
  have hlt : B * (m % N) + s < N * B := by
    have h2 : B * (m % N) + B = B * (m % N + 1) := by rw [Nat.mul_add, Nat.mul_one]
    have h3 : B * (m % N + 1) ≤ B * N := Nat.mul_le_mul_left B (by omega)
    have h4 : B * N = N * B := Nat.mul_comm B N
    omega
  rw [hsplit, Nat.add_comm ((N * B) * (m / N)), Nat.add_mul_mod_self_left,
    Nat.mod_eq_of_lt hlt]

/-- After growth, every routed reading below the old `len` is preserved:
non-boundary elements keep their slot (via directory rotation), and boundary
elements migrate into the freshly acquired block. This is the zero-movement
property at block granularity. -/
theorem grow_preserves_contentOf {st : DequeState α} (inv : Inv st)
    (hfull : st.len = st.virtCap) (hfresh : ∀ k, k < st.numBlocks →
      st.dir k ≠ nb) (i : Nat) (hi : i < st.len) :
    contentOf (opGrow nb st) i = contentOf st i := by
  have hB : 0 < st.blockSlots := inv.bsPos
  have hiV : i < st.virtCap := by rw [← hfull]; exact hi
  have hiNB : i < st.numBlocks * st.blockSlots := by rw [← inv.vcEq]; exact hiV
  have hNpos : 0 < st.numBlocks := by
    rcases Nat.eq_zero_or_pos st.numBlocks with h | h
    · rw [h] at hiNB ⊢
      simp at hiNB
    · exact h
  have hhead : st.headOff < st.virtCap := by
    rcases inv.headOk with ⟨hlen, -⟩ | ⟨-, hh⟩
    · rw [hlen] at hi
      exact absurd hi (Nat.not_lt_zero i)
    · exact hh
  have hheadNB : st.headOff < st.numBlocks * st.blockSlots := by
    rw [← inv.vcEq]; exact hhead
  have hrot : st.headOff / st.blockSlots < st.numBlocks :=
    Nat.div_lt_of_lt_mul (by rw [Nat.mul_comm]; exact hheadNB)
  have hbdy : st.headOff % st.blockSlots < st.blockSlots := Nat.mod_lt _ hB
  -- The window position after the front is rebased to `headOff % B`; it does
  -- not wrap in the enlarged capacity.
  have hxlt : st.headOff % st.blockSlots + i < (st.numBlocks + 1) * st.blockSlots := by
    rw [Nat.add_mul, Nat.one_mul]
    omega
  have hqle : (st.headOff % st.blockSlots + i) / st.blockSlots ≤ st.numBlocks := by
    have h : (st.headOff % st.blockSlots + i) / st.blockSlots < st.numBlocks + 1 := by
      apply Nat.div_lt_of_lt_mul
      rw [Nat.mul_comm]
      exact hxlt
    omega
  -- Split `headOff + i` as `B * (rot + q) + s`, where `q`/`s` are the block
  -- and slot of the rebased window position.
  have hdecomp : st.headOff + i
      = st.blockSlots * (st.headOff / st.blockSlots
          + (st.headOff % st.blockSlots + i) / st.blockSlots)
        + (st.headOff % st.blockSlots + i) % st.blockSlots := by
    have h1 : st.blockSlots * (st.headOff / st.blockSlots)
        + st.headOff % st.blockSlots = st.headOff :=
      Nat.div_add_mod st.headOff st.blockSlots
    have h2 : st.blockSlots * ((st.headOff % st.blockSlots + i) / st.blockSlots)
        + (st.headOff % st.blockSlots + i) % st.blockSlots
        = st.headOff % st.blockSlots + i :=
      Nat.div_add_mod (st.headOff % st.blockSlots + i) st.blockSlots
    rw [Nat.mul_add]
    omega
  -- Old routing: the same slot offset, block index rotated by `rot`.
  have holdBlock : ((st.headOff + i) % st.virtCap) / st.blockSlots
      = (st.headOff / st.blockSlots
          + (st.headOff % st.blockSlots + i) / st.blockSlots) % st.numBlocks := by
    rw [show st.virtCap = st.numBlocks * st.blockSlots from inv.vcEq, hdecomp]
    rw [mulAddModBlock (N := st.numBlocks)
      (m := st.headOff / st.blockSlots
        + (st.headOff % st.blockSlots + i) / st.blockSlots)
      (s := (st.headOff % st.blockSlots + i) % st.blockSlots) hNpos (Nat.mod_lt _ hB)]
    rw [mulAddDiv hB, Nat.div_eq_of_lt (Nat.mod_lt _ hB), Nat.add_zero]
  have holdSlot : ((st.headOff + i) % st.virtCap) % st.blockSlots
      = (st.headOff % st.blockSlots + i) % st.blockSlots := by
    rw [show st.virtCap = st.numBlocks * st.blockSlots from inv.vcEq, hdecomp]
    rw [mulAddModBlock (N := st.numBlocks)
      (m := st.headOff / st.blockSlots
        + (st.headOff % st.blockSlots + i) / st.blockSlots)
      (s := (st.headOff % st.blockSlots + i) % st.blockSlots) hNpos (Nat.mod_lt _ hB)]
    rw [mulAddMod, Nat.mod_eq_of_lt (Nat.mod_lt _ hB)]
  -- New routing: no wrap at all.
  have hnewBlock : (((opGrow nb st).headOff + i) % (opGrow nb st).virtCap)
      / (opGrow nb st).blockSlots
      = (st.headOff % st.blockSlots + i) / st.blockSlots := by
    rw [opGrow_headOff, opGrow_virtCap, opGrow_blockSlots, Nat.mod_eq_of_lt hxlt]
  have hnewSlot : (((opGrow nb st).headOff + i) % (opGrow nb st).virtCap)
      % (opGrow nb st).blockSlots
      = (st.headOff % st.blockSlots + i) % st.blockSlots := by
    rw [opGrow_headOff, opGrow_virtCap, opGrow_blockSlots, Nat.mod_eq_of_lt hxlt]
  unfold contentOf
  simp only [routeBlock_eq, routeSlot_eq]
  rw [hnewBlock, hnewSlot, holdBlock, holdSlot]
  rw [opGrow_dir]
  by_cases hq : (st.headOff % st.blockSlots + i) / st.blockSlots < st.numBlocks
  · -- The rebased position still lies in a rotated live block: the directory
    -- rotation sends it to the same base as the old routing.
    rw [if_pos hq]
    rw [show (st.headOff % st.blockSlots + i) / st.blockSlots
          + st.headOff / st.blockSlots
        = st.headOff / st.blockSlots
          + (st.headOff % st.blockSlots + i) / st.blockSlots
        from Nat.add_comm _ _]
    rw [opGrow_blocks, if_neg (by
      rintro ⟨hbn, -⟩
      exact hfresh _ (Nat.mod_lt _ hNpos) hbn)]
  · -- The rebased position reaches the new block: its offset is below the
    -- boundary, so the migrated slots supply exactly the old reading.
    have hqeq : (st.headOff % st.blockSlots + i) / st.blockSlots = st.numBlocks := by
      omega
    have hslt : (st.headOff % st.blockSlots + i) % st.blockSlots
        < st.headOff % st.blockSlots := by
      have hx := Nat.div_add_mod (st.headOff % st.blockSlots + i) st.blockSlots
      rw [hqeq] at hx
      have hi' : i < st.blockSlots * st.numBlocks := by rw [Nat.mul_comm]; exact hiNB
      omega
    have hrotMod : (st.headOff / st.blockSlots + st.numBlocks) % st.numBlocks
        = st.headOff / st.blockSlots := by
      have h := Nat.add_mul_mod_self_left (st.headOff / st.blockSlots) st.numBlocks 1
      rw [Nat.mul_one] at h
      rw [h, Nat.mod_eq_of_lt hrot]
    rw [if_neg hq, if_pos hqeq, opGrow_blocks, if_pos (by exact ⟨rfl, hslt⟩)]
    rw [hqeq, hrotMod]

end Svd.Abs
