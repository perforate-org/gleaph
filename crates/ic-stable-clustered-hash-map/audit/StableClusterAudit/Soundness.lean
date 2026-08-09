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
-/

lemma insert_preserves_invariant (h : ClusterInvariant s) (hstep : RelocateStep s s' entry value position) :
    ClusterInvariant s' := by
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
