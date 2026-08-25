/-
Stage 3 top-level operation contracts, part 1: result-state computation lemmas.

Each lemma evaluates the result state of `opInsert` / `opRemove` given explicit
discriminant facts, mirroring the `opGet_some` / `opGet_none_*` style in Search.lean.
The preservation theorems built on them live in this file as well.
-/
import Lhm.Abs.Place
import Lhm.Abs.Preserve

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## `opInsert` result states -/

theorem opInsert_state_update1 [DecidableEq K] {st : MapState K V} {k : K} {v : V}
    {j : Nat} {kk : K} {vo : V}
    (h : findIn st (cand1 st k) k = some (j, (kk, vo))) :
    (opInsert st k v).2 = setValue st (cand1 st k) j k v := by
  unfold opInsert
  rw [h]

theorem opInsert_state_update2 [DecidableEq K] {st : MapState K V} {k : K} {v : V}
    {j : Nat} {kk : K} {vo : V}
    (h1 : findIn st (cand1 st k) k = none)
    (h2 : findIn st (cand2 st k) k = some (j, (kk, vo))) :
    (opInsert st k v).2 = setValue st (cand2 st k) j k v := by
  unfold opInsert
  rw [h1]
  by_cases hc12 : cand1 st k = cand2 st k
  · -- coincident candidates: `h2` contradicts `h1`
    exfalso
    rw [← hc12] at h2
    rw [h1] at h2
    contradiction
  · rw [if_neg hc12, h2]

theorem opInsert_state_place [DecidableEq K] {st : MapState K V} {k : K} {v : V}
    {b j : Nat}
    (hscrut2 : (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k) = none)
    (h1 : findIn st (cand1 st k) k = none)
    (hcs : chooseFreeSlot st (cand1 st k) (cand2 st k) = some (b, j)) :
    (opInsert st k v).2 = placeAt st k v b j := by
  unfold opInsert
  rw [h1, hscrut2, hcs]

theorem opInsert_state_splitRequired [DecidableEq K] {st : MapState K V} {k : K} {v : V}
    (hscrut2 : (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k) = none)
    (h1 : findIn st (cand1 st k) k = none)
    (hcs : chooseFreeSlot st (cand1 st k) (cand2 st k) = none) :
    (opInsert st k v).2 = st := by
  unfold opInsert
  rw [h1, hscrut2, hcs]

/-! ## Free-slot choice facts -/

/-- Any slot handed out by `chooseFreeSlot` lies in one of the candidate blocks, at a
real slot index that is genuinely free. -/
theorem chooseFreeSlot_spec {st : MapState K V} {c1 c2 b j : Nat}
    (h : chooseFreeSlot st c1 c2 = some (b, j)) :
    (b = c1 ∨ b = c2) ∧ j < SlotsPerBucket ∧ st.buckets b j = none := by
  have free : ∀ c i, firstFreeIdx st c = some i →
      i < SlotsPerBucket ∧ st.buckets c i = none := by
    intro c i hi
    obtain ⟨hlt, hp⟩ := firstMatch_found hi
    refine ⟨hlt, ?_⟩
    simp at hp
    simp only [occPred] at hp
    cases hb : st.buckets c i with
    | none => rfl
    | some e => rw [hb] at hp; simp at hp
  unfold chooseFreeSlot at h
  cases hf1 : firstFreeIdx st c1 with
  | none =>
      rw [hf1] at h
      by_cases hc12 : c1 = c2
      · rw [if_pos hc12] at h
        simp at h
      · rw [if_neg hc12] at h
        cases hf2 : firstFreeIdx st c2 with
        | none => rw [hf2] at h; simp at h
        | some i2 =>
            rw [hf2] at h
            simp only [] at h
            have hb2 := free c2 i2 hf2
            obtain ⟨rfl, rfl⟩ := Option.some.inj h
            exact ⟨Or.inr rfl, hb2⟩
  | some i1 =>
      rw [hf1] at h
      by_cases hc12 : c1 = c2
      · rw [if_pos hc12] at h
        simp only [] at h
        have hb1 := free c1 i1 hf1
        obtain ⟨rfl, rfl⟩ := Option.some.inj h
        exact ⟨Or.inl rfl, hb1⟩
      · rw [if_neg hc12] at h
        cases hf2 : firstFreeIdx st c2 with
        | none =>
            rw [hf2] at h
            simp only [] at h
            have hb1 := free c1 i1 hf1
            obtain ⟨rfl, rfl⟩ := Option.some.inj h
            exact ⟨Or.inl rfl, hb1⟩
        | some i2 =>
            rw [hf2] at h
            simp only [] at h
            have hb1 := free c1 i1 hf1
            have hb2 := free c2 i2 hf2
            split at h
            · obtain ⟨rfl, rfl⟩ := Option.some.inj h
              exact ⟨Or.inl rfl, hb1⟩
            · obtain ⟨rfl, rfl⟩ := Option.some.inj h
              exact ⟨Or.inr rfl, hb2⟩

/-! ## `opRemove` result states -/

theorem opRemove_state_clear1 [DecidableEq K] {st : MapState K V} {k : K} {j : Nat}
    {e : K × V} (h : findIn st (cand1 st k) k = some (j, e)) :
    (opRemove st k).2 = clearSlot st (cand1 st k) j := by
  unfold opRemove
  rw [h]

theorem opRemove_state_clear2 [DecidableEq K] {st : MapState K V} {k : K} {j : Nat}
    {e : K × V}
    (hscrut2 : (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k)
        = some (j, e))
    (h1 : findIn st (cand1 st k) k = none) :
    (opRemove st k).2 = clearSlot st (cand2 st k) j := by
  unfold opRemove
  rw [h1, hscrut2]

theorem opRemove_state_none [DecidableEq K] {st : MapState K V} {k : K}
    (hscrut2 : (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k) = none)
    (h1 : findIn st (cand1 st k) k = none) :
    (opRemove st k).2 = st := by
  unfold opRemove
  rw [h1, hscrut2]

/-! ## Preservation -/

/-- `insert` preserves `Inv`: updates reuse an occupied same-key slot (`setValue`),
fresh placements land in a genuinely free slot of a candidate block whose key is
globally absent from both candidate blocks (`placeAt`), and a refused insert leaves
the state untouched. -/
theorem opInsert_preserves [DecidableEq K] {st : MapState K V} (inv : Inv st)
    (k : K) (v : V) : Inv (opInsert st k v).2 := by
  cases hf1 : findIn st (cand1 st k) k with
  | some w =>
      obtain ⟨j, kk, vo⟩ := w
      have hspec := findIn_some_spec hf1
      have hkk : kk = k := hspec.2.2
      have hloc := hspec.2.1
      rw [hkk] at hloc
      rw [opInsert_state_update1 hf1]
      exact inv_setValue inv hloc
  | none =>
      by_cases hc12 : cand1 st k = cand2 st k
      · -- coincident candidates: one block plays both roles
        have hscrut2 : (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k)
            = none := if_pos hc12
        have habsent : ∀ c, InCands st k c → findIn st c k = none := by
          intro c hin
          rcases hin with h | h
          · rw [h]; exact hf1
          · rw [h, ← hc12]; exact hf1
        cases hcs : chooseFreeSlot st (cand1 st k) (cand2 st k) with
        | some bj =>
            obtain ⟨b, j⟩ := bj
            have hcspec := chooseFreeSlot_spec hcs
            rw [opInsert_state_place hscrut2 hf1 hcs]
            rcases hcspec.1 with hbc | hbc
            · refine inv_place inv (by rw [hbc]; exact Or.inl rfl) hcspec.2.1
                hcspec.2.2 habsent
            · refine inv_place inv (by rw [hbc]; exact Or.inr rfl) hcspec.2.1
                hcspec.2.2 habsent
        | none => rw [opInsert_state_splitRequired hscrut2 hf1 hcs]; exact inv
      · -- distinct candidates
        cases hf2 : findIn st (cand2 st k) k with
        | some w2 =>
            obtain ⟨j, kk, vo⟩ := w2
            have hspec := findIn_some_spec hf2
            have hkk : kk = k := hspec.2.2
            have hloc := hspec.2.1
            rw [hkk] at hloc
            rw [opInsert_state_update2 hf1 hf2]
            exact inv_setValue inv hloc
        | none =>
            have hscrut2none :
                (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k)
                  = none := by rw [if_neg hc12]; exact hf2
            have habsent : ∀ c, InCands st k c → findIn st c k = none := by
              intro c hin
              rcases hin with h | h
              · rw [h]; exact hf1
              · rw [h]; exact hf2
            cases hcs : chooseFreeSlot st (cand1 st k) (cand2 st k) with
            | some bj =>
                obtain ⟨b, j⟩ := bj
                have hcspec := chooseFreeSlot_spec hcs
                rw [opInsert_state_place hscrut2none hf1 hcs]
                rcases hcspec.1 with hbc | hbc
                · refine inv_place inv (by rw [hbc]; exact Or.inl rfl) hcspec.2.1
                    hcspec.2.2 habsent
                · refine inv_place inv (by rw [hbc]; exact Or.inr rfl) hcspec.2.1
                    hcspec.2.2 habsent
            | none => rw [opInsert_state_splitRequired hscrut2none hf1 hcs]; exact inv

/-- `remove` preserves `Inv`: hits clear a genuinely occupied slot; misses leave the
state untouched. No candidate facts are needed — every new-state entry is an
old-state entry verbatim. -/
theorem opRemove_preserves [DecidableEq K] {st : MapState K V} (inv : Inv st) (k : K) :
    Inv (opRemove st k).2 := by
  cases hf1 : findIn st (cand1 st k) k with
  | some w =>
      obtain ⟨j, e⟩ := w
      rw [opRemove_state_clear1 hf1]
      exact inv_clearSlot inv (findIn_some_spec hf1).2.1
  | none =>
      by_cases hc12 : cand1 st k = cand2 st k
      · rw [opRemove_state_none (if_pos hc12) hf1]; exact inv
      · cases hf2 : findIn st (cand2 st k) k with
        | some w2 =>
            obtain ⟨j, e⟩ := w2
            rw [opRemove_state_clear2 (by rw [if_neg hc12]; exact hf2) hf1]
            exact inv_clearSlot inv (findIn_some_spec hf2).2.1
        | none =>
            rw [opRemove_state_none (by rw [if_neg hc12]; exact hf2) hf1]
            exact inv

end Lhm.Abs
