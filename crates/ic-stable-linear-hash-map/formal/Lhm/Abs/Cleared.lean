/-
Stage 3 preservation, part 3: `clear` and `reset` restore a pristine control surface.
At the logical layer the cleared state has no entries anywhere (see `clearedState`),
so placement and uniqueness hold vacuously; geometry is numerically fixed; counters
are zero. `reset` additionally bumps the incarnation, which stays positive.
-/
import Lhm.Abs.Preserve

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-- Re-pointing the incarnation field preserves `Inv` whenever the new value is
nonzero: every other invariant field is untouched by a structure update. -/
theorem inv_set_incarnation {s : MapState K V} (h : Inv s) {i : Nat} (hi : i ≠ 0) :
    Inv { s with incarnation := i } :=
  { h with geomIncarnationPos := hi }

/-- The final logical effect of `clear` satisfies `Inv`: initial-level geometry,
zeroed counters, empty content. -/
theorem inv_cleared {st : MapState K V} (inv : Inv st) : Inv (clearedState st) := by
  have hil63 : InitialLevel < 63 := by
    have h : InitialLevel = 3 := rfl
    omega
  have hbuckets : ∀ x y, (clearedState st).buckets x y = none := fun _ _ => rfl
  have hzeroLoad : ∀ x, loadOf (clearedState st) x = 0 := by
    intro x
    refine countMatch_all_false _ SlotsPerBucket (fun i _ => ?_)
    simp only [occPred]
    rw [hbuckets]
    rfl
  have hzeroOvf : ∀ x, ovfLoadOf (clearedState st) x = 0 := by
    intro x
    refine countMatch_all_false _ SlotsPerBucket (fun i _ => ?_)
    simp only [ovfPred]
    rw [hbuckets]
    simp
  refine ⟨?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_, ?_⟩
  · exact Nat.le_refl InitialLevel
  · exact hil63
  · exact two_pow_pos InitialLevel
  · rfl
  · show (st.mutationEpoch + 2) % 2 = 0
    have hep := inv.geomEpochEven
    omega
  · exact inv.geomIncarnationPos
  · show (0 : Nat)
        = totalLoads (fun x => loadOf (clearedState st) x) (2 ^ InitialLevel)
    exact (totalLoads_full_zero _ _ (fun x _ => hzeroLoad x)).symm
  · show (0 : Nat)
        = totalLoads (fun x => ovfLoadOf (clearedState st) x) (2 ^ InitialLevel)
    exact (totalLoads_full_zero _ _ (fun x _ => hzeroOvf x)).symm
  · intro b j e he
    rw [hbuckets b j] at he
    contradiction
  · intro b1 j1 e1 b2 j2 e2 _ _ l1 l2 _
    rw [hbuckets b1 j1] at l1
    contradiction

/-- The final logical effect of `reset` satisfies `Inv`: clear plus an incarnation
bump, which keeps the value positive. -/
theorem inv_reset {st : MapState K V} (inv : Inv st) : Inv (resetState st) := by
  have hpos : st.incarnation + 1 ≠ 0 := by omega
  exact inv_set_incarnation (inv_cleared inv) hpos

end Lhm.Abs
