/-
Stage 3 preservation, part 2: fresh placement (`placeAt`) preserves `Inv`.
-/
import Lhm.Abs.Deltas
import Lhm.Abs.Preserve

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Pointwise facts about placeAt -/

theorem placeAt_buckets {st : MapState K V} {k : K} {v : V} {b j : Nat} :
    (placeAt st k v b j).buckets = setBucketEntry st.buckets b j (some (k, v)) := rfl

theorem placeAt_buckets_ne {st : MapState K V} {k : K} {v : V} {b j y : Nat} (hy : y ≠ j) :
    (placeAt st k v b j).buckets b y = st.buckets b y := by
  rw [placeAt_buckets]
  simp [setBucketEntry, hy]

theorem placeAt_buckets_at {st : MapState K V} {k : K} {v : V} {b j : Nat} :
    (placeAt st k v b j).buckets b j = some (k, v) := by
  rw [placeAt_buckets]
  simp [setBucketEntry]

theorem placeAt_buckets_xne {st : MapState K V} {k : K} {v : V} {b x : Nat} (hx : x ≠ b)
    (j y : Nat) :
    (placeAt st k v b j).buckets x y = st.buckets x y := by
  rw [placeAt_buckets]
  simp [setBucketEntry, hx]

/-! ## The insert-place path preserves `Inv` -/

/-- Placing a fresh entry into a free slot of one of its candidate buckets preserves
`Inv`. The free slot must be genuinely unoccupied (`hfree`), inside its bucket
(`hj`), inside the extent (`hcand` via P1), and the key must be globally absent from
both candidate blocks (`habsent`) so uniqueness survives the fresh copy. -/
theorem inv_place [DecidableEq K] {st : MapState K V} (inv : Inv st) {k : K} {v : V}
    {b j : Nat} (hcand : InCands st k b) (hj : j < SlotsPerBucket)
    (hfree : st.buckets b j = none)
    (habsent : ∀ c, InCands st k c → findIn st c k = none) :
    Inv (placeAt st k v b j) := by
  -- the target bucket lies inside the allocated extent (P1 composed with `hcand`)
  have hbpb : b < st.physicalBuckets := by
    rcases hcand with h | h
    · rw [h]; exact (cand_lt_pb inv k).1
    · rw [h]; exact (cand_lt_pb inv k).2
  -- pointwise bucket agreement off the target slot
  have ho : ∀ y, y ≠ j → (placeAt st k v b j).buckets b y = st.buckets b y :=
    fun y hy => placeAt_buckets_ne hy
  have hat : (placeAt st k v b j).buckets b j = some (k, v) := placeAt_buckets_at
  have hbne : (placeAt st k v b j).buckets b j ≠ none := by rw [hat]; simp
  -- per-bucket load delta at the placement bucket
  have hdelta : loadOf (placeAt st k v b j) b = loadOf st b + 1 :=
    loadOf_place_plus_one hj ho hfree hbne
  have hloadOther : ∀ x, x ≠ b → loadOf (placeAt st k v b j) x = loadOf st x := by
    intro x hx
    refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
    simp only [occPred]
    rw [placeAt_buckets_xne hx j i]
  -- counter equations for the new state
  have hcLenNew : (placeAt st k v b j).len
      = totalLenOf (placeAt st k v b j) (placeAt st k v b j).physicalBuckets := by
    show st.len + 1 = totalLoads (fun x => loadOf (placeAt st k v b j) x) st.physicalBuckets
    have htot : totalLoads (fun x => loadOf (placeAt st k v b j) x) st.physicalBuckets
        = totalLoads (fun x => loadOf st x) st.physicalBuckets + 1 :=
      totalLoads_plus_one _ _ _ b hbpb (fun x _ hxb => hloadOther x hxb) hdelta
    rw [htot, ← totalLenOf_eq]
    have hcl := inv.countersLen
    omega
  have hovfOther : ∀ x, x ≠ b → ovfLoadOf (placeAt st k v b j) x = ovfLoadOf st x := by
    intro x hx
    refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
    simp only [ovfPred]
    rw [placeAt_buckets_xne hx j i]
  have hcOvfNew : (placeAt st k v b j).overflowEntries
      = totalOvfOf (placeAt st k v b j) (placeAt st k v b j).physicalBuckets := by
    show st.overflowEntries + (if PrimarySlots ≤ j then 1 else 0)
        = totalLoads (fun x => ovfLoadOf (placeAt st k v b j) x) st.physicalBuckets
    rcases Nat.lt_or_ge j PrimarySlots with hjlt | hj8
    · -- primary-slot placement: inline-overflow accounting unchanged
      have hovfSame : ovfLoadOf (placeAt st k v b j) b = ovfLoadOf st b := by
        refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
        simp only [ovfPred]
        by_cases hij : i = j
        · rw [hij, placeAt_buckets_at, hfree]
          simp [Nat.not_le.mpr hjlt]
        · rw [placeAt_buckets_ne hij]
      rw [if_neg (by omega),
        totalLoads_congr (f := fun x => ovfLoadOf (placeAt st k v b j) x)
          (g := fun x => ovfLoadOf st x) st.physicalBuckets
          (fun x hx => by
            by_cases hxb : x = b
            · rw [hxb]; exact hovfSame
            · exact hovfOther x hxb),
        ← totalOvfOf_eq]
      exact inv.countersOvf
    · -- overflow-page placement: one more occupied inline-overflow slot
      have hodelta : ovfLoadOf (placeAt st k v b j) b = ovfLoadOf st b + 1 :=
        ovfLoadOf_place_plus_one hj hj8 ho hfree hbne
      have hotot : totalLoads (fun x => ovfLoadOf (placeAt st k v b j) x) st.physicalBuckets
          = totalLoads (fun x => ovfLoadOf st x) st.physicalBuckets + 1 :=
        totalLoads_plus_one _ _ _ b hbpb (fun x _ hxb => hovfOther x hxb) hodelta
      rw [if_pos hj8, hotot, ← totalOvfOf_eq]
      have hc := inv.countersOvf
      omega
  -- every new-state entry sits inside its own candidate pair
  have hplaced : ∀ x y e, (placeAt st k v b j).buckets x y = some e →
      x < (placeAt st k v b j).physicalBuckets ∧ y < SlotsPerBucket ∧ InCands st e.1 x := by
    intro x y e he
    by_cases hxy : x = b ∧ y = j
    · rcases hxy with ⟨hxb, hyj⟩
      rw [hxb, hyj] at he
      have hkv : (k, v) = e := Option.some.inj (hat.symm.trans he)
      refine ⟨by rw [hxb]; exact hbpb, by rw [hyj]; exact hj, ?_⟩
      rw [hxb, ← hkv]
      exact hcand
    · have hold : st.buckets x y = some e := by
        by_cases hxb : x = b
        · have hyj : y ≠ j := fun hcon => hxy ⟨hxb, hcon⟩
          have heq : (placeAt st k v b j).buckets b y = st.buckets b y :=
            placeAt_buckets_ne hyj
          rw [hxb] at he ⊢
          rw [← heq]
          exact he
        · have heq : (placeAt st k v b j).buckets x y = st.buckets x y :=
            placeAt_buckets_xne hxb j y
          rw [← heq]
          exact he
      exact inv.placed x y e hold
  -- uniqueness carries over: away from the target slot entries are old entries; the
  -- fresh copy is globally absent from both candidate blocks by hypothesis
  have huq : ∀ b1 j1 e1 b2 j2 e2, j1 < SlotsPerBucket → j2 < SlotsPerBucket →
      (placeAt st k v b j).buckets b1 j1 = some e1 →
      (placeAt st k v b j).buckets b2 j2 = some e2 →
      e1.1 = e2.1 → b1 = b2 ∧ j1 = j2 := by
    intro b1 j1 e1 b2 j2 e2 hj1 hj2 l1 l2 hkeys
    -- away from the fresh slot, new-state entries are old-state entries verbatim
    have oldOf : ∀ x y e, (placeAt st k v b j).buckets x y = some e →
        ¬ (x = b ∧ y = j) → ∃ eo, st.buckets x y = some eo ∧ eo.1 = e.1 := by
      intro x y e hl hnt
      refine ⟨e, ?_, rfl⟩
      by_cases hxb : x = b
      · have hyj : y ≠ j := fun hcon => hnt ⟨hxb, hcon⟩
        rw [hxb] at hl ⊢
        rw [← placeAt_buckets_ne hyj]
        exact hl
      · rw [← placeAt_buckets_xne hxb j y]
        exact hl
    by_cases ht1 : b1 = b ∧ j1 = j
    · -- first location is the fresh copy (key `k`)
      rw [ht1.1, ht1.2] at l1
      rw [placeAt_buckets_at] at l1
      have hkv1 : (k, v) = e1 := Option.some.inj l1
      have hk1 : e1.1 = k := by rw [← hkv1]
      by_cases ht2 : b2 = b ∧ j2 = j
      · exact ⟨ht1.1.trans ht2.1.symm, ht1.2.trans ht2.2.symm⟩
      · -- second location holds an old copy of `k` inside its own candidate pair:
        -- contradicts global absence
        obtain ⟨eo2, hol2, hke2⟩ := oldOf b2 j2 e2 l2 ht2
        have hk2 : eo2.1 = k := hke2.trans (hkeys.symm.trans hk1)
        have hc2 : InCands st k b2 := by
          rw [← hk2]; exact (inv.placed b2 j2 eo2 hol2).2.2
        obtain ⟨_, _, hf⟩ := findIn_some_of_present ⟨j2, eo2, hj2, hol2, hk2⟩
        rw [habsent b2 hc2] at hf
        contradiction
    · by_cases ht2 : b2 = b ∧ j2 = j
      · -- symmetric case
        rw [ht2.1, ht2.2] at l2
        rw [placeAt_buckets_at] at l2
        have hkv2 : (k, v) = e2 := Option.some.inj l2
        have hk2 : e2.1 = k := by rw [← hkv2]
        obtain ⟨eo1, hol1, hke1⟩ := oldOf b1 j1 e1 l1 ht1
        have hk1 : eo1.1 = k := hke1.trans (hkeys.trans hk2)
        have hc1 : InCands st k b1 := by
          rw [← hk1]; exact (inv.placed b1 j1 eo1 hol1).2.2
        obtain ⟨_, _, hf⟩ := findIn_some_of_present ⟨j1, eo1, hj1, hol1, hk1⟩
        rw [habsent b1 hc1] at hf
        contradiction
      · -- both locations hold pre-existing entries
        obtain ⟨eo1, hol1, hke1⟩ := oldOf b1 j1 e1 l1 ht1
        obtain ⟨eo2, hol2, hke2⟩ := oldOf b2 j2 e2 l2 ht2
        exact inv.unique b1 j1 eo1 b2 j2 eo2 hj1 hj2 hol1 hol2
          (hke1.trans (hkeys.trans hke2.symm))
  refine inv_transfer_core inv rfl rfl rfl rfl rfl ?_ rfl hcLenNew hcOvfNew hplaced huq
  show (st.mutationEpoch + 2) % 2 = 0
  have he := inv.geomEpochEven
  omega

end Lhm.Abs
