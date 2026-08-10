/-
Stage 3 — Soundness proofs for `StableClusteredHashMap`.

Proves that the Stage 1 properties (SCOPE §4) follow from the Stage 2 model
(`Map.lean`) and the abstract state (`Abstract.lean`). Where a proof is not yet
discharged, it is left as `sorry` with a comment explaining what is needed.

Targets:
  (a) entry preservation across resize  (`ResizePreservesEntries`)
  (b) cluster invariant preservation      (`InsertPreservesInvariant`, ...)
  (c) re-open consistency                 (`ReopenConsistent`)
-/

import Mathlib
import StableClusterAudit.Abstract
import StableClusterAudit.Map

open StableCluster

namespace StableClusterAudit

/-!
## Target (a) — `size_up` preserves the entry set and count

`size_up` (src/map.rs L510-L542) grows the table in place: the old region keeps its
keys/values/distances verbatim, and the newly grown region is cleared (`clear_region`,
src/map.rs L529-L536). `SizeUp` states exactly this, so the entry set is unchanged.
-/

-- `capacity` is strictly increasing, so the old region is inside the new one.
lemma capacity_lt_capacity_succ (n : Nat) : capacity n < capacity (n + 1) := by
  unfold capacity
  have hpow : 2 ^ (n + 1) = 2 ^ n * 2 := by rw [pow_succ]
  rw [hpow]
  omega

-- `size_up` preserves the entry set, pointwise.
lemma sizeUp_preserves_keySet (h : SizeUp s s') (k : Key) : KeySet s k ↔ KeySet s' k := by
  constructor
  · intro ⟨i, hicap, hiocc, hkey⟩
    refine ⟨i, ?_, ?_, ?_⟩
    · simpa [h.n] using lt_trans hicap (capacity_lt_capacity_succ s.n)
    · have hd : s'.dist i = s.dist i := h.distOld i hicap
      change s'.dist i ≠ EMPTY
      rw [hd]
      exact hiocc
    · rw [h.keyAt i]
      exact hkey
  · intro ⟨i, hicap', hiocc', hkey'⟩
    by_cases hicap : i < capacity s.n
    · refine ⟨i, hicap, ?_, ?_⟩
      · have hd : s'.dist i = s.dist i := h.distOld i hicap
        change s.dist i ≠ EMPTY
        rw [← hd]
        exact hiocc'
      · rw [← h.keyAt i]
        exact hkey'
    -- i is in the newly grown region: it is cleared (EMPTY), so not occupied.
    · have hge : capacity s.n ≤ i := Nat.le_of_not_gt hicap
      have hd : s'.dist i = EMPTY := h.distNew i hge hicap'
      exact False.elim (hiocc' hd)

lemma sizeUp_preserves_keySet_eq (h : SizeUp s s') : KeySet s = KeySet s' := by
  funext k
  exact propext (sizeUp_preserves_keySet h k)

lemma sizeUp_preserves_len (h : SizeUp s s') : s.len = s'.len := h.len.symm

lemma sizeUp_preserves_entries (h : SizeUp s s') : ResizePreservesEntries s s' := by
  exact ⟨sizeUp_preserves_keySet_eq h, sizeUp_preserves_len h⟩

/-!
## Target (a) continued — `remap` preserves the entry set (src/map.rs L544-L584)

`remap` relocates entries to their new buckets without adding or dropping any, so the
entry set and count are preserved. `RemapStep` (Map.lean) states exactly this invariant
(`keySet`/`len`), which the implementation guarantees by removing and re-inserting
precisely one entry per step. The deeper check that `remap_position`'s remove-then-insert
achieves it (via the relocation chain) is the same argument as target (b) and remains
deferred.
-/

lemma remap_preserves_keySet (h : RemapStep s s') (k : Key) : KeySet s k ↔ KeySet s' k := by
  exact Iff.of_eq (congrFun h.keySet k)

lemma remap_preserves_entries (h : RemapStep s s') : ResizePreservesEntries s s' := by
  exact ⟨h.keySet, h.len.symm⟩

/-!
## Target (b) — cluster invariant preserved by mutations (src/map.rs L411-L508)

`insert_and_relocate` / `remove_and_relocate` shift cluster tails. Proving that
`ClusterOrdered` and `EntryAtCorrectBucket` are maintained through an arbitrary
relocation chain is the crux of the audit. The relocation relations (`RelocateStep`,
`UnRelocateStep`) capture one step but not the loop's closure, so an inductive argument
over the relocation chain is required.

Progress: the base case of an insert (writing into an empty slot) preserves
`DistanceValid`; the ordered-cluster argument and the chain are deferred below.
-/

-- The base insert write keeps the distance invariant: the new slot's distance
-- `position - bucket ≤ position`, and all other slots are unchanged.
lemma relocateWrite_preserves_distanceValid {s s' : State} {entry : Key} {value : Nat} {position : Nat}
    (h : RelocateWrite s s' entry value position) (hiv : DistanceValid s) : DistanceValid s' := by
  intro i hicap hiocc
  by_cases hpos : i = position
  · subst hpos
    rw [h.dist]
    exact Nat.sub_le i (bucket entry s.n)
  · have hicap_s : i < capacity s.n := by simpa [h.n] using hicap
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hpos]
      exact hiocc
    have hvalid := hiv i hicap_s hiocc_s
    rw [h.dist_other i hpos]
    exact hvalid

-- The base insert write keeps the ordered-cluster invariant, assuming `position` is a valid
-- insertion point for the entry's bucket (as `find_insert_position` guarantees).
lemma relocateWrite_preserves_clusterOrdered {s s' : State} {entry : Key} {value : Nat} {position : Nat}
    (h : RelocateWrite s s' entry value position)
    (hco : ClusterOrdered s) (hip : IsInsertionPoint s position (bucket entry s.n)) :
    ClusterOrdered s' := by
  have hbkt_pos : BucketAt s' position = bucket entry s.n := by
    unfold BucketAt
    rw [h.dist]
    have hle : bucket entry s.n ≤ position := h.b_le_pos
    omega
  have hbkt_other : ∀ i, i ≠ position → BucketAt s' i = BucketAt s i := by
    intro i hi_ne
    unfold BucketAt
    rw [h.dist_other i hi_ne]
  have hocc_iff : ∀ i, i ≠ position → (IsOccupied s' i ↔ IsOccupied s i) := by
    intro i hi_ne
    unfold IsOccupied
    rw [h.dist_other i hi_ne]
  intro i j hicap_i hicap_j hij hiocc_i hiocc_j
  by_cases h_i_pos : i = position
  · by_cases h_j_pos : j = position
    · exfalso
      exact (ne_of_lt hij) (h_i_pos.trans h_j_pos.symm)
    · rw [h_i_pos, hbkt_pos]
      rw [hbkt_other j h_j_pos]
      have hj_gt : position < j := by simpa [h_i_pos] using hij
      have hicap_sj : j < capacity s.n := by simpa [h.n] using hicap_j
      have hiocc_sj : IsOccupied s j := (hocc_iff j h_j_pos).1 hiocc_j
      exact hip.2.2 j hj_gt hicap_sj hiocc_sj
  · by_cases h_j_pos : j = position
    · rw [h_j_pos, hbkt_pos]
      rw [hbkt_other i h_i_pos]
      have hi_lt : i < position := by simpa [h_j_pos] using hij
      have hicap_si : i < capacity s.n := by simpa [h.n] using hicap_i
      have hiocc_si : IsOccupied s i := (hocc_iff i h_i_pos).1 hiocc_i
      exact hip.2.1 i hi_lt hiocc_si
    · rw [hbkt_other i h_i_pos, hbkt_other j h_j_pos]
      have hicap_si : i < capacity s.n := by simpa [h.n] using hicap_i
      have hicap_sj : j < capacity s.n := by simpa [h.n] using hicap_j
      have hiocc_si : IsOccupied s i := (hocc_iff i h_i_pos).1 hiocc_i
      have hiocc_sj : IsOccupied s j := (hocc_iff j h_j_pos).1 hiocc_j
      exact hco i j hicap_si hicap_sj hij hiocc_si hiocc_sj

-- The base insert write keeps each entry at its correct bucket: the new entry sits at
-- `bucket entry s.n`, and all other entries are unchanged. Requires a fresh insert with no
-- resize in progress (`remapEnd = none`).
lemma relocateWrite_preserves_entryAtCorrectBucket {s s' : State} {entry : Key} {value : Nat}
    {position : Nat}
    (h : RelocateWrite s s' entry value position) (hcorrect : EntryAtCorrectBucket s)
    (hremap : s.remapEnd = none) (hremap' : s'.remapEnd = s.remapEnd) :
    EntryAtCorrectBucket s' := by
  have hbkt_pos : BucketAt s' position = bucket entry s.n := by
    unfold BucketAt
    rw [h.dist]
    have hle : bucket entry s.n ≤ position := h.b_le_pos
    omega
  intro i hicap hiocc
  by_cases hi_pos : i = position
  · rw [hi_pos, hbkt_pos]
    have heb : ExpectedBucket s' position = bucket entry s.n := by
      simp [ExpectedBucket, h.keyAt, hremap', hremap, h.n.symm]
    rw [heb]
  · have hicap_s : i < capacity s.n := by simpa [h.n] using hicap
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hi_pos]
      exact hiocc
    have hc := hcorrect i hicap_s hiocc_s
    have heb : ExpectedBucket s' i = ExpectedBucket s i := by
      simp [ExpectedBucket, h.keyAt_other i hi_pos, hremap', hremap, h.n.symm]
    rw [heb]
    unfold BucketAt
    rw [h.dist_other i hi_pos]
    exact hc

-- The base insert write preserves the full cluster invariant (a fresh insert with no
-- resize in progress, at a valid insertion point).
lemma relocateWrite_preserves_clusterInvariant {s s' : State} {entry : Key} {value : Nat}
    {position : Nat}
    (h : RelocateWrite s s' entry value position) (hci : ClusterInvariant s)
    (hip : IsInsertionPoint s position (bucket entry s.n))
    (hremap : s.remapEnd = none) (hremap' : s'.remapEnd = s.remapEnd) :
    ClusterInvariant s' := by
  exact ⟨
    relocateWrite_preserves_distanceValid h hci.1,
    relocateWrite_preserves_clusterOrdered h hci.2.1 hip,
    relocateWrite_preserves_entryAtCorrectBucket h hci.2.2 hremap hremap'
  ⟩

-- A single relocation step keeps the distance invariant: the entry written at `position`
-- has `entryDist ≤ position`, and all other slots are unchanged.
lemma relocateStep_preserves_distanceValid {s s' : State} {entry : Key} {value : Nat}
    {entryDist : Nat} {position : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hiv : DistanceValid s) : DistanceValid s' := by
  intro i hicap hiocc
  by_cases hpos : i = position
  · rw [hpos, h.entryDistAt]
    exact h.entryDist_le
  · have hicap_s : i < capacity s.n := by simpa [h.n] using hicap
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hpos]
      exact hiocc
    have hvalid := hiv i hicap_s hiocc_s
    rw [h.dist_other i hpos]
    exact hvalid

-- A single relocation step keeps the ordered-cluster invariant, assuming `position` is an
-- order boundary for the pending entry's bucket (`position - entryDist`), which is what
-- `find_insert_position` gives for the overflow case.
lemma relocateStep_preserves_clusterOrdered {s s' : State} {entry : Key} {value : Nat}
    {entryDist : Nat} {position : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hco : ClusterOrdered s) (hbound : IsOrderBoundary s position (position - entryDist)) :
    ClusterOrdered s' := by
  have hbkt_pos : BucketAt s' position = position - entryDist := by
    unfold BucketAt
    rw [h.entryDistAt]
  have hbkt_other : ∀ i, i ≠ position → BucketAt s' i = BucketAt s i := by
    intro i hi_ne
    unfold BucketAt
    rw [h.dist_other i hi_ne]
  have hocc_iff : ∀ i, i ≠ position → (IsOccupied s' i ↔ IsOccupied s i) := by
    intro i hi_ne
    unfold IsOccupied
    rw [h.dist_other i hi_ne]
  intro i j hicap_i hicap_j hij hiocc_i hiocc_j
  by_cases h_i_pos : i = position
  · by_cases h_j_pos : j = position
    · exfalso
      exact (ne_of_lt hij) (h_i_pos.trans h_j_pos.symm)
    · rw [h_i_pos, hbkt_pos]
      rw [hbkt_other j h_j_pos]
      have hj_gt : position < j := by simpa [h_i_pos] using hij
      have hicap_sj : j < capacity s.n := by simpa [h.n] using hicap_j
      have hiocc_sj : IsOccupied s j := (hocc_iff j h_j_pos).1 hiocc_j
      exact hbound.2 j hj_gt hicap_sj hiocc_sj
  · by_cases h_j_pos : j = position
    · rw [h_j_pos, hbkt_pos]
      rw [hbkt_other i h_i_pos]
      have hi_lt : i < position := by simpa [h_j_pos] using hij
      have hicap_si : i < capacity s.n := by simpa [h.n] using hicap_i
      have hiocc_si : IsOccupied s i := (hocc_iff i h_i_pos).1 hiocc_i
      exact hbound.1 i hi_lt hiocc_si
    · rw [hbkt_other i h_i_pos, hbkt_other j h_j_pos]
      have hicap_si : i < capacity s.n := by simpa [h.n] using hicap_i
      have hicap_sj : j < capacity s.n := by simpa [h.n] using hicap_j
      have hiocc_si : IsOccupied s i := (hocc_iff i h_i_pos).1 hiocc_i
      have hiocc_sj : IsOccupied s j := (hocc_iff j h_j_pos).1 hiocc_j
      exact hco i j hicap_si hicap_sj hij hiocc_si hiocc_sj

-- A single relocation step keeps each entry at its correct bucket, provided the pending
-- entry is at its home bucket (`position - entryDist = bucket entry s.n`, which the loop
-- maintains) and no resize is in progress. All other entries are unchanged.
lemma relocateStep_preserves_entryAtCorrectBucket {s s' : State} {entry : Key} {value : Nat}
    {entryDist : Nat} {position : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hcorrect : EntryAtCorrectBucket s) (hremap : s.remapEnd = none)
    (hremap' : s'.remapEnd = s.remapEnd) (hbucket : position - entryDist = bucket entry s.n) :
    EntryAtCorrectBucket s' := by
  intro i hicap hiocc
  by_cases hi_pos : i = position
  · rw [hi_pos]
    unfold BucketAt
    rw [h.entryDistAt, hbucket]
    have heb : ExpectedBucket s' position = bucket entry s.n := by
      simp [ExpectedBucket, h.entryAt, hremap', hremap, h.n.symm]
    rw [heb]
  · have hicap_s : i < capacity s.n := by simpa [h.n] using hicap
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hi_pos]
      exact hiocc
    have hc := hcorrect i hicap_s hiocc_s
    have heb : ExpectedBucket s' i = ExpectedBucket s i := by
      simp [ExpectedBucket, h.keyAt_other i hi_pos, hremap', hremap, h.n.symm]
    rw [heb]
    unfold BucketAt
    rw [h.dist_other i hi_pos]
    exact hc

-- A single relocation step preserves the full cluster invariant: even though the displaced
-- entry is in flight, the resulting (partial) table still satisfies the cluster invariant,
-- given the order-boundary, the pending entry being at its home bucket, and no resize.
lemma relocateStep_preserves_clusterInvariant {s s' : State} {entry : Key} {value : Nat}
    {entryDist : Nat} {position : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hci : ClusterInvariant s) (hbound : IsOrderBoundary s position (bucket entry s.n))
    (hremap : s.remapEnd = none) (hremap' : s'.remapEnd = s.remapEnd)
    (hbucket : position - entryDist = bucket entry s.n) :
    ClusterInvariant s' := by
  have hbound' : IsOrderBoundary s position (position - entryDist) := by
    rw [hbucket]
    exact hbound
  exact ⟨
    relocateStep_preserves_distanceValid h hci.1,
    relocateStep_preserves_clusterOrdered h hci.2.1 hbound',
    relocateStep_preserves_entryAtCorrectBucket h hci.2.2 hremap hremap' hbucket
  ⟩

-- The displaced entry stays at its home bucket: after a RelocateStep moves the occupant `t`
-- from `position` (distance `tDist`) to `next` with distance `tDist + (next - position)`, its
-- home bucket `position - tDist` is unchanged.
lemma displaced_home_bucket (position next tDist : Nat) (hpos_le : position ≤ next)
    (htdist_le : tDist ≤ position) :
    next - (tDist + (next - position)) = position - tDist := by
  omega

-- A slot strictly inside `endOfClusterFrom s b i` is occupied and at bucket `b` (it is part
-- of bucket `b`'s cluster being scanned). Proved by strong induction on the scan length.
lemma bucketAt_in_scan_aux (s : State) (b : Nat) :
    ∀ m i i', capacity s.n - i = m → i ≤ i' → i' < endOfClusterFrom s b i → BucketAt s i' = b := by
  intro m
  induction m using Nat.strong_induction_on with
  | h m ih =>
      intro i i' hm hle hlt
      by_cases hicap : i < capacity s.n
      · by_cases hguard : IsOccupied s i ∧ BucketAt s i = b
        · have hrecur : endOfClusterFrom s b i = endOfClusterFrom s b (i + 1) := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          have hlt' : i' < endOfClusterFrom s b (i + 1) := by simpa [hrecur] using hlt
          by_cases hi' : i = i'
          · subst hi'
            exact hguard.2
          · have hle' : i + 1 ≤ i' := by omega
            have hm' : capacity s.n - (i + 1) < m := by omega
            exact ih (capacity s.n - (i + 1)) hm' (i + 1) i' rfl hle' hlt'
        · have heq : endOfClusterFrom s b i = i := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          omega
      · have heq : endOfClusterFrom s b i = i := by
          rw [endOfClusterFrom]
          simp [hicap]
        omega

lemma bucketAt_in_scan (s : State) (b i i' : Nat) (hle : i ≤ i') (hlt : i' < endOfClusterFrom s b i) :
    BucketAt s i' = b :=
  bucketAt_in_scan_aux s b (capacity s.n - i) i i' rfl hle hlt

-- The cluster scan never returns a slot before where it started.
lemma endOfClusterFrom_ge_aux (s : State) (b : Nat) :
    ∀ m i, capacity s.n - i = m → i ≤ endOfClusterFrom s b i := by
  intro m
  induction m using Nat.strong_induction_on with
  | h m ih =>
      intro i hm
      by_cases hicap : i < capacity s.n
      · by_cases hguard : IsOccupied s i ∧ BucketAt s i = b
        · have hrecur : endOfClusterFrom s b i = endOfClusterFrom s b (i + 1) := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          have hm' : capacity s.n - (i + 1) < m := by omega
          have hge : i + 1 ≤ endOfClusterFrom s b (i + 1) :=
            ih (capacity s.n - (i + 1)) hm' (i + 1) rfl
          rw [hrecur]
          omega
        · have heq : endOfClusterFrom s b i = i := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          rw [heq]
      · have heq : endOfClusterFrom s b i = i := by
          rw [endOfClusterFrom]
          simp [hicap]
        rw [heq]

lemma endOfClusterFrom_ge (s : State) (b i : Nat) : i ≤ endOfClusterFrom s b i :=
  endOfClusterFrom_ge_aux s b (capacity s.n - i) i rfl

-- The cluster scan does not pass the end of the table when it starts at or below capacity.
lemma endOfClusterFrom_le_capacity_aux (s : State) (b : Nat) :
    ∀ m i, capacity s.n - i = m → i ≤ capacity s.n → endOfClusterFrom s b i ≤ capacity s.n := by
  intro m
  induction m using Nat.strong_induction_on with
  | h m ih =>
      intro i hm hicap0
      by_cases hicap : i < capacity s.n
      · by_cases hguard : IsOccupied s i ∧ BucketAt s i = b
        · have hrecur : endOfClusterFrom s b i = endOfClusterFrom s b (i + 1) := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          have hm' : capacity s.n - (i + 1) < m := by omega
          have hicap0' : i + 1 ≤ capacity s.n := by omega
          have hle := ih (capacity s.n - (i + 1)) hm' (i + 1) rfl hicap0'
          rw [hrecur]
          exact hle
        · have heq : endOfClusterFrom s b i = i := by
            rw [endOfClusterFrom]
            simp [hicap, hguard]
          rw [heq]
          exact hicap0
      · have heq : endOfClusterFrom s b i = i := by
          rw [endOfClusterFrom]
          simp [hicap]
        rw [heq]
        exact hicap0

lemma endOfClusterFrom_le_capacity (s : State) (b i : Nat) (hicap : i ≤ capacity s.n) :
    endOfClusterFrom s b i ≤ capacity s.n :=
  endOfClusterFrom_le_capacity_aux s b (capacity s.n - i) i rfl hicap

-- The end of the cluster containing `position` is an order boundary for that bucket:
-- every occupied slot below it has a bucket ≤ the cluster's bucket and every occupied slot
-- above it has a bucket ≥ it. This is what makes the relocation step order-preserving.
lemma order_boundary_of_cluster_end {s : State} {position next tDist : Nat}
    (hco : ClusterOrdered s) (hnext : next = endOfCluster s position)
    (hocc : IsOccupied s position) (hdist : s.dist position = tDist)
    (hpos_cap : position < capacity s.n) :
    IsOrderBoundary s next (position - tDist) := by
  have hbkt_pos : BucketAt s position = position - tDist := by
    unfold BucketAt
    rw [hdist]
  have hpos_le_next : position ≤ next := by
    rw [hnext]
    unfold endOfCluster
    exact endOfClusterFrom_ge s (BucketAt s position) position
  have hnext_cap : next ≤ capacity s.n := by
    rw [hnext]
    unfold endOfCluster
    exact endOfClusterFrom_le_capacity s (BucketAt s position) position (Nat.le_of_lt hpos_cap)
  constructor
  · intro i hi_next hiocc_i
    by_cases hi_pos : i = position
    · subst hi_pos
      omega
    · have hi_cap : i < capacity s.n := lt_of_lt_of_le hi_next hnext_cap
      by_cases hi_lt : i < position
      · have hle : BucketAt s i ≤ BucketAt s position :=
          hco i position hi_cap hpos_cap hi_lt hiocc_i hocc
        omega
      · have hi_gt : position < i :=
          Nat.lt_of_le_of_ne (Nat.le_of_not_gt hi_lt) (by intro h; exact hi_pos h.symm)
        have hnext2 : endOfClusterFrom s (position - tDist) position = next := by
          rw [hnext]
          unfold endOfCluster
          rw [hbkt_pos]
        have hscan : i < endOfClusterFrom s (position - tDist) position := by
          rw [hnext2]
          exact hi_next
        have hpos_le_i : position ≤ i := Nat.le_of_lt hi_gt
        have hin : BucketAt s i = position - tDist :=
          bucketAt_in_scan s (position - tDist) position i hpos_le_i hscan
        omega
  · intro i hi_next hicap_i hiocc_i
    have hle : BucketAt s position ≤ BucketAt s i :=
      hco position i hpos_cap hicap_i (lt_of_le_of_lt hpos_le_next hi_next) hocc hiocc_i
    omega

-- A relocation step keeps the order boundary at the displaced cluster's end: the new entry
-- lands before the displaced entry (its bucket `position - entryDist` is ≤ the cluster's
-- bucket `position - tDist`), so `next` is still a boundary in the post-step state.
lemma relocateStep_preserves_order_boundary {s s' : State} {entry : Key} {value : Nat}
    {entryDist tDist position next : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hbnd : IsOrderBoundary s next (position - tDist))
    (hprec : position - entryDist ≤ position - tDist) (hpos_le_next : position ≤ next) :
    IsOrderBoundary s' next (position - tDist) := by
  constructor
  · intro i hi_next hiocc_i
    by_cases hi_pos : i = position
    · subst hi_pos
      unfold BucketAt
      rw [h.entryDistAt]
      exact hprec
    · have hiocc_s : IsOccupied s i := by
        change s.dist i ≠ EMPTY
        rw [← h.dist_other i hi_pos]
        exact hiocc_i
      have hle : BucketAt s i ≤ position - tDist := hbnd.1 i hi_next hiocc_s
      unfold BucketAt
      rw [h.dist_other i hi_pos]
      exact hle
  · intro i hi_next hicap_i hiocc_i
    have hi_ne : i ≠ position := by
      intro h_ip
      have : position < i := lt_of_le_of_lt hpos_le_next hi_next
      omega
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hi_ne]
      exact hiocc_i
    have hicap_s : i < capacity s.n := by simpa [h.n] using hicap_i
    have hle : position - tDist ≤ BucketAt s i := hbnd.2 i hi_next hicap_s hiocc_s
    unfold BucketAt
    rw [h.dist_other i hi_ne]
    exact hle

-- Conditional theorem: a supplied, already-certified `InsertRelocateOK` settled
-- `InsertRelocate` chain preserves `ClusterInvariant` under `remapEnd = none`; it does not prove
-- Rust constructs the certificate, active-remap insertion, or mid-chain `size_up`.
lemma insert_preserves_invariant {s s' : State} {key : Key} {value : Nat} {position : Nat}
    {h : InsertRelocate s s' key value position} (hok : InsertRelocateOK h)
    (hci : ClusterInvariant s) (hremap : s.remapEnd = none) (hremap' : s'.remapEnd = s.remapEnd) :
    ClusterInvariant s' := by
  induction hok with
  | done hw hslot hbound =>
      exact relocateWrite_preserves_clusterInvariant hw hci ⟨hslot, hbound.1, hbound.2⟩ hremap hremap'
  | step mid entryDist hstep hnext hbound hbucket _hprec hok_next ih =>
      have hci_mid : ClusterInvariant mid :=
        relocateStep_preserves_clusterInvariant hstep hci hbound hremap hstep.remapEnd hbucket
      have hremap_mid : mid.remapEnd = none :=
        hstep.remapEnd.trans hremap
      have hremap'_mid :=
        hremap'.trans hstep.remapEnd.symm
      exact ih hci_mid hremap_mid hremap'_mid

lemma remove_preserves_invariant (h : ClusterInvariant s) (hstep : UnRelocateStep s s' position) :
    ClusterInvariant s' := by
  -- `UnRelocateStep` lacks the metadata and relocation-chain facts needed to prove removal preserves the invariant (src/map.rs L491-L520).
  -- For every supplied key, Counterexamples.lean proves a machine-checked violating
  -- witness; if `Key` is empty, `UnRelocateStep` is uninhabited and that witness is
  -- unavailable, so the counterexample requires an inhabited key domain.
  sorry

lemma remap_step_preserves_invariant (h : ClusterInvariant s) (hstep : RemapStep s s') :
    ClusterInvariant s' := by
  -- `RemapStep` postulates only `keySet` and `len`, which underconstrains invariant preservation (src/map.rs L546-L596).
  sorry

/-!
## Target (c) — predicate-level re-open consistency

`ReopenConsistent` is an abstract state predicate: under `ClusterInvariant`, `KeySet` is
equivalent to the existential `LookupFound` predicate through `EntryAtCorrectBucket`.
The theorem below is kernel-checked at that level only. It does not refine the actual
`lookupIndex`, a persisted memory image, or `init` / re-open behavior.

The implementation boundary is `src/map.rs L107-L126, L349-L370`; this theorem does
not establish that those concrete lookup or re-open paths realize the predicate.
-/

lemma reopen_consistent_of_cluster_invariant (h : ClusterInvariant s) :
    ReopenConsistent s := by
  refine ⟨h, ?_⟩
  intro k
  constructor
  · rintro ⟨i, hicap, hiocc, hkey⟩
    exact ⟨i, hicap, hiocc, hkey, h.2.2 i hicap hiocc⟩
  · rintro ⟨i, hicap, hiocc, hkey, _⟩
    exact ⟨i, hicap, hiocc, hkey⟩

end StableClusterAudit
