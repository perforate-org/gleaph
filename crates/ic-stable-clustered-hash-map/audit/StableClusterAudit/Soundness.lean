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

-- A single relocation step is an *intermediate* state (the displaced entry is in flight, not
-- yet written), so it does not by itself preserve `ClusterInvariant`: the full `insert` is a
-- chain of such steps terminated by a `RelocateWrite`, and the invariant is preserved by the
-- whole chain. Proving that requires an induction over the chain length, with each step
-- keeping the ordered-cluster structure across the displaced region; that induction is the
-- remaining core of target (b) and is deferred here.
lemma insert_preserves_invariant (h : ClusterInvariant s)
    (hstep : RelocateStep s s' entry value entryDist position) : ClusterInvariant s' := by
  sorry

lemma remove_preserves_invariant (h : ClusterInvariant s) (hstep : UnRelocateStep s s' position) :
    ClusterInvariant s' := by
  sorry

lemma remap_step_preserves_invariant (h : ClusterInvariant s) (hstep : RemapStep s s') :
    ClusterInvariant s' := by
  sorry

/-!
## Target (c) — re-open consistency (src/map.rs L107-L126, L349-L370)

A persisted state read back by `init` must satisfy the cluster invariant and its lookup
must find exactly the stored keys. The `lookupIndex` transcription in `Map.lean` scans
the new table then the mixed range; proving it finds exactly `KeySet` relies on the
cluster invariant (target (b)) being established, which is not yet discharged.
-/

lemma reopen_consistent_of_cluster_invariant (h : ClusterInvariant s) :
    ReopenConsistent s := by
  sorry

end StableClusterAudit
