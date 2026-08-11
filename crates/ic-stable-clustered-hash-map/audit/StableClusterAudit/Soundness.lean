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
## Fresh-table base state

`new` clears the complete table before the first insert. The following lemmas establish the
initial invariant, no-holes, and length/cardinality conditions used by insertion-history proofs.
-/

-- src/map.rs L135-L141 (`new` writes the header and clears every slot).
lemma freshState_clusterInvariant (n : Nat) : ClusterInvariant (freshState n) := by
  refine ⟨?_, ?_, ?_⟩
  · intro i hi hocc
    simp [freshState, IsOccupied] at hocc
  · intro i j hi hj hij hiocc hjocc
    simp [freshState, IsOccupied] at hiocc
  · intro i hi hocc
    simp [freshState, IsOccupied] at hocc

-- src/map.rs L135-L141 (`clear_region` writes `EMPTY` to every slot).
lemma freshState_noHoles (n : Nat) : NoHoles (freshState n) := by
  intro i k hi hocc hkey
  simp [freshState, IsOccupied] at hocc

-- src/map.rs L135-L141 (`new` initializes `len` to zero and clears every slot).
lemma freshState_lenCoherent (n : Nat) : LenCoherent (freshState n) := by
  simp [LenCoherent, OccupiedSlots, freshState, IsOccupied]

-- src/map.rs L135-L141 (a freshly cleared table contains no key).
lemma freshState_keySet_empty (n : Nat) (k : Key) : ¬ KeySet (freshState n) k := by
  rintro ⟨i, hi, hocc, hkey⟩
  simp [freshState, IsOccupied] at hocc

/-!
## Target (a) — `size_up` preserves the entry set and count

`size_up` (src/map.rs L526-L554) grows the table in place: the old region keeps its
keys/values/distances verbatim, and the newly grown region is cleared (`clear_region`,
src/map.rs L542-L549). `SizeUp` states exactly this, so the entry set is unchanged.
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
## Target (a) continued — `remap` preserves the entry set (src/map.rs L559-L597)

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
## Target (b) — cluster invariant preserved by mutations (src/map.rs L421-L520)

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

-- A terminating insert write preserves the no-holes scan condition when the insertion-point
-- prefix is occupied. The explicit prefix fact is the part not carried by `RelocateWrite` or
-- `InsertRelocateOK`; `find_insert_position` supplies it in the Rust loop. src/map.rs L309-L319,
-- L464-L466.
lemma relocateWrite_preserves_noHoles {s s' : State} {entry : Key} {value : Nat} {position : Nat}
    (h : RelocateWrite s s' entry value position) (hno : NoHoles s)
    (_hpos : position < capacity s.n)
    (hprefix : ∀ j, bucket entry s.n ≤ j → j < position → IsOccupied s j) :
    NoHoles s' := by
  intro i k hicapi hiocc hkeyi j hkj hji
  have hn : s'.n = s.n := h.n.symm
  by_cases hipos : i = position
  · subst i
    have hkey : k = entry := by
      apply Option.some.inj
      calc
        some k = s'.keyAt position := hkeyi.symm
        _ = some entry := h.keyAt
    have hkj' : bucket entry s.n ≤ j := by simpa [hkey, hn] using hkj
    have hsource := hprefix j hkj' hji
    change s'.dist j ≠ EMPTY
    rw [h.dist_other j (by omega)]
    exact hsource
  · have hicapi_s : i < capacity s.n := by simpa [hn] using hicapi
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hipos]
      exact hiocc
    have hkey_s : s.keyAt i = some k := by
      rw [← h.keyAt_other i hipos]
      exact hkeyi
    have hkj_s : bucket k s.n ≤ j := by simpa [hn] using hkj
    have hsource := hno i k hicapi_s hiocc_s hkey_s j hkj_s hji
    by_cases hjpos : j = position
    · subst j
      change s'.dist position ≠ EMPTY
      rw [h.dist]
      exact ne_of_lt h.distFit
    · change s'.dist j ≠ EMPTY
      rw [h.dist_other j hjpos]
      exact hsource

-- A relocation step also preserves `NoHoles` when the pending entry's home-to-position prefix
-- is occupied and its written distance is a real (non-EMPTY) distance. The displaced occupant
-- is pending rather than present in the intermediate state, so this lemma is intentionally about
-- the slots that the step has actually written. src/map.rs L468-L478.
lemma relocateStep_preserves_noHoles {s s' : State} {entry : Key} {value : Nat}
    {entryDist position : Nat} (h : RelocateStep s s' entry value entryDist position)
    (hno : NoHoles s) (_hpos : position < capacity s.n)
    (hprefix : ∀ j, bucket entry s.n ≤ j → j < position → IsOccupied s j)
    (hentry : entryDist < EMPTY) : NoHoles s' := by
  intro i k hicapi hiocc hkeyi j hkj hji
  have hn : s'.n = s.n := h.n.symm
  by_cases hipos : i = position
  · subst i
    have hkey : k = entry := by
      apply Option.some.inj
      calc
        some k = s'.keyAt position := hkeyi.symm
        _ = some entry := h.entryAt
    have hkj' : bucket entry s.n ≤ j := by simpa [hkey, hn] using hkj
    have hsource := hprefix j hkj' hji
    change s'.dist j ≠ EMPTY
    rw [h.dist_other j (by omega)]
    exact hsource
  · have hicapi_s : i < capacity s.n := by simpa [hn] using hicapi
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.dist_other i hipos]
      exact hiocc
    have hkey_s : s.keyAt i = some k := by
      rw [← h.keyAt_other i hipos]
      exact hkeyi
    have hkj_s : bucket k s.n ≤ j := by simpa [hn] using hkj
    have hsource := hno i k hicapi_s hiocc_s hkey_s j hkj_s hji
    by_cases hjpos : j = position
    · subst j
      change s'.dist position ≠ EMPTY
      rw [h.entryDistAt]
      exact ne_of_lt hentry
    · change s'.dist j ≠ EMPTY
      rw [h.dist_other j hjpos]
      exact hsource

-- The explicit prefix certificate composes the terminating and intermediate cases across the
-- entire insert relocation chain. It proves the structural scan condition, but does not yet
-- derive the per-step prefixes from Rust's `find_insert_position` loop. src/map.rs L447-L487.
lemma insertRelocate_preserves_noHoles {s s' : State} {key : Key} {value : Nat}
    {position : Nat} {h : InsertRelocate s s' key value position}
    (hok : InsertRelocateNoHolesOK h) (hno : NoHoles s) : NoHoles s' := by
  induction hok with
  | done hw hpos hprefix =>
      exact relocateWrite_preserves_noHoles hw hno hpos hprefix
  | step mid entryDist hstep hnext hpos hprefix hentry hok ih =>
      have hno_mid : NoHoles mid :=
        relocateStep_preserves_noHoles hstep hno hpos hprefix hentry
      exact ih hno_mid

-- The Rust `find_insert_position` scan supplies the occupied-prefix premise for the first
-- pending entry: every slot it traverses before returning is occupied and belongs no later than
-- the requested bucket. This lemma extracts only the occupancy part needed by `NoHoles`.
-- src/map.rs L309-L319.
lemma findInsertPositionFrom_prefix (s : State) (b i : Nat) :
    ∀ j, i ≤ j → j < findInsertPositionFrom s b i →
      j < capacity s.n ∧ IsOccupied s j ∧ BucketAt s j ≤ b := by
  intro j hij hjout
  rw [findInsertPositionFrom.eq_1] at hjout
  by_cases hcap : i < capacity s.n
  · by_cases hcontinue : IsOccupied s i ∧ BucketAt s i ≤ b
    · simp only [if_pos hcap, if_pos hcontinue] at hjout
      by_cases hji : j = i
      · subst j
        exact ⟨hcap, hcontinue.1, hcontinue.2⟩
      · have hrec := findInsertPositionFrom_prefix s b (i + 1) j (by omega) hjout
        exact hrec
    · simp only [if_pos hcap, if_neg hcontinue] at hjout
      omega
  · simp only [if_neg hcap] at hjout
    omega
termination_by capacity s.n - i
decreasing_by omega

lemma findInsertPosition_prefix {s : State} {b position : Nat}
    (hpos : findInsertPosition s b = position) :
    ∀ j, b ≤ j → j < position → IsOccupied s j := by
  intro j hbj hjpos
  have hscan : j < findInsertPosition s b := by simpa [hpos] using hjpos
  have hscan' : j < findInsertPositionFrom s b b := by
    simpa [findInsertPosition] using hscan
  exact (findInsertPositionFrom_prefix s b b j hbj hscan').2.1

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
lemma slot_in_scan_aux (s : State) (b : Nat) :
    ∀ m i i', capacity s.n - i = m → i ≤ i' → i' < endOfClusterFrom s b i →
      IsOccupied s i' ∧ BucketAt s i' = b := by
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
            exact hguard
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
  (slot_in_scan_aux s b (capacity s.n - i) i i' rfl hle hlt).2

lemma occupied_in_scan (s : State) (b i i' : Nat) (hle : i ≤ i')
    (hlt : i' < endOfClusterFrom s b i) : IsOccupied s i' :=
  (slot_in_scan_aux s b (capacity s.n - i) i i' rfl hle hlt).1

-- Once a pending entry displaces an occupied slot, the next pending entry's prefix is also
-- occupied: `NoHoles` covers the range before the current position, while the end-of-cluster
-- scan covers the range from the current position to `next`. The copied slot at `position`
-- is occupied in the intermediate state, and all later scanned slots are unchanged.
-- src/map.rs L468-L476.
lemma relocateStep_next_prefix_of_noHoles {s mid : State} {entry : Key} {value entryDist position : Nat}
    (h : RelocateStep s mid entry value entryDist position)
    (hno : NoHoles s) (hpos : position < capacity s.n) (hentry : entryDist < EMPTY) :
    ∀ j, bucket h.tKey mid.n ≤ j → j < h.next → IsOccupied mid j := by
  intro j hkj hjnext
  have htDist_lt : h.tDist < EMPTY := by
    exact Nat.lt_of_le_of_lt (Nat.le_add_right _ _) h.tDistShifted
  have hocc_pos : IsOccupied s position := by
    change s.dist position ≠ EMPTY
    rw [h.distT]
    exact ne_of_lt htDist_lt
  have hkj_s : bucket h.tKey s.n ≤ j := by
    simpa [h.n] using hkj
  by_cases hj_lt : j < position
  · have hsource := hno position h.tKey hpos hocc_pos h.occT j hkj_s hj_lt
    change mid.dist j ≠ EMPTY
    rw [h.dist_other j (by omega)]
    exact hsource
  · have hj_ge : position ≤ j := Nat.le_of_not_gt hj_lt
    by_cases hj_eq : j = position
    · subst j
      change mid.dist position ≠ EMPTY
      rw [h.entryDistAt]
      exact ne_of_lt hentry
    · have hnext_scan : j < endOfCluster s position := by
        simpa [h.next_is_end] using hjnext
      have hscan : j < endOfClusterFrom s (BucketAt s position) position := by
        simpa [endOfCluster] using hnext_scan
      have hsource := occupied_in_scan s (BucketAt s position) position j hj_ge hscan
      change mid.dist j ≠ EMPTY
      rw [h.dist_other j hj_eq]
      exact hsource

-- The occupancy certificate supplies the position and distance bounds needed to thread the
-- prefix proof through every relocation step. The first prefix is supplied by the initial
-- scan; each recursive prefix is derived by `relocateStep_next_prefix_of_noHoles`.
lemma insertRelocateNoHolesOK_of_occupancyOK {s s' : State} {key : Key} {value : Nat}
    {position : Nat} {h : InsertRelocate s s' key value position}
    (hok : InsertRelocateOccupancyOK h) (hno : NoHoles s)
    (hprefix : ∀ j, bucket key s.n ≤ j → j < position → IsOccupied s j) :
    InsertRelocateNoHolesOK h := by
  induction hok with
  | done hw hpos =>
      exact InsertRelocateNoHolesOK.done hw hpos hprefix
  | step mid entryDist hstep hnext hpos hentry hok ih =>
      have hno_mid : NoHoles mid :=
        relocateStep_preserves_noHoles hstep hno hpos hprefix hentry
      have hprefix_next :
          ∀ j, bucket hstep.tKey mid.n ≤ j → j < hstep.next → IsOccupied mid j :=
        relocateStep_next_prefix_of_noHoles hstep hno hpos hentry
      exact InsertRelocateNoHolesOK.step mid entryDist hstep hnext hpos hprefix hentry
        (ih hno_mid hprefix_next)

lemma insertRelocate_preserves_noHoles_of_occupancyOK {s s' : State} {key : Key} {value : Nat}
    {position : Nat} {h : InsertRelocate s s' key value position}
    (hok : InsertRelocateOccupancyOK h) (hno : NoHoles s)
    (hprefix : ∀ j, bucket key s.n ≤ j → j < position → IsOccupied s j) :
    NoHoles s' := by
  exact insertRelocate_preserves_noHoles
    (insertRelocateNoHolesOK_of_occupancyOK hok hno hprefix) hno

-- The public scan supplies the initial prefix directly. With the existing occupancy
-- certificate, this closes the complete-chain `NoHoles` bridge for a settled insert.
lemma insertRelocate_preserves_noHoles_of_findPosition
    {s s' : State} {key : Key} {value : Nat} {position : Nat}
    {h : InsertRelocate s s' key value position}
    (hscan : findInsertPosition s (bucket key s.n) = position)
    (hok : InsertRelocateOccupancyOK h) (hno : NoHoles s) :
    NoHoles s' := by
  have hprefix : ∀ j, bucket key s.n ≤ j → j < position → IsOccupied s j :=
    findInsertPosition_prefix hscan
  exact insertRelocate_preserves_noHoles_of_occupancyOK hok hno hprefix

-- A no-resize execution trace projects to the existing weak relocation-chain relation.
-- The strict position bounds exclude the `size_up` branch; the projection intentionally erases
-- the execution-only distance and bound evidence. src/map.rs L460-L488.
theorem insertRelocateOfTrace {s s' : State} {key : Key} {value pendingDist position : Nat}
    (htrace : InsertRelocateTrace s s' key value pendingDist position) :
    InsertRelocate s s' key value position := by
  induction htrace with
  | done hw _hpos _hentry _hdist =>
      exact InsertRelocate.done hw
  | step mid hstep _hpos _hentry _nextDist _hnextDist _hprogress _hnextPos hnext ih =>
      exact InsertRelocate.step mid _ hstep ih

-- The same trace constructs the occupancy certificate needed by the cardinality and `NoHoles`
-- bridges: terminal writes are in bounds, and every intermediate pending distance is real.
-- src/map.rs L467-L488.
lemma insertRelocateOccupancyOK_of_trace
    {s s' : State} {key : Key} {value pendingDist position : Nat}
    (htrace : InsertRelocateTrace s s' key value pendingDist position) :
    InsertRelocateOccupancyOK (insertRelocateOfTrace htrace) := by
  induction htrace with
  | done hw hpos _hentry _hdist =>
      exact InsertRelocateOccupancyOK.done hw hpos
  | step mid hstep hpos hentry _nextDist _hnextDist _hprogress _hnextPos hnext ih =>
      exact InsertRelocateOccupancyOK.step mid _ hstep (insertRelocateOfTrace hnext)
        hpos hentry ih

-- A successful settled `lookupIndex` scan returns an in-bounds occupied slot with the
-- requested key at its expected bucket. This is the concrete scan-result fact consumed by
-- the public-remove certificate; it does not prove that every stored key is found.
-- src/map.rs L325-L372.
lemma scanFor_some_aux (s : State) (key : Key) (b : Nat) :
    ∀ m start i, capacity s.n - start = m →
      scanFor s key b start = some i →
      i < capacity s.n ∧ IsOccupied s i ∧ BucketAt s i = b ∧ s.keyAt i = some key := by
  intro m
  induction m using Nat.strong_induction_on with
  | h m ih =>
      intro start i hm hscan
      rw [scanFor] at hscan
      by_cases hstart : start < capacity s.n
      · simp only [hstart, ↓reduceIte] at hscan
        by_cases hempty : s.dist start = EMPTY
        · simp [hempty] at hscan
        · simp only [hempty, ↓reduceIte] at hscan
          by_cases hgreater : BucketAt s start > b
          · simp [hgreater] at hscan
          · simp only [hgreater, ↓reduceIte] at hscan
            by_cases hmatch : BucketAt s start = b ∧ s.keyAt start = some key
            · simp [hmatch] at hscan
              subst i
              exact ⟨hstart, hempty, hmatch.1, hmatch.2⟩
            · simp [hmatch] at hscan
              have hm' : capacity s.n - (start + 1) < m := by omega
              exact ih (capacity s.n - (start + 1)) hm' (start + 1) i rfl hscan
      · simp [hstart] at hscan

lemma lookupIndex_some_implies_lookupFound {s : State} {key : Key} {i : Nat}
    (hremap : s.remapEnd = none) (hlookup : lookupIndex s key = some i) :
    LookupFound s key := by
  unfold lookupIndex at hlookup
  by_cases hlen : s.len = 0
  · simp [hlen] at hlookup
  · simp only [hlen, ↓reduceIte] at hlookup
    simp only [hremap] at hlookup
    cases hscan : scanFor s key (bucket key s.n) (bucket key s.n) with
    | none => simp [hscan] at hlookup
    | some j =>
        have hs := scanFor_some_aux s key (bucket key s.n)
          (capacity s.n - bucket key s.n) (bucket key s.n) j rfl hscan
        simp [hscan] at hlookup
        have hj : j = i := hlookup
        subst j
        refine ⟨i, hs.1, hs.2.1, hs.2.2.2, ?_⟩
        simpa [ExpectedBucket, hs.2.2.2, hremap] using hs.2.2.1

lemma publicRemoveSettled_lookupFound {s s' : State} {key : Key}
    (h : PublicRemoveSettled s s' key) : LookupFound s key := by
  cases h with
  | found hlookup hsettled _relocate _setLen =>
      exact lookupIndex_some_implies_lookupFound hsettled hlookup

lemma len_pos_of_lenCoherent_keySet {s : State} {k : Key}
    (hlen : LenCoherent s) (hkeyset : KeySet s k) : 0 < s.len := by
  rcases hkeyset with ⟨i, hicapi, hoci, _hkey⟩
  rw [hlen]
  apply Finset.card_pos.mpr
  refine ⟨i, ?_⟩
  simp [OccupiedSlots, hicapi, hoci]

-- A single terminating insert write changes the occupied-slot cardinality by one: the source
-- position is empty, the target position is occupied, and all other distances are unchanged.
-- src/map.rs L464-L466 (`write_entry` at an empty insertion position).
lemma occupiedSlots_card_after_empty_write {s s' : State} {position : Nat}
    (hn : s'.n = s.n) (hpos : position < capacity s.n)
    (hslot : s.dist position = EMPTY) (hnew : s'.dist position ≠ EMPTY)
    (hother : ∀ i, i ≠ position → s'.dist i = s.dist i) :
    (OccupiedSlots s').card = (OccupiedSlots s).card + 1 := by
  have hslots : OccupiedSlots s' = insert position (OccupiedSlots s) := by
    ext i
    by_cases hi : i = position
    · subst i
      simp [OccupiedSlots, hn, hpos, hslot, hnew]
    · simp [OccupiedSlots, hn, hi, hother i hi]
  have hnot : position ∉ OccupiedSlots s := by
    simp [OccupiedSlots, hpos, hslot]
  rw [hslots, Finset.card_insert_of_notMem hnot]

-- A relocation step overwrites an occupied slot with another occupied entry and therefore keeps
-- the occupied-slot cardinality unchanged. src/map.rs L468-L478 (`write_entry` over a hit slot).
lemma occupiedSlots_card_after_occupied_write {s s' : State} {position : Nat}
    (hn : s'.n = s.n) (hpos : position < capacity s.n)
    (hsrc : s.dist position ≠ EMPTY) (hnew : s'.dist position ≠ EMPTY)
    (hother : ∀ i, i ≠ position → s'.dist i = s.dist i) :
    (OccupiedSlots s').card = (OccupiedSlots s).card := by
  have hslots : OccupiedSlots s' = OccupiedSlots s := by
    ext i
    by_cases hi : i = position
    · subst i
      simp [OccupiedSlots, hn, hpos, hsrc, hnew]
    · simp [OccupiedSlots, hn, hother i hi]
  exact congrArg Finset.card hslots

-- Each certified insert chain fills exactly one previously empty slot. The occupancy certificate
-- is intentionally separate from `InsertRelocateOK`: the existing order certificate does not
-- itself carry the in-bounds/non-empty facts required for this cardinality argument.
-- src/map.rs L447-L487.
lemma insertRelocate_preserves_occupiedCard {s s' : State} {key : Key} {value : Nat}
    {position : Nat} {h : InsertRelocate s s' key value position}
    (hok : InsertRelocateOccupancyOK h) :
    (OccupiedSlots s').card = (OccupiedSlots s).card + 1 := by
  induction hok with
  | done hw hpos =>
      apply occupiedSlots_card_after_empty_write hw.n.symm hpos hw.slotEmpty
      · rw [hw.dist]
        exact ne_of_lt hw.distFit
      · exact hw.dist_other
  | step mid entryDist hstep hnext hpos hentry hok ih =>
      have hstep_card :=
        occupiedSlots_card_after_occupied_write hstep.n.symm hpos
          (by
            rw [hstep.distT]
            have hsum := hstep.tDistShifted
            omega)
          (by
            rw [hstep.entryDistAt]
            exact ne_of_lt hentry)
          hstep.dist_other
      rw [ih, hstep_card]

-- The public insert length update is coherent once the preceding relocation chain carries the
-- occupancy certificate. This is a settled, certificate-level bridge; Rust construction of both
-- certificates and active-remap insertion remain outside the theorem. src/map.rs L424-L451.
lemma publicInsertSettled_preserves_lenCoherent {s mid s' : State} {key : Key} {value : Nat}
    {position : Nat} {h : InsertRelocate s mid key value position}
    (hok : InsertRelocateOccupancyOK h) (hlen : LenCoherent s)
    (hheader : mid.len = s.len)
    (setLen : s' = {mid with len := mid.len + 1}) : LenCoherent s' := by
  subst s'
  unfold LenCoherent at hlen ⊢
  have hcard := insertRelocate_preserves_occupiedCard hok
  change mid.len + 1 = (OccupiedSlots mid).card
  omega

-- Lookup completeness needs the separately stated `NoHoles` condition and a positive
-- `LenCoherent` condition because the abstract `State` otherwise leaves `len` independent
-- from the occupied-slot count.
-- src/map.rs L325-L372.
lemma scanFor_complete_aux (s : State) (key : Key) (b : Nat) (hci : ClusterInvariant s)
    (hno : NoHoles s) (hb : b = bucket key s.n) :
    ∀ d start i, i - start = d → b ≤ start → start ≤ i → i < capacity s.n →
      IsOccupied s i → s.keyAt i = some key → BucketAt s i = b →
      ∃ j, scanFor s key b start = some j := by
  intro d
  induction d using Nat.strong_induction_on with
  | h d ih =>
      intro start i hdist hbst hsi hicapi hoci hkeyi hbucketi
      have hstartcap : start < capacity s.n := lt_of_le_of_lt hsi hicapi
      by_cases hsame : start = i
      · subst i
        unfold scanFor
        simp [hstartcap, hoci, hbucketi, hkeyi]
      · have hstartlt : start < i := by omega
        have hbst' : bucket key s.n ≤ start := by simpa [hb] using hbst
        have hoccstart : IsOccupied s start :=
          hno i key hicapi hoci hkeyi start hbst' hstartlt
        have hbucketstart : BucketAt s start ≤ b := by
          have hord := hci.2.1 start i hstartcap hicapi hstartlt hoccstart hoci
          omega
        unfold scanFor
        simp only [hstartcap, ↓reduceIte]
        have hdiststart : s.dist start ≠ EMPTY := hoccstart
        simp only [hdiststart, ↓reduceIte]
        have hnotgreater : ¬BucketAt s start > b := by omega
        simp only [hnotgreater, ↓reduceIte]
        by_cases hmatch : BucketAt s start = b ∧ s.keyAt start = some key
        · exact ⟨start, by simp [hmatch]⟩
        · have hnextle : start + 1 ≤ i := by omega
          have hnextbst : b ≤ start + 1 := by omega
          have hnextd : i - (start + 1) < d := by omega
          have hrec := ih (i - (start + 1)) hnextd (start + 1) i rfl
            hnextbst hnextle hicapi hoci hkeyi hbucketi
          simp [hmatch, hrec]

lemma lookupIndex_complete_of_noHoles {s : State}
    (hci : ClusterInvariant s) (hno : NoHoles s) (hlen : LenCoherent s)
    (hremap : s.remapEnd = none) :
    ∀ k, KeySet s k → ∃ i, lookupIndex s k = some i := by
  intro k hkeyset
  rcases hkeyset with ⟨i, hicapi, hoci, hkeyi⟩
  have hlenPos : 0 < s.len := len_pos_of_lenCoherent_keySet hlen ⟨i, hicapi, hoci, hkeyi⟩
  have hbucketi : BucketAt s i = bucket k s.n := by
    have hcorrect := hci.2.2 i hicapi hoci
    simpa [ExpectedBucket, hkeyi, hremap] using hcorrect
  have hb_i : bucket k s.n ≤ i := by
    have hdist := hci.1 i hicapi hoci
    unfold BucketAt at hbucketi
    omega
  obtain ⟨j, hscan⟩ := scanFor_complete_aux s k (bucket k s.n) hci hno rfl
    (i - bucket k s.n) (bucket k s.n) i rfl le_rfl hb_i hicapi hoci hkeyi hbucketi
  refine ⟨j, ?_⟩
  unfold lookupIndex
  simp [hlenPos.ne', hscan]

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

-- The bounded helper prevents a target-only slot from becoming in-bounds through a header
-- change. It is intentionally narrower than removal invariant preservation: the relation
-- still lacks the faithful continue/stop chain and the slot facts needed for that proof.
-- src/map.rs L504-L520.
lemma unrelocateStepWithStableHeader_preserves_inBounds {s s' : State} {position i : Nat}
    (h : UnRelocateStepWithStableHeader s s' position) : InBounds s i ↔ InBounds s' i := by
  unfold InBounds
  rw [h.header.n]

-- Clearing the final hole only removes one occupied slot. All remaining occupied slots
-- retain their distances, ordering, keys, and active-remap interpretation.
-- src/map.rs L506-L513.
lemma clearCurrentHole_preserves_clusterInvariant {s s' : State} {position : Nat}
    (h : ClearCurrentHole s s' position) (hci : ClusterInvariant s) :
    ClusterInvariant s' := by
  have hn : s'.n = s.n := h.frame.header.n
  have hremap : s'.remapEnd = s.remapEnd := h.frame.header.remapEnd
  constructor
  · intro i hicap hiocc
    have hi_ne : i ≠ position := by
      intro hi
      subst i
      exact hiocc h.distAt_position
    have hicap_s : i < capacity s.n := by simpa [hn] using hicap
    have hiocc_s : IsOccupied s i := by
      change s.dist i ≠ EMPTY
      rw [← h.frame.dist_other i hi_ne]
      exact hiocc
    rw [h.frame.dist_other i hi_ne]
    exact hci.1 i hicap_s hiocc_s
  · constructor
    · intro i j hicap_i hicap_j hij hiocc_i hiocc_j
      have hi_ne : i ≠ position := by
        intro hi
        subst i
        exact hiocc_i h.distAt_position
      have hj_ne : j ≠ position := by
        intro hj
        subst j
        exact hiocc_j h.distAt_position
      have hicap_si : i < capacity s.n := by simpa [hn] using hicap_i
      have hicap_sj : j < capacity s.n := by simpa [hn] using hicap_j
      have hiocc_si : IsOccupied s i := by
        change s.dist i ≠ EMPTY
        rw [← h.frame.dist_other i hi_ne]
        exact hiocc_i
      have hiocc_sj : IsOccupied s j := by
        change s.dist j ≠ EMPTY
        rw [← h.frame.dist_other j hj_ne]
        exact hiocc_j
      unfold BucketAt
      rw [h.frame.dist_other i hi_ne, h.frame.dist_other j hj_ne]
      exact hci.2.1 i j hicap_si hicap_sj hij hiocc_si hiocc_sj
    · intro i hicap hiocc
      have hi_ne : i ≠ position := by
        intro hi
        subst i
        exact hiocc h.distAt_position
      have hicap_s : i < capacity s.n := by simpa [hn] using hicap
      have hiocc_s : IsOccupied s i := by
        change s.dist i ≠ EMPTY
        rw [← h.frame.dist_other i hi_ne]
        exact hiocc
      have hc := hci.2.2 i hicap_s hiocc_s
      have hexpected : ExpectedBucket s' i = ExpectedBucket s i := by
        simp [ExpectedBucket, h.frame.keyAt_other i hi_ne, hremap, hn]
      rw [hexpected]
      unfold BucketAt
      rw [h.frame.dist_other i hi_ne]
      exact hc

-- In the continue branch, the copied tail and its stale source slot have the same home
-- bucket. Under a settled header, both therefore satisfy the same expected-bucket rule;
-- the copied slot also remains between the surrounding ordered clusters.
-- src/map.rs L515-L520.
lemma removeContinue_preserves_clusterInvariant {s s' : State} {position next : Nat}
    (h : RemoveContinue s s' position next) (hci : ClusterInvariant s)
    (hremap : s.remapEnd = none) : ClusterInvariant s' := by
  have hn : s'.n = s.n := h.frame.header.n
  have hremap' : s'.remapEnd = none := h.frame.header.remapEnd.trans hremap
  have hstart_le_next : position + 1 ≤ next := Nat.add_one_le_iff.mpr h.position_lt_next
  have htail :
      next = endOfClusterFrom s (BucketAt s (position + 1)) (position + 1) - 1 := by
    simpa [tailOfCluster, endOfCluster] using h.next_is_tail
  have hend_pos :
      0 < endOfClusterFrom s (BucketAt s (position + 1)) (position + 1) := by
    omega
  have hend :
      endOfClusterFrom s (BucketAt s (position + 1)) (position + 1) = next + 1 := by
    omega
  have hnext_scan :
      next < endOfClusterFrom s (BucketAt s (position + 1)) (position + 1) := by
    omega
  have hnext_occ : IsOccupied s next :=
    occupied_in_scan s (BucketAt s (position + 1)) (position + 1) next
      hstart_le_next hnext_scan
  have hnext_valid : s.dist next ≤ next :=
    hci.1 next h.next_lt_capacity hnext_occ
  have hbucket_position : BucketAt s' position = BucketAt s next := by
    unfold BucketAt
    rw [h.distAt_position]
    have hshift_le_position : s.dist next - (next - position) ≤ position := by omega
    have hdist_parts : s.dist next - (next - position) + (next - position) = s.dist next :=
      Nat.sub_add_cancel h.shift_le_tailDist
    have hposition_parts :
        position - (s.dist next - (next - position)) +
            (s.dist next - (next - position)) = position :=
      Nat.sub_add_cancel hshift_le_position
    have hnext_parts : next - s.dist next + s.dist next = next :=
      Nat.sub_add_cancel hnext_valid
    omega
  constructor
  · intro i hicap hiocc
    by_cases hi_pos : i = position
    · subst i
      rw [h.distAt_position]
      omega
    · have hicap_s : i < capacity s.n := by simpa [hn] using hicap
      have hiocc_s : IsOccupied s i := by
        change s.dist i ≠ EMPTY
        rw [← h.frame.dist_other i hi_pos]
        exact hiocc
      rw [h.frame.dist_other i hi_pos]
      exact hci.1 i hicap_s hiocc_s
  · constructor
    · intro i j hicap_i hicap_j hij hiocc_i hiocc_j
      by_cases hi_pos : i = position
      · subst i
        rw [hbucket_position]
        by_cases hj_le_next : j ≤ next
        · have hstart_le_j : position + 1 ≤ j := by omega
          have hj_scan :
              j < endOfClusterFrom s (BucketAt s (position + 1)) (position + 1) := by
            omega
          have hbucket_j_scan :=
            bucketAt_in_scan s (BucketAt s (position + 1)) (position + 1) j
              hstart_le_j hj_scan
          have hbucket_next_scan :=
            bucketAt_in_scan s (BucketAt s (position + 1)) (position + 1) next
              hstart_le_next hnext_scan
          have hj_ne : j ≠ position := by omega
          unfold BucketAt at hbucket_j_scan hbucket_next_scan ⊢
          rw [h.frame.dist_other j hj_ne]
          omega
        · have hnext_lt_j : next < j := Nat.lt_of_not_ge hj_le_next
          have hj_ne : j ≠ position := by omega
          have hicap_sj : j < capacity s.n := by simpa [hn] using hicap_j
          have hiocc_sj : IsOccupied s j := by
            change s.dist j ≠ EMPTY
            rw [← h.frame.dist_other j hj_ne]
            exact hiocc_j
          have hordered :=
            hci.2.1 next j h.next_lt_capacity hicap_sj hnext_lt_j hnext_occ hiocc_sj
          unfold BucketAt at hordered ⊢
          rw [h.frame.dist_other j hj_ne]
          exact hordered
      · by_cases hj_pos : j = position
        · subst j
          rw [hbucket_position]
          have hicap_si : i < capacity s.n := by simpa [hn] using hicap_i
          have hiocc_si : IsOccupied s i := by
            change s.dist i ≠ EMPTY
            rw [← h.frame.dist_other i hi_pos]
            exact hiocc_i
          have hi_next : i < next := lt_trans hij h.position_lt_next
          have hordered :=
            hci.2.1 i next hicap_si h.next_lt_capacity hi_next hiocc_si hnext_occ
          unfold BucketAt at hordered ⊢
          rw [h.frame.dist_other i hi_pos]
          exact hordered
        · have hicap_si : i < capacity s.n := by simpa [hn] using hicap_i
          have hicap_sj : j < capacity s.n := by simpa [hn] using hicap_j
          have hiocc_si : IsOccupied s i := by
            change s.dist i ≠ EMPTY
            rw [← h.frame.dist_other i hi_pos]
            exact hiocc_i
          have hiocc_sj : IsOccupied s j := by
            change s.dist j ≠ EMPTY
            rw [← h.frame.dist_other j hj_pos]
            exact hiocc_j
          unfold BucketAt
          rw [h.frame.dist_other i hi_pos, h.frame.dist_other j hj_pos]
          exact hci.2.1 i j hicap_si hicap_sj hij hiocc_si hiocc_sj
    · intro i hicap hiocc
      by_cases hi_pos : i = position
      · subst i
        obtain ⟨tailKey, htail_key⟩ := h.next_key_present
        have hcorrect_next := hci.2.2 next h.next_lt_capacity hnext_occ
        have htarget_expected : ExpectedBucket s' position = bucket tailKey s.n := by
          simp [ExpectedBucket, h.keyAt_position, htail_key, hremap', hn]
        have hsource_expected : ExpectedBucket s next = bucket tailKey s.n := by
          simp [ExpectedBucket, htail_key, hremap]
        rw [hbucket_position, htarget_expected, ← hsource_expected]
        exact hcorrect_next
      · have hicap_s : i < capacity s.n := by simpa [hn] using hicap
        have hiocc_s : IsOccupied s i := by
          change s.dist i ≠ EMPTY
          rw [← h.frame.dist_other i hi_pos]
          exact hiocc
        have hcorrect := hci.2.2 i hicap_s hiocc_s
        have hexpected : ExpectedBucket s' i = ExpectedBucket s i := by
          simp [ExpectedBucket, h.frame.keyAt_other i hi_pos, hremap', hremap, hn]
        rw [hexpected]
        unfold BucketAt
        rw [h.frame.dist_other i hi_pos]
        exact hcorrect

-- Faithful settled-state result for the complete remove-gap loop. The settled premise is
-- necessary for the current `ExpectedBucket`: the active-boundary counterexample below
-- shows that moving a tail across `remapEnd` can otherwise change its expected table size.
-- src/map.rs L504-L520.
theorem removeRelocate_preserves_invariant {s s' : State} {position : Nat}
    (hrel : RemoveRelocate s s' position) (hci : ClusterInvariant s)
    (hremap : s.remapEnd = none) : ClusterInvariant s' := by
  induction hrel with
  | stop hstop =>
      exact clearCurrentHole_preserves_clusterInvariant hstop.clear hci
  | step hcontinue _rest ih =>
      have hci_mid : ClusterInvariant _ :=
        removeContinue_preserves_clusterInvariant hcontinue hci hremap
      have hremap_mid : _ := hcontinue.frame.header.remapEnd.trans hremap
      exact ih hci_mid hremap_mid

-- Settled found-branch refinement for public `remove`. The certificate records the exact
-- lookup-selected position, the faithful remove-gap chain, and the caller's final length
-- decrement. The proof consumes that certificate; it does not prove that the preceding
-- `remap_step` or concrete Rust lookup constructs it. src/map.rs L491-L520.
theorem publicRemoveSettled_preserves_invariant {s s' : State} {key : Key}
    (h : PublicRemoveSettled s s' key) :
    RemovePreservesInvariant s s' ∧ 0 < s.len ∧ s'.len = s.len - 1 ∧
      s'.n = s.n ∧ s'.remapEnd = none := by
  cases h with
  | found hlookup hsettled hrel hsetLen =>
      have hheader := hrel.sameHeader
      have hlen : 0 < s.len := by
        apply Nat.pos_of_ne_zero
        intro hzero
        simp [lookupIndex, hzero] at hlookup
      rw [hsetLen]
      refine ⟨?_, hlen, ?_, ?_, ?_⟩
      · intro hci
        have hci_after := removeRelocate_preserves_invariant hrel hci hsettled
        simpa [ClusterInvariant, DistanceValid, ClusterOrdered, EntryAtCorrectBucket,
          IsOccupied, BucketAt, ExpectedBucket] using hci_after
      · exact congrArg (fun len => len - 1) hheader.len
      · exact hheader.n
      · exact hheader.remapEnd.trans hsettled

lemma remove_preserves_invariant (h : ClusterInvariant s) (hstep : UnRelocateStep s s' position) :
    ClusterInvariant s' := by
  -- `UnRelocateStep` lacks the metadata and relocation-chain facts needed to prove removal preserves the invariant (src/map.rs L504-L520).
  -- For every supplied key, Counterexamples.lean proves a machine-checked violating
  -- witness; if `Key` is empty, `UnRelocateStep` is uninhabited and that witness is
  -- unavailable, so the counterexample requires an inhabited key domain.
  sorry

-- The concrete remove loop can move a tail from above `remapEnd` to a position at or
-- below it; the current abstract `ExpectedBucket` then changes table size solely because
-- the slot index changed. The theorem above proves the settled `remapEnd = none` case;
-- this machine-checked counterexample shows why that premise cannot simply be dropped
-- without a stronger active-remap invariant. src/map.rs L421-L520.
theorem removeRelocate_activeBoundary_counterexample (k : Key)
    (hold : bucket k 1 = 0) (hnew : bucket k 2 = 2) :
    ∃ s s' position,
      ClusterInvariant s ∧ RemoveRelocate s s' position ∧ ¬ ClusterInvariant s' := by
  let s : State :=
    { n := 2, len := 2, remapEnd := some 2
      dist := fun i => if i = 2 then 2 else if i = 3 then 1 else EMPTY
      keyAt := fun i => if i = 2 ∨ i = 3 then some k else none
      valAt := fun _ => 0 }
  let mid : State :=
    { n := 2, len := 2, remapEnd := some 2
      dist := fun i => if i = 2 then 0 else if i = 3 then 1 else EMPTY
      keyAt := fun i => if i = 2 ∨ i = 3 then some k else none
      valAt := fun _ => 0 }
  let s' : State :=
    { n := 2, len := 2, remapEnd := some 2
      dist := fun i => if i = 2 then 0 else EMPTY
      keyAt := fun i => if i = 2 ∨ i = 3 then some k else none
      valAt := fun _ => 0 }
  refine ⟨s, s', 2, ?_, ?_, ?_⟩
  · constructor
    · intro i hi hocc
      dsimp [s] at hi hocc ⊢
      norm_num [capacity] at hi
      split_ifs at hocc ⊢ <;> omega
    · constructor
      · intro i j hi hj hij hiocc hjocc
        dsimp [s] at hi hj hiocc hjocc ⊢
        norm_num [capacity] at hi hj
        simp only [BucketAt] at hiocc hjocc ⊢
        split_ifs at hiocc hjocc ⊢ <;> omega
      · intro i hi hocc
        dsimp [s] at hi hocc ⊢
        norm_num [capacity] at hi
        by_cases hi2 : i = 2
        · subst i
          simp [ExpectedBucket, hold]
        · by_cases hi3 : i = 3
          · subst i
            norm_num [BucketAt, ExpectedBucket, hnew]
          · simp [hi2, hi3] at hocc
  · apply RemoveRelocate.step (mid := mid) (next := 3)
    · refine
        { frame := ?_, position_lt_last := ?_, nextDist_not_empty := ?_,
          nextDist_not_home := ?_, next_is_tail := ?_, position_lt_next := ?_,
          next_lt_capacity := ?_, next_key_present := ?_, keyAt_position := ?_,
          valAt_position := ?_, shift_le_tailDist := ?_, distAt_position := ?_ }
      · refine
          { header := ?_, keyAt_other := ?_, valAt_other := ?_, dist_other := ?_ }
        · exact ⟨rfl, rfl, rfl⟩
        · intro i _hi
          rfl
        · intro i _hi
          rfl
        · intro i hi
          simp [s, mid, hi]
      · norm_num [s, capacity]
      · norm_num [s, EMPTY]
      · norm_num [s]
      · change 3 = endOfClusterFrom s (BucketAt s 3) 3 - 1
        have hbucket3 : BucketAt s 3 = 2 := by norm_num [s, BucketAt]
        rw [hbucket3, endOfClusterFrom]
        have hcap3 : 3 < capacity s.n := by norm_num [s, capacity]
        simp only [hcap3, hbucket3, if_pos]
        rw [endOfClusterFrom]
        norm_num [s, capacity, IsOccupied, EMPTY]
      · omega
      · norm_num [s, capacity]
      · exact ⟨k, by simp [s]⟩
      · simp [s, mid]
      · rfl
      · norm_num [s]
      · norm_num [s, mid]
    · apply RemoveRelocate.stop
      let hframe : RemoveFrame mid s' 3 := by
        refine ⟨⟨rfl, rfl, rfl⟩, ?_, ?_, ?_⟩
        · intro i hi
          simp [mid, s', hi]
        · intro i _hi
          rfl
        · intro i hi
          simp [mid, s', hi]
      let hclear : ClearCurrentHole mid s' 3 := by
        refine ⟨hframe, ?_, ?_, ?_, ?_⟩
        · norm_num [mid, capacity]
        · simp [mid, s']
        · rfl
        · simp [s']
      refine ⟨hclear, Or.inr ⟨?_, Or.inl ?_⟩⟩
      · norm_num [mid, capacity]
      · simp [mid]
  · intro hinv
    have hcorrect := hinv.2.2 2 (by norm_num [s', capacity]) (by
      norm_num [s', IsOccupied, EMPTY])
    norm_num [s', BucketAt, ExpectedBucket, hold] at hcorrect

lemma remap_step_preserves_invariant (h : ClusterInvariant s) (hstep : RemapStep s s') :
    ClusterInvariant s' := by
  -- `RemapStep` postulates only `keySet` and `len`, which underconstrains invariant preservation (src/map.rs L559-L597).
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
