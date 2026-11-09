/-
Stage-3' observable contracts for `get`/`set` and `Inv` preservation (`set`).

`get` is total: `opGet` is `contentOf` inside the live window and `none` outside,
which is exactly what `flatRead (logicalList st)` reports (`logicalList_at`).

`set` writes one routed slot without touching the header. Its list contract is
the `putAt`-shape: at the written position the observable reads the new element,
everywhere else it reads what it read before. The "everywhere else" half needs
routing injectivity on the live window, so the contract carries `Inv`: the
injectivity of the directory over live positions (`Inv.dirInj`) turns a shared
`(block base, slot)` pair into equal logical positions, and `updBlock` therefore
only disturbs the written position.

Without `Inv` the list contract is false as a statement about all states: a
window longer than the virtual capacity aliases two logical positions onto the
same physical slot. For example with `blockSlots = virtCap = 1, headOff = 0,
len = 2`, every index routes to `(dir 0, 0)`, so setting position `0` also
changes position `1` (this state violates `Inv.lenLe`). The corrected contract is
therefore stated as `opSet_spec` with the `Inv st` hypothesis; the file claims no
unconditional `putAt`-shape for `set`.
-/
import Svd.Abs.Ops

namespace Svd.Abs

variable {α : Type}

/-! ## `get` -/

/-- `get` agrees with the flattened reading of the observable list: both are the
routed `contentOf` below `len` and `none` past it. -/
theorem opGet_spec (st : DequeState α) (p : Nat) :
    opGet st p = flatRead (logicalList st) p := by
  unfold opGet
  rw [logicalList_at]

/-! ## `set`: the written position and every other position

`opSet` is a record update of `blocks` alone, so every routed lookup of
`opSet st p v` reduces to the lookup of `st` at the same routed pair, with
`blocks` replaced by the point update. -/

/-- The written position reads the new element. -/
theorem specSet_contentOf_opSet_self (st : DequeState α) (p : Nat) (v : α) :
    contentOf (opSet st p v) p = some v := by
  show updBlock st.blocks (st.dir (routeBlock st p)) (routeSlot st p) (some v)
      (st.dir (routeBlock st p)) (routeSlot st p) = some v
  exact updBlock_self

/-- Every other live position keeps its old reading: distinct logical positions
below `len` never share a routed `(block base, slot)` pair, because equal bases
are equal directory positions (`Inv.dirInj`) and equal slots then give equal
logical positions (`routed_pair_inj_len`). -/
theorem specSet_contentOf_opSet_ne {st : DequeState α} (inv : Inv st) {p q : Nat}
    {v : α} (hp : p < st.len) (hq : q < st.len) (hqp : q ≠ p) :
    contentOf (opSet st p v) q = contentOf st q := by
  have hvcPos : 0 < st.virtCap :=
    Nat.lt_of_le_of_lt (Nat.zero_le q) (Nat.lt_of_lt_of_le hq inv.lenLe)
  have hkq : routeBlock st q < st.numBlocks :=
    blockOf_lt inv.bsPos inv.vcEq (virtualIndex_lt hvcPos)
  have hkp : routeBlock st p < st.numBlocks :=
    blockOf_lt inv.bsPos inv.vcEq (virtualIndex_lt hvcPos)
  have hne : st.dir (routeBlock st q) ≠ st.dir (routeBlock st p) ∨
      routeSlot st q ≠ routeSlot st p := by
    by_cases h : st.dir (routeBlock st q) = st.dir (routeBlock st p) ∧
        routeSlot st q = routeSlot st p
    · exfalso
      have hk : routeBlock st q = routeBlock st p := inv.dirInj _ _ hkq hkp h.1
      have hs : routeSlot st q = routeSlot st p := h.2
      exact hqp (routed_pair_inj_len inv hq hp hk hs)
    · by_cases hd : st.dir (routeBlock st q) = st.dir (routeBlock st p)
      · exact Or.inr fun hs => h ⟨hd, hs⟩
      · exact Or.inl hd
  show updBlock st.blocks (st.dir (routeBlock st p)) (routeSlot st p) (some v)
      (st.dir (routeBlock st q)) (routeSlot st q)
      = st.blocks (st.dir (routeBlock st q)) (routeSlot st q)
  rw [updBlock_ne hne]

/-! ## `set`: observable-list contract -/

/-- `set` inside the live window is the point update of the observable list.
The length is unchanged, and each reading splits into the written position
(`specSet_contentOf_opSet_self` / `flatRead_putAt_self`) and every other one
(`specSet_contentOf_opSet_ne` / `flatRead_putAt_ne`).

This is the corrected form of the `set` contract: the unconditional statement
(only `p < st.len`) is false for states outside `Inv` (see the module
docstring), and routing injectivity is precisely what `Inv` supplies. The
`inv` hypothesis is therefore part of the contract, not a proof convenience. -/
theorem opSet_spec {st : DequeState α} (inv : Inv st) {p : Nat} {v : α}
    (hp : p < st.len) :
    logicalList (opSet st p v) = putAt (logicalList st) p (some v) := by
  apply eq_of_length_eq_of_flatRead
  · rw [logicalList_length, putAt_length, logicalList_length]
    rfl
  · intro q
    have hlen : (opSet st p v).len = st.len := rfl
    rw [logicalList_at, hlen]
    by_cases hq : q < st.len
    · rw [if_pos hq]
      by_cases hqp : q = p
      · rw [hqp, specSet_contentOf_opSet_self]
        rw [flatRead_putAt_self (logicalList st) p (some v)
          (by rw [logicalList_length]; exact hp)]
      · rw [specSet_contentOf_opSet_ne inv hp hq hqp]
        rw [flatRead_putAt_ne (logicalList st) p (some v) q hqp]
        rw [logicalList_at, if_pos hq]
    · rw [if_neg hq]
      have hqp : q ≠ p := by
        have hpq : p < q := Nat.lt_of_lt_of_le hp (Nat.le_of_not_lt hq)
        omega
      rw [flatRead_putAt_ne (logicalList st) p (some v) q hqp]
      rw [logicalList_at, if_neg hq]

/-! ## `Inv` preservation -/

/-- `set` preserves the invariant: it changes only `blocks`, so every geometric
conjunct is inherited unchanged, and occupancy is re-established at `p` from the
written value and elsewhere from the old occupancy plus
`specSet_contentOf_opSet_ne`. -/
theorem inv_opSet {st : DequeState α} {p : Nat} {v : α} (inv : Inv st)
    (hp : p < st.len) : Inv (opSet st p v) :=
  { bsPos := inv.bsPos
    dsPos := inv.dsPos
    numLeDir := inv.numLeDir
    vcEq := inv.vcEq
    lenLe := inv.lenLe
    headOk := inv.headOk
    dirInj := inv.dirInj
    freeDisj := inv.freeDisj
    occupied := by
      intro q hq
      by_cases hqp : q = p
      · rw [hqp]
        exact ⟨v, specSet_contentOf_opSet_self st p v⟩
      · rcases inv.occupied q hq with ⟨e, he⟩
        exact ⟨e, by rw [specSet_contentOf_opSet_ne inv hp hq hqp]; exact he⟩ }

end Svd.Abs
