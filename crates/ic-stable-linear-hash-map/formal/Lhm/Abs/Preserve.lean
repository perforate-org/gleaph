/-
Stage 3 preservation, part 1: the insert-update path preserves `Inv`.
-/
import Lhm.Abs.Transfer
import Lhm.Abs.Deltas

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

/-! ## The remove path preserves `Inv` -/

/-- Removing the entry stored at `(b, j)` (which must be genuinely occupied) preserves
`Inv`. No candidate information is needed: every new-state entry is an old-state entry
verbatim, so placement and uniqueness transport directly; only the counters move.
The `splitDebt` rewrite rule is outside `Inv`, mirroring its advisory role in the
control record. -/
theorem inv_clearSlot {st : MapState K V} (inv : Inv st) {b j : Nat} {e0 : K × V}
    (hloc : st.buckets b j = some e0) :
    Inv (clearSlot st b j) := by
  -- extent/slot facts of the removed location
  obtain ⟨hbpb, hj, _⟩ := inv.placed b j e0 hloc
  have hbuckets : (clearSlot st b j).buckets = setBucketEntry st.buckets b j none := rfl
  have ho : ∀ y, y ≠ j → (clearSlot st b j).buckets b y = st.buckets b y := by
    intro y hy
    rw [hbuckets]
    exact setBucketEntry_right_ne hy
  have hnone : (clearSlot st b j).buckets b j = none := by
    rw [hbuckets]; exact setBucketEntry_self
  have hocc : st.buckets b j ≠ none := by rw [hloc]; simp
  -- per-bucket load delta at the cleared bucket
  have hdelta : loadOf (clearSlot st b j) b + 1 = loadOf st b :=
    loadOf_place_minus_one hj ho hocc hnone
  -- the cleared slot contributed one occupied position, hence old `len ≥ 1`
  have hlenpos : 1 ≤ st.len := by
    have hsucc : countMatch (occPred st b) (j + 1) = countMatch (occPred st b) j + 1 :=
      countMatch_succ_of_true _ j (by show (st.buckets b j).isSome = true; rw [hloc]; simp)
    have hmono : countMatch (occPred st b) (j + 1) ≤ loadOf st b :=
      countMatch_mono _ SlotsPerBucket (j + 1) (by omega)
    calc 1 ≤ countMatch (occPred st b) j + 1 := Nat.le_add_left _ _
      _ = countMatch (occPred st b) (j + 1) := hsucc.symm
      _ ≤ loadOf st b := hmono
      _ ≤ totalLoads (fun x => loadOf st x) st.physicalBuckets :=
            totalLoads_ge_single _ _ _ hbpb
      _ = st.len := by
            show totalLenOf st st.physicalBuckets = st.len
            exact inv.countersLen.symm
  -- counter equations for the new state
  have hloadOther : ∀ x, x ≠ b → loadOf (clearSlot st b j) x = loadOf st x := by
    intro x hx
    refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
    simp only [occPred]
    have heq : (clearSlot st b j).buckets x i = st.buckets x i := by
      rw [hbuckets]; exact setBucketEntry_left_ne hx
    rw [heq]
  have hcLenNew : (clearSlot st b j).len
      = totalLenOf (clearSlot st b j) (clearSlot st b j).physicalBuckets := by
    show st.len - 1 = totalLoads (fun x => loadOf (clearSlot st b j) x) st.physicalBuckets
    have htot : totalLoads (fun x => loadOf (clearSlot st b j) x) st.physicalBuckets + 1
        = totalLoads (fun x => loadOf st x) st.physicalBuckets :=
      totalLoads_minus_one _ _ _ b hbpb (fun x _ hxb => hloadOther x hxb) hdelta
    have hcl : st.len = totalLoads (fun x => loadOf st x) st.physicalBuckets :=
      inv.countersLen
    omega
  have hcOvfNew : (clearSlot st b j).overflowEntries
      = totalOvfOf (clearSlot st b j) (clearSlot st b j).physicalBuckets := by
    show st.overflowEntries - (if PrimarySlots ≤ j then 1 else 0)
        = totalLoads (fun x => ovfLoadOf (clearSlot st b j) x) st.physicalBuckets
    rcases Nat.lt_or_ge j PrimarySlots with hjlt | hj8
    · -- primary-slot clear: overflow accounting unchanged
      have hovfSame : ovfLoadOf (clearSlot st b j) b = ovfLoadOf st b := by
        refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
        simp only [ovfPred]
        by_cases hij : i = j
        · rw [hij, hnone, hloc]
          simp [Nat.not_le.mpr hjlt]
        · have heq : (clearSlot st b j).buckets b i = st.buckets b i := by
            rw [hbuckets]; exact setBucketEntry_right_ne hij
          rw [heq]
      have hovfTot : totalLoads (fun x => ovfLoadOf (clearSlot st b j) x)
          st.physicalBuckets = totalLoads (fun x => ovfLoadOf st x) st.physicalBuckets := by
        refine totalLoads_congr _ _ _ (fun x hx => ?_)
        by_cases hxb : x = b
        · rw [hxb]; exact hovfSame
        · refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
          simp only [ovfPred]
          have heq : (clearSlot st b j).buckets x i = st.buckets x i := by
            rw [hbuckets]; exact setBucketEntry_left_ne hxb
          rw [heq]
      rw [if_neg (by omega), hovfTot, ← totalOvfOf_eq]
      exact inv.countersOvf
    · -- overflow-page clear: one fewer occupied inline-overflow slot
      have hodelta : ovfLoadOf (clearSlot st b j) b + 1 = ovfLoadOf st b :=
        ovfLoadOf_place_minus_one hj hj8 ho hocc hnone
      have hovfOther : ∀ x, x ≠ b → ovfLoadOf (clearSlot st b j) x = ovfLoadOf st x := by
        intro x hx
        refine countMatch_congr _ _ SlotsPerBucket (fun i _ => ?_)
        simp only [ovfPred]
        have heq : (clearSlot st b j).buckets x i = st.buckets x i := by
          rw [hbuckets]; exact setBucketEntry_left_ne hx
        rw [heq]
      have hotot : totalLoads (fun x => ovfLoadOf (clearSlot st b j) x)
          st.physicalBuckets + 1 = totalLoads (fun x => ovfLoadOf st x) st.physicalBuckets :=
        totalLoads_minus_one _ _ _ b hbpb (fun x _ hxb => hovfOther x hxb) hodelta
      rw [if_pos hj8]
      have hc : st.overflowEntries
          = totalLoads (fun x => ovfLoadOf st x) st.physicalBuckets :=
        inv.countersOvf
      omega
  -- every new-state entry is an old-state entry verbatim
  have toOld : ∀ x y e, (clearSlot st b j).buckets x y = some e →
      st.buckets x y = some e := by
    intro x y e he
    rw [hbuckets] at he
    by_cases hxb : x = b
    · by_cases hyj : y = j
      · rw [hxb, hyj, setBucketEntry_self] at he
        contradiction
      · rw [setBucketEntry_right_ne hyj] at he
        exact he
    · rw [setBucketEntry_left_ne hxb] at he
      exact he
  have hplaced : ∀ x y e, (clearSlot st b j).buckets x y = some e →
      x < (clearSlot st b j).physicalBuckets ∧ y < SlotsPerBucket ∧ InCands st e.1 x := by
    intro x y e he
    exact inv.placed x y e (toOld x y e he)
  have huq : ∀ b1 j1 e1 b2 j2 e2, j1 < SlotsPerBucket → j2 < SlotsPerBucket →
      (clearSlot st b j).buckets b1 j1 = some e1 →
      (clearSlot st b j).buckets b2 j2 = some e2 →
      e1.1 = e2.1 → b1 = b2 ∧ j1 = j2 := by
    intro b1 j1 e1 b2 j2 e2 hj1 hj2 l1 l2 hkeys
    exact inv.unique b1 j1 e1 b2 j2 e2 hj1 hj2 (toOld b1 j1 e1 l1) (toOld b2 j2 e2 l2)
      hkeys
  refine inv_transfer_core inv rfl rfl rfl rfl rfl ?_ rfl hcLenNew hcOvfNew hplaced huq
  show (st.mutationEpoch + 2) % 2 = 0
  have hep := inv.geomEpochEven
  omega

end Lhm.Abs
