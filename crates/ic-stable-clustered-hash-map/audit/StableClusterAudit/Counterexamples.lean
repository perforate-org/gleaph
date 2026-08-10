/-
Stage 1 / adversarial — Counterexample to the structural claim behind invariant B4.

The code comment claimed real distances are "bounded by the overflow area N, far below
u16::MAX" (src/map.rs). We show this is NOT a structural consequence of the
ordered-cluster invariants (B1 DistanceValid, B2 ClusterOrdered): a valid clustered
table can hold an entry whose distance from its bucket exceeds the overflow-area size n.

Concretely, let all keys hash to bucket 0. `find_insert_position(0)` appends to the
cluster at slots 0,1,2,... (src/map.rs L309-L319), so the k-th entry sits at slot k with
distance k. With n=3 (capacity 11), inserting 5 such keys (len 5 < 3/4*8 = 6, so no
resize) yields a cluster with max distance 4 > n = 3.

Resolution: this non-structural u16 bound was a real overflow risk (a bucket's cluster
could exceed 65535 and collide with the EMPTY marker). The map now stores distances as
u32 and enforces the fit at insert via `checked_distance`, which traps on overflow.
`DistanceBounded` (B4) is therefore excluded from `ClusterInvariant` and kept only for
reference.
-/

import Mathlib
import StableClusterAudit.Map

open StableCluster

/-- n=3 (capacity 2^3+3 = 11). Slots 0..4 are occupied by bucket-0's cluster
(dist = slot index), slots 5..10 are empty. keyAt is irrelevant to B1/B2, so it is set
to none; keys are an abstract type with no constructors. -/
def badState : State :=
  { n := 3
    len := 5
    remapEnd := none
    dist := fun i => if i ≤ 4 then i else EMPTY
    keyAt := fun _ => none
    valAt := fun _ => 0 }

-- Occupied slots are exactly 0..4, each with distance = its index.
lemma badState_dist (i : Nat) (hi : i ≤ 4) : badState.dist i = i := by
  simp [badState, hi]

-- B1 holds: an occupied slot's distance does not exceed its position.
lemma badState_distanceValid : DistanceValid badState := by
  intro i _hb hoc
  have hle4 : i ≤ 4 := by
    by_cases h : i ≤ 4
    · exact h
    · have hd : badState.dist i = EMPTY := by simp [badState, h]
      exact False.elim (hoc hd)
  have hd : badState.dist i = i := badState_dist i hle4
  simp [hd]

-- B2 holds: bucket values (i - dist i) are non-decreasing; here they are all 0.
lemma badState_clusterOrdered : ClusterOrdered badState := by
  intro i j _hbi _hbj _hij hoci hocj
  have hi : i ≤ 4 := by
    by_cases h : i ≤ 4
    · exact h
    · have hd : badState.dist i = EMPTY := by simp [badState, h]
      exact False.elim (hoci hd)
  have hj : j ≤ 4 := by
    by_cases h : j ≤ 4
    · exact h
    · have hd : badState.dist j = EMPTY := by simp [badState, h]
      exact False.elim (hocj hd)
  have hdi : badState.dist i = i := badState_dist i hi
  have hdj : badState.dist j = j := badState_dist j hj
  simp [BucketAt, hdi, hdj]

-- B4 fails: the entry at slot 4 has distance 4 > n = 3.
lemma badState_notDistanceBounded : ¬ DistanceBounded badState := by
  intro db
  have h4 : 4 < capacity badState.n := by norm_num [capacity, badState]
  have hoc4 : IsOccupied badState 4 := by
    simp [IsOccupied, badState]
    norm_num [EMPTY]
  have h := db 4 h4 hoc4          -- badState.dist 4 ≤ badState.n
  have hd : badState.dist 4 = 4 := by simp [badState]
  rw [hd] at h                    -- h : 4 ≤ badState.n
  norm_num [badState] at h        -- 4 ≤ 3 → contradiction, closes goal

/-- The structural invariants B1 ∧ B2 do NOT imply B4 (dist ≤ n). Hence the u16
distance safety claimed in the code comment is an extra, hash/load-dependent condition,
not a consequence of the ordered-cluster structure. -/
example : ¬ (∀ s : State, (DistanceValid s ∧ ClusterOrdered s) → DistanceBounded s) := by
  intro h
  have hv : DistanceValid badState ∧ ClusterOrdered badState := ⟨badState_distanceValid, badState_clusterOrdered⟩
  exact badState_notDistanceBounded (h badState hv)

/-!
## Counterexample to invariant preservation by the current `RemapStep` relation

`RemapStep` currently records only entry-set, length, and boundary preservation. The
following smallest states show that those constraints do not imply preservation of the
cluster invariant: both states have no keys and length zero, but the target state has an
invalid occupied distance at its sole in-bounds slot.
-/

-- src/map.rs L546-L596 and Map.lean L289-L296: modeled remap relation under test.
def remapGoodState : State :=
  { n := 0
    len := 0
    remapEnd := none
    dist := fun _ => EMPTY
    keyAt := fun _ => none
    valAt := fun _ => 0 }

def remapBadState : State :=
  { n := 0
    len := 0
    remapEnd := none
    dist := fun i => if i = 0 then 1 else EMPTY
    keyAt := fun _ => none
    valAt := fun _ => 0 }

-- Abstract.lean L128-L129: `ClusterInvariant` requires `DistanceValid`,
-- `ClusterOrdered`, and `EntryAtCorrectBucket`.
theorem remapGoodState_clusterInvariant : ClusterInvariant remapGoodState := by
  constructor
  · intro i _hib hoc
    exact False.elim (hoc (by simp [remapGoodState]))
  · constructor
    · intro i _j _hibi _hibj _hij hoci _hocj
      exact False.elim (hoci (by simp [remapGoodState]))
    · intro i _hib hoc
      exact False.elim (hoc (by simp [remapGoodState]))

-- Map.lean L283-L296: `RemapStep` constrains only `KeySet`, `len`, and the
-- `remapEnd` boundary; it does not constrain slot contents or require an invariant.
theorem remapStep_good_bad : RemapStep remapGoodState remapBadState := by
  refine { keySet := ?_, len := ?_, boundary := ?_ }
  · funext k
    apply propext
    simp [KeySet, remapGoodState, remapBadState]
  · rfl
  · trivial

-- Abstract.lean L99-L102, L128-L129: slot 0 is in bounds and occupied, but its
-- distance is 1, so `DistanceValid` would require the false inequality `1 ≤ 0`.
theorem remapBadState_not_clusterInvariant : ¬ ClusterInvariant remapBadState := by
  intro hinv
  have hdist := hinv.1 0 (by norm_num [capacity, remapBadState]) (by
    norm_num [IsOccupied, remapBadState, EMPTY])
  norm_num [remapBadState] at hdist

/-- The current weak `RemapStep` relation does not preserve `ClusterInvariant`. -/
theorem remapStep_does_not_preserve_clusterInvariant :
    ¬ (∀ s s' : State, RemapStep s s' → ClusterInvariant s → ClusterInvariant s') := by
  intro h
  exact remapBadState_not_clusterInvariant
    (h remapGoodState remapBadState remapStep_good_bad remapGoodState_clusterInvariant)
