/-
Stage-3' pop contracts for the segmented block-ring (ADR 0086 as built).

`opPopBack` and `opPopFront` mirror `VecDeque::pop_back` / `VecDeque::pop_front`
(vec_deque.rs): read the routed slot, shrink `len`, retire the top block when the
pop drains it, and reset/advance the front. The observable contracts are stated
on `logicalList`: a pop removes the last (resp. first) reading. `Inv` survives
the optional `retireTop` and the head reset.
-/
import Svd.Abs.Ops

namespace Svd.Abs

variable {α : Type}

/-! ## Definitional shapes

Both operations match on `st.len`, so `st.len = n + 1` exposes the `succ`
branch. The retirement conditions are named so the case splits below stay
readable; each is definitionally the corresponding `let` in `Ops.lean`. -/

/-- `pop_front`'s drained condition: the popped slot was the last one in the
deque, or in the front block. -/
abbrev specPop_drained (st : DequeState α) (n : Nat) : Prop :=
  n = 0 ∨ (if st.headOff + 1 = st.virtCap then n + st.blockSlots ≤ st.virtCap
    else (st.headOff + 1) % st.blockSlots = 0)

/-- `pop_back`'s retirement condition: the popped slot sits in the top block and
the window below it no longer reaches that block. -/
abbrev specPop_retireBack (st : DequeState α) (n : Nat) : Prop :=
  ((st.headOff + n) % st.virtCap) / st.blockSlots + 1 = st.numBlocks ∧
    (n = 0 ∨ st.headOff + n ≤ (st.numBlocks - 1) * st.blockSlots)

/-- `pop_front`'s retirement condition: the front block drained and is the top
block. It is evaluated against the old routing, before the head advances. -/
abbrev specPop_retireFront (st : DequeState α) (n : Nat) : Prop :=
  specPop_drained st n ∧ st.headOff / st.blockSlots + 1 = st.numBlocks

/-! ## `succ` branch of each operation -/

theorem specPop_popBack_fst {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopBack st).1 = contentOf st n := by
  rw [opPopBack.eq_1, h]

theorem specPop_popBack_snd {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopBack st).2 =
      (if n = 0 then
        { (if specPop_retireBack st n then retireTop { st with len := n }
            else { st with len := n }) with headOff := 0 }
      else (if specPop_retireBack st n then retireTop { st with len := n }
        else { st with len := n })) := by
  rw [opPopBack.eq_1, h]

theorem specPop_popFront_fst {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopFront st).1 =
      st.blocks (st.dir (st.headOff / st.blockSlots)) (st.headOff % st.blockSlots) := by
  rw [opPopFront.eq_1, h]

theorem specPop_popFront_snd {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopFront st).2 =
      { (if specPop_retireFront st n then retireTop { st with len := n }
          else { st with len := n })
          with headOff := (if n = 0 then 0 else (st.headOff + 1) % st.virtCap) } := by
  rw [opPopFront.eq_1, h]

/-! ## Reading through a header update

`contentOf` only depends on `blocks`, `dir`, `blockSlots`, `headOff` and
`virtCap`. These lemmas reduce a reading in a state that changed one of the
header fields to a two-level lookup whose routed position is explicit. -/

/-- Two states with the same routing data and same block contents read alike. -/
theorem specPop_contentOf_congr {st s : DequeState α} (hb : s.blocks = st.blocks)
    (hd : s.dir = st.dir) (hbs : s.blockSlots = st.blockSlots)
    (hh : s.headOff = st.headOff) (hv : s.virtCap = st.virtCap) (p : Nat) :
    contentOf s p = contentOf st p := by
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf
  rw [hb, hd, hbs, hh, hv]

/-- Changing only `len` does not change a reading. -/
theorem specPop_contentOf_len (st : DequeState α) (n p : Nat) :
    contentOf { st with len := n } p = contentOf st p :=
  specPop_contentOf_congr rfl rfl rfl rfl rfl p

/-- A welfare-free reading with an explicit front offset. -/
theorem specPop_contentOf_head (st : DequeState α) (n h' p : Nat) :
    contentOf { st with len := n, headOff := h' } p =
      st.blocks (st.dir (((h' + p) % st.virtCap) / st.blockSlots))
        (((h' + p) % st.virtCap) % st.blockSlots) := by
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf
  rfl

/-- After retiring the top block, a reading is taken against the smaller
virtual capacity, so it is unchanged when the routed position does not move. -/
theorem specPop_contentOf_retire {st : DequeState α} {n p : Nat}
    (hvc : (st.headOff + p) % ((st.numBlocks - 1) * st.blockSlots)
      = (st.headOff + p) % st.virtCap) :
    contentOf (retireTop { st with len := n }) p = contentOf st p := by
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf retireTop
  simp only [hvc]

/-- An explicit front offset plus a reading one position later. -/
theorem specPop_contentOf_shift (st : DequeState α) (n h' p : Nat)
    (hmod : (h' + p) % st.virtCap = (st.headOff + (p + 1)) % st.virtCap) :
    contentOf { st with len := n, headOff := h' } p = contentOf st (p + 1) := by
  rw [specPop_contentOf_head]
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf
  rw [hmod]

/-- A reading through the top directory entry, with an explicit front offset
and the retired (smaller) virtual capacity. -/
theorem specPop_contentOf_retire_head (st : DequeState α) (n h' p : Nat) :
    contentOf { retireTop { st with len := n } with headOff := h' } p =
      st.blocks (st.dir (((h' + p) % ((st.numBlocks - 1) * st.blockSlots)) / st.blockSlots))
        (((h' + p) % ((st.numBlocks - 1) * st.blockSlots)) % st.blockSlots) := by
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf retireTop
  rfl

/-- After a draining pop that retires the top block, the front is 0 and the next
reading is exactly the old reading one position later. -/
theorem specPop_contentOf_retire_zero (st : DequeState α) (n p : Nat)
    (hwrap : st.headOff + 1 = st.virtCap)
    (hvc : st.virtCap = st.numBlocks * st.blockSlots)
    (hp : p < (st.numBlocks - 1) * st.blockSlots) :
    contentOf { retireTop { st with len := n } with headOff := 0 } p
      = contentOf st (p + 1) := by
  rw [specPop_contentOf_retire_head]
  unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf
  have hleCap : (st.numBlocks - 1) * st.blockSlots ≤ st.virtCap := by
    rw [hvc]
    exact Nat.mul_le_mul_right _ (Nat.sub_le _ _)
  have hpVC : p < st.virtCap := Nat.lt_of_lt_of_le hp hleCap
  have hpmodNew : p % ((st.numBlocks - 1) * st.blockSlots) = p := Nat.mod_eq_of_lt hp
  have hpmodOld : p % st.virtCap = p := Nat.mod_eq_of_lt hpVC
  have harg : st.headOff + (p + 1) = st.virtCap + p := by omega
  have hmodOld : (st.virtCap + p) % st.virtCap = p := by
    have h := Nat.add_mul_mod_self_left p st.virtCap 1
    rw [Nat.mul_one] at h
    rw [Nat.add_comm, h, hpmodOld]
  simp only [Nat.zero_add]
  rw [hpmodNew, harg, hmodOld]

/-! ## Length and geometry of the result -/

theorem specPop_popBack_len {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopBack st).2.len = n := by
  rw [specPop_popBack_snd h]
  by_cases hn0 : n = 0
  · rw [if_pos hn0]
    by_cases hret : specPop_retireBack st n
    · rw [if_pos hret]; rfl
    · rw [if_neg hret]
  · rw [if_neg hn0]
    by_cases hret : specPop_retireBack st n
    · rw [if_pos hret]; rfl
    · rw [if_neg hret]

theorem specPop_popFront_len {st : DequeState α} {n : Nat} (h : st.len = n + 1) :
    (opPopFront st).2.len = n := by
  rw [specPop_popFront_snd h]
  by_cases hn0 : n = 0
  · rw [if_pos hn0]
    by_cases hret : specPop_retireFront st n
    · rw [if_pos hret]; rfl
    · rw [if_neg hret]
  · rw [if_neg hn0]
    by_cases hret : specPop_retireFront st n
    · rw [if_pos hret]; rfl
    · rw [if_neg hret]

/-- A nonempty deque has at least one block. -/
theorem specPop_numBlocks_pos {st : DequeState α} (inv : Inv st) (hlen : 0 < st.len) :
    0 < st.numBlocks := by
  have hvc : 0 < st.virtCap := Nat.lt_of_lt_of_le hlen inv.lenLe
  rcases Nat.eq_zero_or_pos st.numBlocks with h | h
  · exfalso
    have : st.virtCap = 0 := by rw [inv.vcEq, h, Nat.zero_mul]
    omega
  · exact h

/-- A nonempty deque has its front inside the virtual capacity. -/
theorem specPop_head_lt {st : DequeState α} (inv : Inv st) (hlen : 0 < st.len) :
    st.headOff < st.virtCap := by
  rcases inv.headOk with ⟨h0, -⟩ | ⟨-, hh⟩
  · omega
  · exact hh

/-- A draining front pop that retires the top block must be the wrap-around
case: if `headOff + 1 < virtCap`, the block boundary cannot coincide with the
top block after the front block drained. -/
theorem specPop_wrap_of_drained {st : DequeState α}
    (hB : 0 < st.blockSlots) {n : Nat} (hn : n ≠ 0)
    (hd : specPop_drained st n)
    (hblock : st.headOff / st.blockSlots + 1 = st.numBlocks)
    (hvc : st.virtCap = st.numBlocks * st.blockSlots) :
    st.headOff + 1 = st.virtCap := by
  rcases hd with h0 | hd
  · exact absurd h0 hn
  · by_cases hw : st.headOff + 1 = st.virtCap
    · exact hw
    · exfalso
      rw [if_neg hw] at hd
      have hBdvd : st.blockSlots ∣ st.headOff % st.blockSlots + 1 := by
        have h1 : st.blockSlots ∣ st.headOff + 1 := Nat.dvd_of_mod_eq_zero hd
        have h2 : st.blockSlots ∣ st.blockSlots * (st.headOff / st.blockSlots) :=
          ⟨st.headOff / st.blockSlots, rfl⟩
        have hsub : st.headOff + 1 - st.blockSlots * (st.headOff / st.blockSlots)
            = st.headOff % st.blockSlots + 1 := by
          have := Nat.div_add_mod st.headOff st.blockSlots
          omega
        rw [← hsub]
        exact Nat.dvd_sub h1 h2
      have hmodlt : st.headOff % st.blockSlots + 1 ≤ st.blockSlots := by
        have := Nat.mod_lt st.headOff hB
        omega
      have hge : st.blockSlots ≤ st.headOff % st.blockSlots + 1 :=
        Nat.le_of_dvd (Nat.succ_pos _) hBdvd
      have heqB : st.headOff % st.blockSlots + 1 = st.blockSlots :=
        Nat.le_antisymm hmodlt hge
      have hdecomp : st.headOff + 1
          = st.blockSlots * (st.headOff / st.blockSlots)
            + (st.headOff % st.blockSlots + 1) := by
        have := Nat.div_add_mod st.headOff st.blockSlots
        omega
      have : st.headOff + 1 = st.virtCap := by
        calc st.headOff + 1
            = st.blockSlots * (st.headOff / st.blockSlots) + st.blockSlots := by
              rw [hdecomp, heqB]
          _ = st.blockSlots * (st.headOff / st.blockSlots + 1) := by
              rw [Nat.mul_add, Nat.mul_one]
          _ = st.blockSlots * st.numBlocks := by rw [hblock]
          _ = st.virtCap := by rw [hvc, Nat.mul_comm]
      exact absurd this hw

/-! ## Content preservation -/

/-- The retiring branch of `pop_back`: once the top block is dropped, the
remaining window lies entirely below the new virtual capacity, so every routed
reading below the new length is unchanged. -/
theorem specPop_popBack_content_retire {st : DequeState α} (inv : Inv st) {n p : Nat}
    (hbound : st.headOff + n ≤ (st.numBlocks - 1) * st.blockSlots)
    (hp : p < n) :
    contentOf (retireTop { st with len := n }) p = contentOf st p := by
  apply specPop_contentOf_retire
  have hltNew : st.headOff + p < (st.numBlocks - 1) * st.blockSlots := by omega
  have hnew : (st.headOff + p) % ((st.numBlocks - 1) * st.blockSlots)
      = st.headOff + p := Nat.mod_eq_of_lt hltNew
  have hleCap : (st.numBlocks - 1) * st.blockSlots ≤ st.virtCap := by
    rw [inv.vcEq]
    exact Nat.mul_le_mul_right _ (Nat.sub_le _ _)
  have hltV : st.headOff + p < st.virtCap := Nat.lt_of_lt_of_le hltNew hleCap
  have hold : (st.headOff + p) % st.virtCap = st.headOff + p := Nat.mod_eq_of_lt hltV
  rw [hnew, hold]

/-- `pop_back` keeps every reading below the new length. The top block may
retire, but `headOff + n ≤ topStart` keeps the whole remaining window below the
new virtual capacity, so routed positions do not move. -/
theorem specPop_popBack_content {st : DequeState α} (inv : Inv st) {n : Nat}
    (h : st.len = n + 1) {p : Nat} (hp : p < n) :
    contentOf (opPopBack st).2 p = contentOf st p := by
  rw [specPop_popBack_snd h]
  by_cases hn0 : n = 0
  · rw [if_pos hn0]
    have hp0 : p < 0 := by rwa [hn0] at hp
    exact absurd hp0 (Nat.not_lt_zero p)
  · rw [if_neg hn0]
    by_cases hret : specPop_retireBack st n
    · rw [if_pos hret]
      exact specPop_popBack_content_retire inv
        (by rcases hret.2 with h0 | htop
            · exact absurd h0 hn0
            · exact htop) hp
    · rw [if_neg hret]
      exact specPop_contentOf_len st n p

/-- `pop_front` shifts every reading down by one. When the front block drains and
the top block retires, the pop must be the wrap-around case, the front resets to
0, and the old reading two positions ahead is exactly the shifted one. -/
theorem specPop_popFront_content {st : DequeState α} (inv : Inv st) {n : Nat}
    (h : st.len = n + 1) {p : Nat} (hp : p < n) :
    contentOf (opPopFront st).2 p = contentOf st (p + 1) := by
  rw [specPop_popFront_snd h]
  by_cases hn0 : n = 0
  · rw [if_pos hn0]
    have hp0 : p < 0 := by rwa [hn0] at hp
    exact absurd hp0 (Nat.not_lt_zero p)
  · rw [if_neg hn0]
    by_cases hret : specPop_retireFront st n
    · rw [if_pos hret]
      have hwrap : st.headOff + 1 = st.virtCap :=
        specPop_wrap_of_drained inv.bsPos hn0 hret.1 hret.2 inv.vcEq
      have hhead0 : (st.headOff + 1) % st.virtCap = 0 := by
        rw [hwrap, Nat.mod_self]
      rw [hhead0]
      have hpNew : p < (st.numBlocks - 1) * st.blockSlots := by
        have hd : n = 0 ∨ (if st.headOff + 1 = st.virtCap
            then n + st.blockSlots ≤ st.virtCap
            else (st.headOff + 1) % st.blockSlots = 0) := hret.1
        rw [if_pos hwrap] at hd
        rcases hd with h0 | hd
        · exact absurd h0 hn0
        · have hcalc : (st.numBlocks - 1) * st.blockSlots
              = st.numBlocks * st.blockSlots - st.blockSlots := by
            rw [Nat.sub_mul, Nat.one_mul]
          rw [hcalc, ← inv.vcEq]
          omega
      exact specPop_contentOf_retire_zero st n p hwrap inv.vcEq hpNew
    · rw [if_neg hret]
      refine specPop_contentOf_shift st n ((st.headOff + 1) % st.virtCap) p ?_
      rw [Nat.mod_add_mod]
      congr 1
      omega

/-! ## `Inv` preservation -/

/-- `Inv` after a pop that repairs only `len` and `headOff` (no retirement). -/
theorem specPop_inv_plain {st : DequeState α} (inv : Inv st) {n h' : Nat}
    (hlen : n ≤ st.virtCap)
    (hhead : (n = 0 ∧ h' = 0) ∨ (0 < n ∧ h' < st.virtCap))
    (hocc : ∀ p, p < n → ∃ e, contentOf { st with len := n, headOff := h' } p = some e) :
    Inv { st with len := n, headOff := h' } where
  bsPos := inv.bsPos
  dsPos := inv.dsPos
  numLeDir := inv.numLeDir
  vcEq := inv.vcEq
  lenLe := hlen
  headOk := hhead
  dirInj := inv.dirInj
  freeDisj := inv.freeDisj
  occupied := hocc

/-- `Inv` after a pop that retires the top block. Directory injectivity and
free/live disjointness survive because the retired base is dropped from the live
range and pushed onto the free list, where `dirInj` keeps it apart from the
remaining live entries. -/
theorem specPop_inv_retire {st : DequeState α} (inv : Inv st) (hNB : 0 < st.numBlocks)
    {n h' : Nat}
    (hlen : n ≤ (st.numBlocks - 1) * st.blockSlots)
    (hhead : (n = 0 ∧ h' = 0) ∨
      (0 < n ∧ h' < (st.numBlocks - 1) * st.blockSlots))
    (hocc : ∀ p, p < n →
      ∃ e, contentOf { retireTop { st with len := n } with headOff := h' } p = some e) :
    Inv { retireTop { st with len := n } with headOff := h' } where
  bsPos := inv.bsPos
  dsPos := inv.dsPos
  numLeDir := by
    show st.numBlocks - 1 ≤ st.dirSlots
    have := inv.numLeDir
    omega
  vcEq := rfl
  lenLe := hlen
  headOk := hhead
  dirInj := by
    intro k1 k2 h1 h2 hd
    have h1' : k1 < st.numBlocks - 1 := h1
    have h2' : k2 < st.numBlocks - 1 := h2
    have hd' : st.dir k1 = st.dir k2 := hd
    exact inv.dirInj k1 k2 (by omega) (by omega) hd'
  freeDisj := by
    intro b hb k hk
    have hk' : k < st.numBlocks := by
      have hk'' : k < st.numBlocks - 1 := hk
      omega
    have hb' : b = st.dir (st.numBlocks - 1) ∨ b ∈ st.free := by
      have hmem : b ∈ st.dir (st.numBlocks - 1) :: st.free := hb
      exact List.mem_cons.mp hmem
    rcases hb' with hheadb | hbmem
    · subst hheadb
      intro hcontra
      have hcontra' : st.dir k = st.dir (st.numBlocks - 1) := hcontra
      have heq : k = st.numBlocks - 1 :=
        inv.dirInj k (st.numBlocks - 1) hk' (by omega) hcontra'
      have hk'' : k < st.numBlocks - 1 := hk
      omega
    · exact inv.freeDisj b hbmem k hk'
  occupied := hocc

/-! ## Contracts -/

theorem opPopBack_none {st : DequeState α} (h : st.len = 0) :
    (opPopBack st).1 = none ∧ (opPopBack st).2 = st := by
  constructor
  · rw [opPopBack.eq_1, h]
  · rw [opPopBack.eq_1, h]

theorem opPopBack_spec {st : DequeState α} (inv : Inv st) (h : 0 < st.len) :
    (opPopBack st).1 = flatRead (logicalList st) (st.len - 1) ∧
    logicalList (opPopBack st).2 = (logicalList st).dropLast := by
  obtain ⟨n, hn⟩ : ∃ n, st.len = n + 1 := ⟨st.len - 1, by omega⟩
  have hfst : (opPopBack st).1 = contentOf st n := specPop_popBack_fst hn
  have hlast : st.len - 1 = n := by omega
  constructor
  · rw [hfst, hlast, logicalList_at]
    rw [if_pos (by omega : n < st.len)]
  · have hdrop : (logicalList st).dropLast = contentUpTo st n := by
      unfold logicalList
      rw [hn, contentUpTo_step, List.dropLast_concat]
    have hll : logicalList (opPopBack st).2 = contentUpTo st n := by
      unfold logicalList
      rw [specPop_popBack_len hn]
      exact contentUpTo_congr (st := (opPopBack st).2) (s2 := st) n
        (fun p hp => specPop_popBack_content inv hn hp)
    rw [hll, hdrop]

theorem inv_opPopBack {st : DequeState α} (inv : Inv st) : Inv (opPopBack st).2 := by
  by_cases h0 : st.len = 0
  · rw [opPopBack.eq_1, h0]
    exact inv
  · obtain ⟨n, hn⟩ : ∃ n, st.len = n + 1 := ⟨st.len - 1, by omega⟩
    have hpos : 0 < st.len := by omega
    have hNB : 0 < st.numBlocks := specPop_numBlocks_pos inv hpos
    rw [specPop_popBack_snd hn]
    by_cases hn0 : n = 0
    · rw [if_pos hn0]
      by_cases hret : specPop_retireBack st n
      · rw [if_pos hret]
        exact specPop_inv_retire inv hNB (n := n) (h' := 0)
          (by omega) (Or.inl ⟨hn0, rfl⟩)
          (by
            intro p hp
            have hp0 : p < 0 := by rwa [hn0] at hp
            exact absurd hp0 (Nat.not_lt_zero p))
      · rw [if_neg hret]
        exact specPop_inv_plain inv (n := n) (h' := 0)
          (by omega) (Or.inl ⟨hn0, rfl⟩)
          (by
            intro p hp
            have hp0 : p < 0 := by rwa [hn0] at hp
            exact absurd hp0 (Nat.not_lt_zero p))
    · rw [if_neg hn0]
      by_cases hret : specPop_retireBack st n
      · rw [if_pos hret]
        have hbound : st.headOff + n ≤ (st.numBlocks - 1) * st.blockSlots := by
          rcases hret.2 with hb | hb
          · exact absurd hb hn0
          · exact hb
        exact specPop_inv_retire inv hNB (n := n) (h' := st.headOff)
          (by omega)
          (Or.inr ⟨Nat.pos_of_ne_zero hn0, by omega⟩)
          (by
            intro p hp
            obtain ⟨e, he⟩ := inv.occupied p (by omega : p < st.len)
            exact ⟨e, (specPop_popBack_content_retire inv hbound hp).trans he⟩)
      · rw [if_neg hret]
        exact specPop_inv_plain inv (n := n) (h' := st.headOff)
          (by have := inv.lenLe; omega)
          (Or.inr ⟨Nat.pos_of_ne_zero hn0, specPop_head_lt inv hpos⟩)
          (by
            intro p hp
            obtain ⟨e, he⟩ := inv.occupied p (by omega : p < st.len)
            exact ⟨e, (specPop_contentOf_len st n p).trans he⟩)

theorem opPopFront_none {st : DequeState α} (h : st.len = 0) :
    (opPopFront st).1 = none ∧ (opPopFront st).2 = st := by
  constructor
  · rw [opPopFront.eq_1, h]
  · rw [opPopFront.eq_1, h]

theorem opPopFront_spec {st : DequeState α} (inv : Inv st) (h : 0 < st.len) :
    (opPopFront st).1 = flatRead (logicalList st) 0 ∧
    logicalList (opPopFront st).2 = (logicalList st).tail := by
  obtain ⟨n, hn⟩ : ∃ n, st.len = n + 1 := ⟨st.len - 1, by omega⟩
  have hhead : st.headOff < st.virtCap := specPop_head_lt inv h
  have hfst : (opPopFront st).1 =
      st.blocks (st.dir (st.headOff / st.blockSlots)) (st.headOff % st.blockSlots) :=
    specPop_popFront_fst hn
  have hone : flatRead (logicalList st) 0 = contentOf st 0 := by
    rw [logicalList_at]
    rw [if_pos h]
  have hzero : contentOf st 0 =
      st.blocks (st.dir (st.headOff / st.blockSlots)) (st.headOff % st.blockSlots) := by
    unfold contentOf routeBlock routeSlot virtualIndex blockOf slotOf
    rw [Nat.add_zero, Nat.mod_eq_of_lt hhead]
  constructor
  · rw [hfst, hone]
    exact hzero.symm
  · apply eq_of_length_eq_of_flatRead
    · rw [List.length_tail, logicalList_length, logicalList_length,
        specPop_popFront_len hn, hn]
      simp
    · intro q
      rw [flatRead_tail, logicalList_at (opPopFront st).2 q, logicalList_at st (q + 1)]
      by_cases hq : q < n
      · have hq' : q < (opPopFront st).2.len := by
          rw [specPop_popFront_len hn]
          exact hq
        rw [if_pos hq', if_pos (by omega : q + 1 < st.len)]
        exact specPop_popFront_content inv hn hq
      · have hq' : ¬ q < (opPopFront st).2.len := by
          rw [specPop_popFront_len hn]
          exact hq
        rw [if_neg hq', if_neg (by omega : ¬ q + 1 < st.len)]

theorem inv_opPopFront {st : DequeState α} (inv : Inv st) : Inv (opPopFront st).2 := by
  by_cases h0 : st.len = 0
  · rw [opPopFront.eq_1, h0]
    exact inv
  · obtain ⟨n, hn⟩ : ∃ n, st.len = n + 1 := ⟨st.len - 1, by omega⟩
    have hpos : 0 < st.len := by omega
    have hNB : 0 < st.numBlocks := specPop_numBlocks_pos inv hpos
    have hVCpos : 0 < st.virtCap := Nat.lt_of_lt_of_le hpos inv.lenLe
    rw [specPop_popFront_snd hn]
    by_cases hn0 : n = 0
    · rw [if_pos hn0]
      by_cases hret : specPop_retireFront st n
      · rw [if_pos hret]
        exact specPop_inv_retire inv hNB (n := n) (h' := 0)
          (by omega) (Or.inl ⟨hn0, rfl⟩)
          (by
            intro p hp
            have hp0 : p < 0 := by rwa [hn0] at hp
            exact absurd hp0 (Nat.not_lt_zero p))
      · rw [if_neg hret]
        exact specPop_inv_plain inv (n := n) (h' := 0)
          (by omega) (Or.inl ⟨hn0, rfl⟩)
          (by
            intro p hp
            have hp0 : p < 0 := by rwa [hn0] at hp
            exact absurd hp0 (Nat.not_lt_zero p))
    · rw [if_neg hn0]
      by_cases hret : specPop_retireFront st n
      · rw [if_pos hret]
        have hwrap : st.headOff + 1 = st.virtCap :=
          specPop_wrap_of_drained inv.bsPos hn0 hret.1 hret.2 inv.vcEq
        have hhead0 : (st.headOff + 1) % st.virtCap = 0 := by
          rw [hwrap, Nat.mod_self]
        rw [hhead0]
        have hd : n = 0 ∨ (if st.headOff + 1 = st.virtCap
            then n + st.blockSlots ≤ st.virtCap
            else (st.headOff + 1) % st.blockSlots = 0) := hret.1
        rw [if_pos hwrap] at hd
        have hdrain : n + st.blockSlots ≤ st.virtCap := by
          rcases hd with h0 | hd
          · exact absurd h0 hn0
          · exact hd
        have hlenNew : n ≤ (st.numBlocks - 1) * st.blockSlots := by
          have hcalc : (st.numBlocks - 1) * st.blockSlots
              = st.numBlocks * st.blockSlots - st.blockSlots := by
            rw [Nat.sub_mul, Nat.one_mul]
          rw [hcalc, ← inv.vcEq]
          omega
        exact specPop_inv_retire inv hNB (n := n) (h' := 0)
          hlenNew
          (Or.inr ⟨Nat.pos_of_ne_zero hn0, by omega⟩)
          (by
            intro p hp
            obtain ⟨e, he⟩ := inv.occupied (p + 1) (by omega : p + 1 < st.len)
            exact ⟨e, by
              rw [specPop_contentOf_retire_zero st n p hwrap inv.vcEq (by omega)]
              exact he⟩)
      · rw [if_neg hret]
        refine specPop_inv_plain inv (n := n) (h' := (st.headOff + 1) % st.virtCap)
          (by have := inv.lenLe; omega)
          (Or.inr ⟨Nat.pos_of_ne_zero hn0, Nat.mod_lt _ hVCpos⟩) ?_
        intro p hp
        obtain ⟨e, he⟩ := inv.occupied (p + 1) (by omega : p + 1 < st.len)
        exact ⟨e, by
          rw [show contentOf { st with len := n, headOff := (st.headOff + 1) % st.virtCap } p
              = contentOf st (p + 1) from by
            refine specPop_contentOf_shift st n ((st.headOff + 1) % st.virtCap) p ?_
            rw [Nat.mod_add_mod]
            congr 1
            omega]
          exact he⟩

end Svd.Abs
