/-
Stage 3 abstract state for the stable linear hash map.

The semantic model flattens each bucket block to `(bucket, slotIndex) → Option (K×V)`
with `slotIndex ∈ [0, SLOTS_PER_BUCKET)`; the physical mapping is
`index = page * PRIMARY_SLOTS + slot`, exactly the enumeration used by
`entries_from_image` (map.rs L1787-L1806) and `scan_physical_window`
(map.rs L1088-L1119). Occupancy bitmaps and stored bytes are assumed consistent
(SCOPE.md assumption A5); the byte layer itself remains out of scope here.

Hash functions are opaque state fields (SCOPE.md A1): every property holds for
arbitrary hashes.
-/
import Lhm.Routing
import Lhm.Abs.Base

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Abstract map state -/

/-- Semantic mirror of the persisted map. `buckets b j` is the entry stored in bucket
`b`, physical slot index `j` (`none` = unoccupied). Fields mirror the control record
plus the two opaque routing hashes. -/
structure MapState (K V : Type) where
  hash1 : K → Nat
  hash2 : K → Nat
  buckets : Nat → Nat → Option (K × V)
  physicalBuckets : Nat
  len : Nat
  overflowEntries : Nat
  splitDebt : Nat
  mutationEpoch : Nat
  incarnation : Nat
  level : Nat
  splitCursor : Nat

/-- First routing candidate (map.rs `candidate_buckets_from_bytes` first component). -/
def cand1 (st : MapState K V) (k : K) : Nat :=
  linearBucket (st.hash1 k) st.level st.splitCursor

/-- Second routing candidate (second component). -/
def cand2 (st : MapState K V) (k : K) : Nat :=
  linearBucket (st.hash2 k) st.level st.splitCursor

/-- Membership in the two-choice candidate pair. -/
def InCands (st : MapState K V) (k : K) (b : Nat) : Prop :=
  b = cand1 st k ∨ b = cand2 st k

/-! ## Occupancy accounting -/

/-- Slot-occupancy predicate of bucket `b` over flattened indices. -/
def occPred (st : MapState K V) (b : Nat) : Nat → Bool :=
  fun j => (st.buckets b j).isSome

/-- Number of occupied slots in one bucket (`bucket_load`, map.rs L1437-L1442). -/
def loadOf (st : MapState K V) (b : Nat) : Nat :=
  countMatch (occPred st b) SlotsPerBucket

/-- Overflow-page occupancy predicate: indices on pages `> 0`. -/
def ovfPred (st : MapState K V) (b : Nat) : Nat → Bool :=
  fun j => (PrimarySlots ≤ j && (st.buckets b j).isSome)

/-- Inline-overflow occupancy of one bucket. -/
def ovfLoadOf (st : MapState K V) (b : Nat) : Nat :=
  countMatch (ovfPred st b) SlotsPerBucket

/-- Total occupied slots over buckets `[0, n)` — what persistent `len` counts. -/
def totalLenOf (st : MapState K V) (n : Nat) : Nat :=
  totalLoads (fun b => loadOf st b) n

/-- Total inline-overflow slots over `[0, n)` — what `overflow_entries` counts. -/
def totalOvfOf (st : MapState K V) (n : Nat) : Nat :=
  totalLoads (fun b => ovfLoadOf st b) n

theorem loadOf_le (st : MapState K V) (b : Nat) : loadOf st b ≤ SlotsPerBucket :=
  countMatch_le _ _

/-! ## The stage-3 invariant bundle -/

/-- Everything stage 3 assumes and preserves: geometry validity (as in Stage A's
`ValidControl`), counter/accounting equalities, the placement invariant (every entry
lives inside its own candidate pair, at a real slot index), and global key
uniqueness within the scanned region. -/
structure Inv (st : MapState K V) : Prop where
  geomLevelLow : InitialLevel ≤ st.level
  geomLevelHigh : st.level < 63
  geomCursorBound : st.splitCursor < 2 ^ st.level
  geomBucketsEq : st.physicalBuckets = 2 ^ st.level + st.splitCursor
  geomEpochEven : st.mutationEpoch % 2 = 0
  geomIncarnationPos : st.incarnation ≠ 0
  countersLen : st.len = totalLenOf st st.physicalBuckets
  countersOvf : st.overflowEntries = totalOvfOf st st.physicalBuckets
  placed : ∀ b j e, st.buckets b j = some e →
    b < st.physicalBuckets ∧ j < SlotsPerBucket ∧ InCands st e.1 b
  unique : ∀ b1 j1 e1 b2 j2 e2, j1 < SlotsPerBucket → j2 < SlotsPerBucket →
    st.buckets b1 j1 = some e1 → st.buckets b2 j2 = some e2 →
    e1.1 = e2.1 → b1 = b2 ∧ j1 = j2

/-- Both routing candidates stay strictly inside the allocated extent (P1 composed
with the geometry conjuncts). -/
theorem cand_lt_pb {st : MapState K V} (inv : Inv st) (k : K) :
    cand1 st k < st.physicalBuckets ∧ cand2 st k < st.physicalBuckets := by
  unfold cand1 cand2
  obtain ⟨_, _, _, hpb, _, _, _, _, _, _⟩ := inv
  have h := route_lt_base_plus_cursor (st.hash1 k) st.level st.splitCursor
  have h' := route_lt_base_plus_cursor (st.hash2 k) st.level st.splitCursor
  omega

end Lhm.Abs
