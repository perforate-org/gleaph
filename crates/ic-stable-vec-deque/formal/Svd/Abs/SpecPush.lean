/-
Stage-3' observable contracts for `push_back`/`push_front` (non-full path) and
`Inv` preservation.

`opPushBack st v` writes the routed slot of logical position `st.len` and
increments `len`; `opPushFront st v` moves the front to
`prevHead st = (headOff + virtCap - 1) % virtCap`, writes that routed slot and
increments `len`. Neither touches the directory, the block count, the virtual
capacity or the free list, so the geometry/free-list conjuncts of `Inv` carry
over unchanged; the work is routing (`routeBlock`/`routeSlot`) and occupancy.

The observable-list contracts carry `Inv`. Without it they are false: a
directory that is not injective on live positions can alias two distinct logical
positions onto the same physical slot. Concretely (α = Bool), take

  blockSlots = 2, numBlocks = 2, virtCap = 4, headOff = 0, len = 2, dir ≡ 0,
  blocks 0 0 = blocks 0 1 = some true.

Then `len < virtCap` holds and `logicalList st = [some true, some true]`, but
`opPushBack st false` routes position 2 to `(base 0, slot 0)` — the slot read at
logical position 0 — so `logicalList (opPushBack st false) =
[some false, some true, some false]`, whereas `logicalList st ++ [some false] =
[some true, some true, some false]`. `push_front` fails the same way on that
state (it writes `(base 0, slot 1)`, read at position 1). This is the same
aliasing that forces the `set` contract in `Svd/Abs/SpecSet.lean` to carry
`Inv`; `Inv.dirInj` is what separates a shared base address into equal directory
positions, and `routed_pair_inj_len` then separates the logical positions. The
unconditional forms are therefore not claimed here; the corrected statements
keep the names `opPushBack_spec` / `opPushFront_spec` but take `Inv` as a
hypothesis.
-/
import Svd.Abs.Ops

namespace Svd.Abs

variable {α : Type}

/-! ## Projections of the push operations

Both operations are record updates, so the untouched fields reduce
definitionally, and the blocked content of the result is the point update
applied to the routed pair of `st` (`push_back`) or of the *new* front
(`push_front`). -/

theorem specPush_routeBlock_eq (st : DequeState α) (i : Nat) :
    routeBlock st i = ((st.headOff + i) % st.virtCap) / st.blockSlots := rfl

theorem specPush_routeSlot_eq (st : DequeState α) (i : Nat) :
    routeSlot st i = ((st.headOff + i) % st.virtCap) % st.blockSlots := rfl

theorem specPush_opPushBack_len (st : DequeState α) (v : α) :
    (opPushBack st v).len = st.len + 1 := rfl

theorem specPush_opPushFront_len (st : DequeState α) (v : α) :
    (opPushFront st v).len = st.len + 1 := rfl

/-- `push_back` writes only the routed pair of `st.len`; every lookup of the
result is the point update applied at the old routing. -/
theorem specPush_opPushBack_contentOf (st : DequeState α) (v : α) (p : Nat) :
    contentOf (opPushBack st v) p =
      updBlock st.blocks (st.dir (routeBlock st st.len)) (routeSlot st st.len) (some v)
        (st.dir (routeBlock st p)) (routeSlot st p) := rfl

/-- `push_front` writes the routed pair of the new front (`prevHead`) and keeps
every other coordinate of the routing unchanged. -/
theorem specPush_opPushFront_contentOf (st : DequeState α) (v : α) (p : Nat) :
    contentOf (opPushFront st v) p =
      updBlock st.blocks (st.dir (prevHead st / st.blockSlots))
        (prevHead st % st.blockSlots) (some v)
        (st.dir (routeBlock (opPushFront st v) p))
        (routeSlot (opPushFront st v) p) := rfl

/-! ## Routing arithmetic -/

/-- Distinct virtual positions never share a `(block index, slot)` pair, because
the pair reconstructs the position. -/
theorem specPush_pair_ne_of_vpos_ne {B r₁ r₂ : Nat} (h : r₁ ≠ r₂) :
    ¬ (r₁ / B = r₂ / B ∧ r₁ % B = r₂ % B) := by
  rintro ⟨hk, hs⟩
  have e1 := Nat.div_add_mod r₁ B
  have e2 := Nat.div_add_mod r₂ B
  rw [hk, hs] at e1
  exact h (e1.symm.trans e2)

/-- Shifting by the head offset is injective on positions below `virtCap`, so two
in-window positions that differ stay at different virtual positions. -/
theorem specPush_vpos_ne_of_lt {H V a b : Nat} (hb : b < V) (ha : a < b) :
    (H + a) % V ≠ (H + b) % V := by
  intro habs
  have hab : (a + H) % V = (b + H) % V := by
    rw [Nat.add_comm a H, Nat.add_comm b H]
    exact habs
  have := Svd.addModInjOnRange (N := V) (w := H) (u := a) (v := b) (by omega) hb hab
  omega

/-- The front position of `Inv` is always below the virtual capacity: either the
deque is empty and the front is `0`, or it is non-empty and `Inv.headOk` bounds
it. -/
theorem specPush_head_lt {st : DequeState α} (inv : Inv st) (hV : 0 < st.virtCap) :
    st.headOff < st.virtCap := by
  rcases inv.headOk with ⟨-, h0⟩ | ⟨-, hH⟩
  · rw [h0]; exact hV
  · exact hH

/-- `prevHead` is in range: it is a residue modulo `virtCap`. -/
theorem specPush_prevHead_lt (st : DequeState α) (hV : 0 < st.virtCap) :
    prevHead st < st.virtCap :=
  Nat.mod_lt _ hV

/-- `prevHead` is the front moved one step backward, split at `headOff = 0`
(where it wraps to `virtCap - 1`). -/
theorem specPush_prevHead_mod (st : DequeState α) (hV : 0 < st.virtCap)
    (hH : st.headOff < st.virtCap) :
    prevHead st = if st.headOff = 0 then st.virtCap - 1 else st.headOff - 1 := by
  unfold prevHead
  rcases Nat.eq_zero_or_pos st.headOff with h0 | hpos
  · rw [h0, if_pos rfl, Nat.zero_add, Nat.mod_eq_of_lt (by omega)]
  · rw [if_neg (by omega : ¬ st.headOff = 0)]
    rw [show st.headOff + st.virtCap - 1 = (st.headOff - 1) + st.virtCap from by omega,
      Nat.add_mod_right, Nat.mod_eq_of_lt (by omega)]

/-- The new front position followed by `m ≥ 1` logical steps lands on the old
virtual position of `m - 1`: rebasing the front by `virtCap - 1` is the same as
shifting the window by one. -/
theorem specPush_prevHead_add_mod (st : DequeState α) {m : Nat}
    (hV : 0 < st.virtCap) (hH : st.headOff < st.virtCap) (hm : 0 < m) :
    (prevHead st + m) % st.virtCap = (st.headOff + (m - 1)) % st.virtCap := by
  rw [specPush_prevHead_mod st hV hH]
  rcases Nat.eq_zero_or_pos st.headOff with h0 | hpos
  · rw [if_pos h0, h0, Nat.zero_add]
    rw [show st.virtCap - 1 + m = (m - 1) + st.virtCap from by omega]
    rw [Nat.add_mod_right]
  · rw [if_neg (by omega : ¬ st.headOff = 0)]
    rw [show st.headOff - 1 + m = st.headOff + (m - 1) from by omega]

/-- The new front position is never the old routing of a live position: read
`prevHead` as `(virtCap - 1) + headOff` and cancel the common shift. -/
theorem specPush_prevHead_ne {H V q : Nat} (hV : 0 < V) (hq : q + 1 < V) :
    (H + q) % V ≠ (H + V - 1) % V := by
  intro h
  have hqV : q < V := by omega
  have hv1 : V - 1 < V := by omega
  have h' : (q + H) % V = ((V - 1) + H) % V := by
    rw [Nat.add_comm q H]
    rw [show (V - 1) + H = H + V - 1 from by omega]
    exact h
  have := Svd.addModInjOnRange (N := V) (w := H) (u := q) (v := V - 1) hqV hv1 h'
  omega

theorem specPush_opPushFront_routeBlock_zero (st : DequeState α) (v : α)
    (hV : 0 < st.virtCap) :
    routeBlock (opPushFront st v) 0 = prevHead st / st.blockSlots := by
  rw [specPush_routeBlock_eq]
  show ((prevHead st + 0) % st.virtCap) / st.blockSlots = prevHead st / st.blockSlots
  rw [Nat.add_zero, Nat.mod_eq_of_lt (specPush_prevHead_lt st hV)]

theorem specPush_opPushFront_routeSlot_zero (st : DequeState α) (v : α)
    (hV : 0 < st.virtCap) :
    routeSlot (opPushFront st v) 0 = prevHead st % st.blockSlots := by
  rw [specPush_routeSlot_eq]
  show ((prevHead st + 0) % st.virtCap) % st.blockSlots = prevHead st % st.blockSlots
  rw [Nat.add_zero, Nat.mod_eq_of_lt (specPush_prevHead_lt st hV)]

/-- After `push_front`, logical position `q + 1` routes exactly where logical
position `q` routed before the push. -/
theorem specPush_opPushFront_routeBlock_succ (st : DequeState α) (v : α) (q : Nat)
    (hV : 0 < st.virtCap) (hH : st.headOff < st.virtCap) :
    routeBlock (opPushFront st v) (q + 1) = routeBlock st q := by
  rw [specPush_routeBlock_eq, specPush_routeBlock_eq]
  show ((prevHead st + (q + 1)) % st.virtCap) / st.blockSlots
      = ((st.headOff + q) % st.virtCap) / st.blockSlots
  rw [specPush_prevHead_add_mod st hV hH (by omega : 0 < q + 1)]
  rw [show q + 1 - 1 = q from by omega]

theorem specPush_opPushFront_routeSlot_succ (st : DequeState α) (v : α) (q : Nat)
    (hV : 0 < st.virtCap) (hH : st.headOff < st.virtCap) :
    routeSlot (opPushFront st v) (q + 1) = routeSlot st q := by
  rw [specPush_routeSlot_eq, specPush_routeSlot_eq]
  show ((prevHead st + (q + 1)) % st.virtCap) % st.blockSlots
      = ((st.headOff + q) % st.virtCap) % st.blockSlots
  rw [specPush_prevHead_add_mod st hV hH (by omega : 0 < q + 1)]
  rw [show q + 1 - 1 = q from by omega]

/-! ## The written pair is outside the old window

Both pushes write at a routed pair that no live position uses, so the point
update leaves every old reading in place. `Inv.dirInj` identifies two equal base
addresses with equal directory positions; `specPush_pair_ne_of_vpos_ne` then
turns the virtual-position inequality into a pair inequality. -/

theorem specPush_pushBack_pair_ne {st : DequeState α} (inv : Inv st)
    (h : st.len < st.virtCap) {p : Nat} (hp : p < st.len) :
    ¬ (st.dir (routeBlock st p) = st.dir (routeBlock st st.len) ∧
       routeSlot st p = routeSlot st st.len) := by
  have hV : 0 < st.virtCap := by omega
  have hvpos : (st.headOff + p) % st.virtCap ≠ (st.headOff + st.len) % st.virtCap :=
    specPush_vpos_ne_of_lt h hp
  rintro ⟨hb, hs⟩
  have hbp : routeBlock st p < st.numBlocks :=
    Svd.blockOf_lt inv.bsPos inv.vcEq (Nat.mod_lt _ hV)
  have hbL : routeBlock st st.len < st.numBlocks :=
    Svd.blockOf_lt inv.bsPos inv.vcEq (Nat.mod_lt _ hV)
  have hk : routeBlock st p = routeBlock st st.len := inv.dirInj _ _ hbp hbL hb
  exact specPush_pair_ne_of_vpos_ne (B := st.blockSlots) hvpos
    ⟨by simpa only [specPush_routeBlock_eq] using hk,
     by simpa only [specPush_routeSlot_eq] using hs⟩

theorem specPush_pushFront_pair_ne {st : DequeState α} (inv : Inv st)
    (h : st.len < st.virtCap) {q : Nat} (hq : q < st.len) :
    ¬ (st.dir (routeBlock st q) = st.dir (prevHead st / st.blockSlots) ∧
       routeSlot st q = prevHead st % st.blockSlots) := by
  have hV : 0 < st.virtCap := by omega
  have hH : st.headOff < st.virtCap := specPush_head_lt inv hV
  have hqV : q + 1 < st.virtCap := by omega
  have hvpos : (st.headOff + q) % st.virtCap ≠ prevHead st := by
    have := specPush_prevHead_ne (H := st.headOff) (V := st.virtCap) (q := q) hV hqV
    simpa [prevHead] using this
  rintro ⟨hb, hs⟩
  have hprev : prevHead st < st.virtCap := Nat.mod_lt _ hV
  have hk : routeBlock st q = prevHead st / st.blockSlots := by
    apply inv.dirInj
    · exact Svd.blockOf_lt inv.bsPos inv.vcEq (Nat.mod_lt _ hV)
    · exact Svd.blockOf_lt inv.bsPos inv.vcEq hprev
    · exact hb
  exact specPush_pair_ne_of_vpos_ne (B := st.blockSlots) hvpos
    ⟨by simpa only [specPush_routeBlock_eq] using hk,
     by simpa only [specPush_routeSlot_eq] using hs⟩

/-! ## Point-update readings at the pushed slot -/

theorem specPush_pushBack_contentOf_self (st : DequeState α) (v : α) :
    contentOf (opPushBack st v) st.len = some v := by
  rw [specPush_opPushBack_contentOf]
  exact updBlock_self

theorem specPush_pushBack_contentOf_ne {st : DequeState α} (inv : Inv st)
    (h : st.len < st.virtCap) {v : α} {p : Nat} (hp : p < st.len) :
    contentOf (opPushBack st v) p = contentOf st p := by
  have hnot := specPush_pushBack_pair_ne inv h hp
  have hAB : st.dir (routeBlock st p) ≠ st.dir (routeBlock st st.len) ∨
      routeSlot st p ≠ routeSlot st st.len := by
    by_cases hb : st.dir (routeBlock st p) = st.dir (routeBlock st st.len)
    · refine Or.inr ?_
      intro hs
      exact hnot ⟨hb, hs⟩
    · exact Or.inl hb
  rw [specPush_opPushBack_contentOf, updBlock_ne hAB]
  rfl

theorem specPush_pushFront_contentOf_zero (st : DequeState α) (v : α)
    (hV : 0 < st.virtCap) : contentOf (opPushFront st v) 0 = some v := by
  rw [specPush_opPushFront_contentOf]
  rw [specPush_opPushFront_routeBlock_zero st v hV,
    specPush_opPushFront_routeSlot_zero st v hV]
  exact updBlock_self

theorem specPush_pushFront_contentOf_succ {st : DequeState α} (inv : Inv st)
    (h : st.len < st.virtCap) {v : α} {q : Nat} (hq : q < st.len) :
    contentOf (opPushFront st v) (q + 1) = contentOf st q := by
  have hV : 0 < st.virtCap := by omega
  have hH : st.headOff < st.virtCap := specPush_head_lt inv hV
  have hnot := specPush_pushFront_pair_ne inv h hq
  have hAB : st.dir (routeBlock st q) ≠ st.dir (prevHead st / st.blockSlots) ∨
      routeSlot st q ≠ prevHead st % st.blockSlots := by
    by_cases hb : st.dir (routeBlock st q) = st.dir (prevHead st / st.blockSlots)
    · refine Or.inr ?_
      intro hs
      exact hnot ⟨hb, hs⟩
    · exact Or.inl hb
  rw [specPush_opPushFront_contentOf]
  rw [specPush_opPushFront_routeBlock_succ st v q hV hH,
    specPush_opPushFront_routeSlot_succ st v q hV hH]
  rw [updBlock_ne hAB]
  rfl

/-! ## Observable list contracts (with `Inv`) -/

/-- `push_back` on the non-full path appends exactly one entry. -/
theorem opPushBack_spec {st : DequeState α} {v : α} (inv : Inv st)
    (h : st.len < st.virtCap) :
    logicalList (opPushBack st v) = logicalList st ++ [some v] := by
  have hlen : (logicalList (opPushBack st v)).length =
      (logicalList st ++ [some v]).length := by
    rw [logicalList_length, specPush_opPushBack_len, List.length_append,
      logicalList_length]
    simp
  refine eq_of_length_eq_of_flatRead hlen ?_
  intro p
  rw [flatRead_append_last, logicalList_length,
    logicalList_at (opPushBack st v) p, logicalList_at st p,
    specPush_opPushBack_len]
  rcases Nat.lt_trichotomy p st.len with hlt | heq | hgt
  · rw [if_pos (by omega : p < st.len + 1), if_neg (by omega : p ≠ st.len),
      if_pos hlt]
    exact specPush_pushBack_contentOf_ne inv h hlt
  · subst heq
    rw [if_pos (by omega : st.len < st.len + 1), if_pos rfl]
    exact specPush_pushBack_contentOf_self st v
  · rw [if_neg (by omega : ¬ p < st.len + 1), if_neg (by omega : p ≠ st.len),
      if_neg (by omega : ¬ p < st.len)]

/-- `push_front` on the non-full path prepends exactly one entry. -/
theorem opPushFront_spec {st : DequeState α} {v : α} (inv : Inv st)
    (h : st.len < st.virtCap) :
    logicalList (opPushFront st v) = some v :: logicalList st := by
  have hV : 0 < st.virtCap := by omega
  have hlen : (logicalList (opPushFront st v)).length =
      (some v :: logicalList st).length := by
    rw [logicalList_length, specPush_opPushFront_len, List.length_cons,
      logicalList_length]
  refine eq_of_length_eq_of_flatRead hlen ?_
  intro p
  cases p with
  | zero =>
      rw [flatRead_cons_zero, logicalList_at (opPushFront st v) 0,
        specPush_opPushFront_len, if_pos (by omega : 0 < st.len + 1)]
      exact specPush_pushFront_contentOf_zero st v hV
  | succ q =>
      rw [flatRead_cons_succ, logicalList_at (opPushFront st v) (q + 1),
        logicalList_at st q, specPush_opPushFront_len]
      rcases Nat.lt_or_ge q st.len with hlt | hge
      · rw [if_pos (by omega : q + 1 < st.len + 1), if_pos hlt]
        exact specPush_pushFront_contentOf_succ inv h hlt
      · rw [if_neg (by omega : ¬ q + 1 < st.len + 1),
          if_neg (by omega : ¬ q < st.len)]

/-! ## `Inv` preservation -/

/-- `push_back` on the non-full path preserves `Inv`: the geometry, directory
injectivity and free-list disjointness are untouched, `len` grows by one inside
the capacity, and occupancy splits at the new back slot. -/
theorem inv_opPushBack {st : DequeState α} {v : α} (inv : Inv st)
    (h : st.len < st.virtCap) : Inv (opPushBack st v) := by
  have hV : 0 < st.virtCap := by omega
  have hH : st.headOff < st.virtCap := specPush_head_lt inv hV
  refine
    { bsPos := inv.bsPos
      dsPos := inv.dsPos
      numLeDir := inv.numLeDir
      vcEq := inv.vcEq
      lenLe := by show st.len + 1 ≤ st.virtCap; omega
      headOk := Or.inr ⟨by show 0 < st.len + 1; omega, by simpa [opPushBack] using hH⟩
      dirInj := by
        intro k₁ k₂ h₁ h₂ heq
        exact inv.dirInj k₁ k₂ h₁ h₂ heq
      freeDisj := by
        intro b hb k hk
        exact inv.freeDisj b hb k hk
      occupied := by
        intro p hp
        rcases Nat.lt_or_ge p st.len with hlt | hge
        · obtain ⟨e, he⟩ := inv.occupied p hlt
          exact ⟨e, by rw [specPush_pushBack_contentOf_ne inv h hlt, he]⟩
        · have hpL : p = st.len := by
            have hp' : p < st.len + 1 := hp
            omega
          subst hpL
          exact ⟨v, specPush_pushBack_contentOf_self st v⟩ }

/-- `push_front` on the non-full path preserves `Inv`: the directory and block
geometry are untouched, the new front `prevHead` is in range, `len` grows by one,
and occupancy splits at the new front slot. -/
theorem inv_opPushFront {st : DequeState α} {v : α} (inv : Inv st)
    (h : st.len < st.virtCap) : Inv (opPushFront st v) := by
  have hV : 0 < st.virtCap := by omega
  have hprev : prevHead st < st.virtCap := Nat.mod_lt _ hV
  refine
    { bsPos := inv.bsPos
      dsPos := inv.dsPos
      numLeDir := inv.numLeDir
      vcEq := inv.vcEq
      lenLe := by show st.len + 1 ≤ st.virtCap; omega
      headOk := Or.inr ⟨by show 0 < st.len + 1; omega, by simpa [opPushFront] using hprev⟩
      dirInj := by
        intro k₁ k₂ h₁ h₂ heq
        exact inv.dirInj k₁ k₂ h₁ h₂ heq
      freeDisj := by
        intro b hb k hk
        exact inv.freeDisj b hb k hk
      occupied := by
        intro p hp
        cases p with
        | zero => exact ⟨v, specPush_pushFront_contentOf_zero st v hV⟩
        | succ q =>
            have hq : q < st.len := by
              have hp' : q + 1 < st.len + 1 := hp
              omega
            obtain ⟨e, he⟩ := inv.occupied q hq
            exact ⟨e, by rw [specPush_pushFront_contentOf_succ inv h hq, he]⟩ }

end Svd.Abs
