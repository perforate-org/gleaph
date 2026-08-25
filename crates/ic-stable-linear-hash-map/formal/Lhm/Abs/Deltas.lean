/-
Stage 3 counter deltas for single-slot placement/removal.
-/
import Lhm.Abs.Transfer

namespace Lhm.Abs

open Lhm

variable {K V : Type}

theorem loadOf_place_plus_one {st s2 : MapState K V} {b j : Nat}
    (hj : j < SlotsPerBucket)
    (ho : ∀ y, y ≠ j → s2.buckets b y = st.buckets b y)
    (hold : st.buckets b j = none) (hne : s2.buckets b j ≠ none) :
    loadOf s2 b = loadOf st b + 1 := by
  unfold loadOf
  refine countMatch_plus_one (occPred s2 b) (occPred st b) SlotsPerBucket j hj ?_ ?_ ?_
  · intro i hi hne
    simp only [occPred]
    rw [ho i hne]
  · simp only [occPred]
    rw [hold]
    simp
  · simp only [occPred]
    cases hc : s2.buckets b j with
    | none => exact absurd hc hne
    | some _ => simp

theorem loadOf_place_minus_one {st s2 : MapState K V} {b j : Nat}
    (hj : j < SlotsPerBucket)
    (ho : ∀ y, y ≠ j → s2.buckets b y = st.buckets b y)
    (hold : st.buckets b j ≠ none) (hne : s2.buckets b j = none) :
    loadOf s2 b + 1 = loadOf st b := by
  unfold loadOf
  refine countMatch_minus_one (occPred s2 b) (occPred st b) SlotsPerBucket j hj ?_ ?_ ?_
  · intro i hi hne
    simp only [occPred]
    rw [ho i hne]
  · simp only [occPred]
    cases hc : st.buckets b j with
    | none => exact absurd hc hold
    | some _ => simp
  · simp only [occPred]
    rw [hne]
    simp

theorem ovfLoadOf_place_plus_one {st s2 : MapState K V} {b j : Nat}
    (hj : j < SlotsPerBucket) (hj8 : PrimarySlots ≤ j)
    (ho : ∀ y, y ≠ j → s2.buckets b y = st.buckets b y)
    (hold : st.buckets b j = none) (hnew : s2.buckets b j ≠ none) :
    ovfLoadOf s2 b = ovfLoadOf st b + 1 := by
  unfold ovfLoadOf
  refine countMatch_plus_one (ovfPred s2 b) (ovfPred st b) SlotsPerBucket j hj ?_ ?_ ?_
  · intro i hi hne
    simp only [ovfPred]
    rw [ho i hne]
  · simp only [ovfPred]
    rw [hold]
    simp
  · simp only [ovfPred]
    cases hc : s2.buckets b j with
    | none => exact absurd hc hnew
    | some _ => simp [hj8]

theorem ovfLoadOf_place_minus_one {st s2 : MapState K V} {b j : Nat}
    (hj : j < SlotsPerBucket) (hj8 : PrimarySlots ≤ j)
    (ho : ∀ y, y ≠ j → s2.buckets b y = st.buckets b y)
    (hold : st.buckets b j ≠ none) (hne : s2.buckets b j = none) :
    ovfLoadOf s2 b + 1 = ovfLoadOf st b := by
  unfold ovfLoadOf
  refine countMatch_minus_one (ovfPred s2 b) (ovfPred st b) SlotsPerBucket j hj ?_ ?_ ?_
  · intro i hi hne
    simp only [ovfPred]
    rw [ho i hne]
  · simp only [ovfPred]
    cases hc : st.buckets b j with
    | none => exact absurd hc hold
    | some _ => simp [hj8]
  · simp only [ovfPred]
    rw [hne]
    simp

end Lhm.Abs
