/-
Stage 3 preservation, part 1: the insert-update path preserves `Inv`.
-/
import Lhm.Abs.Transfer

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Pointwise facts about setValue -/

theorem setValue_buckets (st : MapState K V) (b j : Nat) (k : K) (v : V) :
    (setValue st b j k v).buckets = setBucketEntry st.buckets b j (some (k, v)) := rfl

theorem setValue_left_ne {st : MapState K V} {b j x y : Nat} {k : K} {v : V} (hx : x ≠ b) :
    (setValue st b j k v).buckets x y = st.buckets x y :=
  setBucketEntry_left_ne hx

theorem setValue_right_ne {st : MapState K V} {b j x y : Nat} {k : K} {v : V} (hy : y ≠ j) :
    (setValue st b j k v).buckets x y = st.buckets x y :=
  setBucketEntry_right_ne hy

theorem setValue_at {st : MapState K V} {b j : Nat} {k : K} {v : V} :
    (setValue st b j k v).buckets b j = some (k, v) := by
  rw [setValue_buckets]
  simp [setBucketEntry]

/-! ## The update path preserves Inv -/

theorem inv_setValue {st : MapState K V} (inv : Inv st) {b j : Nat} {k : K} {vo v : V}
    (hloc : st.buckets b j = some (k, vo)) :
    Inv (setValue st b j k v) := by
  -- old-state entry behind any new-state entry (same key; value may differ only at
  -- the overwritten target slot, whose key is preserved)
  have oldKeyOf : ∀ x y e, (setValue st b j k v).buckets x y = some e →
      ∃ eo, st.buckets x y = some eo ∧ eo.1 = e.1 := by
    intro x y e he
    by_cases hxy : x = b ∧ y = j
    · obtain ⟨rfl, rfl⟩ := hxy
      rw [setValue_buckets] at he
      have hs : (setBucketEntry st.buckets x y (some (k, v)) x y) = some (k, v) := by
        simp [setBucketEntry]
      rw [hs] at he
      have hin := Option.some.inj he
      exact ⟨(k, vo), hloc, by rw [hin.symm]⟩
    · refine ⟨e, ?_, rfl⟩
      simpa [setValue, setBucketEntry, hxy] using he
  have hisSome : ∀ x y, ((setValue st b j k v).buckets x y).isSome
      = (st.buckets x y).isSome := by
    intro x y
    by_cases hxb : x = b
    · subst hxb
      by_cases hyy : y = j
      · subst hyy
        simp [setValue_at, hloc]
      · rw [setValue_right_ne hyy]
    · rw [setValue_left_ne hxb]
  -- placement in the new state
  have hpl : ∀ x y e, (setValue st b j k v).buckets x y = some e →
      x < st.physicalBuckets ∧ y < SlotsPerBucket ∧ InCands st e.1 x := by
    intro x y e he
    obtain ⟨eo, hol, hkeq⟩ := oldKeyOf x y e he
    have hp := inv.placed x y eo hol
    rw [hkeq] at hp
    exact hp
  -- uniqueness in the new state
  have huq : ∀ b1 j1 e1 b2 j2 e2, j1 < SlotsPerBucket → j2 < SlotsPerBucket →
      (setValue st b j k v).buckets b1 j1 = some e1 →
      (setValue st b j k v).buckets b2 j2 = some e2 →
      e1.1 = e2.1 → b1 = b2 ∧ j1 = j2 := by
    intro b1 j1 e1 b2 j2 e2 hj1 hj2 l1 l2 hkeys
    obtain ⟨eo1, ho1, hk1⟩ := oldKeyOf b1 j1 e1 l1
    obtain ⟨eo2, ho2, hk2⟩ := oldKeyOf b2 j2 e2 l2
    have hku : eo1.1 = eo2.1 := hk1.trans (hkeys.trans hk2.symm)
    obtain ⟨rfl, rfl⟩ := inv.unique b1 j1 eo1 b2 j2 eo2 hj1 hj2 ho1 ho2 hku
    exact ⟨rfl, rfl⟩
  -- assemble
  have hepEven : (setValue st b j k v).mutationEpoch % 2 = 0 := by
    have he := inv.geomEpochEven
    show (st.mutationEpoch + 2) % 2 = 0
    omega
  refine inv_transfer (inv := inv) (hisSome := hisSome) (hh1 := rfl) (hh2 := rfl)
    (hlev := rfl) (hcur := rfl) (hpb := rfl) (hepEven := hepEven) (hinc := rfl)
    (hlen := rfl) (hcLen := inv.countersLen) (hovf := rfl)
    (hcOvf := inv.countersOvf) hpl huq

/-! ## The remove path preserves Inv (part 1: state transition) -/

end Lhm.Abs
