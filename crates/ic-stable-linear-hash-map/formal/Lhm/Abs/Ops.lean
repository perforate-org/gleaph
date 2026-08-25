/-
Stage 3 operation semantics: final logical effects of insert, remove, clear, and
reset. Each definition mirrors its Rust counterpart; the intermediate odd mutation
epoch (guard open/close) is stage-5 material, so successful mutations advance the
epoch by exactly 2 here.

Abstraction note (SCOPE.md A5 extension): the model stores `Option (K×V)` directly,
assuming occupancy words always agree with stored bytes. The free-slot *choice* is
generalized to "any free slot of the chosen candidate block"; the concrete first-free
policy only affects physical scan order, which stage 4 addresses.
-/
import Lhm.Abs.Search

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Update primitives -/

/-- Point update of a flattened content function (generic over the entry type). -/
def setBucketEntry {α : Type} (g : Nat → Nat → Option α) (b j : Nat)
    (e : Option α) : Nat → Nat → Option α :=
  fun b' j' => if b' = b ∧ j' = j then e else g b' j'

theorem setBucketEntry_left_ne {α : Type} {g : Nat → Nat → Option α} {b j : Nat}
    {e : Option α} {x y : Nat} (hx : x ≠ b) :
    setBucketEntry g b j e x y = g x y := by
  simp [setBucketEntry, hx]

theorem setBucketEntry_right_ne {α : Type} {g : Nat → Nat → Option α} {b j : Nat}
    {e : Option α} {x y : Nat} (hy : y ≠ j) :
    setBucketEntry g b j e x y = g x y := by
  simp [setBucketEntry, hy]

theorem setBucketEntry_self {α : Type} {g : Nat → Nat → Option α} {b j : Nat}
    {e : Option α} : setBucketEntry g b j e b j = e := by
  simp [setBucketEntry]

/-- Overwrite the value stored at an existing location (insert-update path,
map.rs L907-L914). -/
def setValue (st : MapState K V) (b j : Nat) (k : K) (v : V) : MapState K V :=
  { st with
    buckets := setBucketEntry st.buckets b j (some (k, v))
    mutationEpoch := st.mutationEpoch + 2 }

/-- Place a fresh entry into a free slot (insert-place path, map.rs L931-L951). -/
def placeAt (st : MapState K V) (k : K) (v : V) (b j : Nat) : MapState K V :=
  { st with
    buckets := setBucketEntry st.buckets b j (some (k, v))
    len := st.len + 1
    overflowEntries := st.overflowEntries + (if PrimarySlots ≤ j then 1 else 0)
    splitDebt := if PrimarySlots ≤ j ∨ st.len + 1 ≥ splitThreshold st.physicalBuckets
                 then max st.splitDebt 1 else st.splitDebt
    mutationEpoch := st.mutationEpoch + 2 }

/-- Remove the entry at a location (`remove`, map.rs L967-L1005, debt rule L996-L998). -/
def clearSlot (st : MapState K V) (b j : Nat) : MapState K V :=
  { st with
    buckets := setBucketEntry st.buckets b j none
    len := st.len - 1
    overflowEntries := st.overflowEntries - (if PrimarySlots ≤ j then 1 else 0)
    splitDebt := if st.overflowEntries - (if PrimarySlots ≤ j then 1 else 0) = 0
                 ∧ st.len - 1 < splitThreshold st.physicalBuckets then 0 else st.splitDebt
    mutationEpoch := st.mutationEpoch + 2 }

/-! ## Clear / reset -/

/-- Final logical effect of `clear` (map.rs L751-L801): initial geometry, zeroed
counters, incarnation preserved. Buckets beyond the initial extent keep stale bytes
that no published control can reach (see REPORT.md finding 4). -/
def clearedState (st : MapState K V) : MapState K V :=
  { st with
    buckets := fun b j => if b < 2 ^ InitialLevel then none else st.buckets b j
    physicalBuckets := 2 ^ InitialLevel
    len := 0
    overflowEntries := 0
    splitDebt := 0
    mutationEpoch := st.mutationEpoch + 2
    level := InitialLevel
    splitCursor := 0 }

/-- Final logical effect of `reset` (map.rs L683-L741): clear plus incarnation bump. -/
def resetState (st : MapState K V) : MapState K V :=
  { clearedState st with incarnation := st.incarnation + 1 }

/-! ## Free-slot choice -/

/-- First unoccupied slot of a bucket. -/
def firstFreeIdx (st : MapState K V) (b : Nat) : Option Nat :=
  firstMatch (fun j => !(occPred st b j)) SlotsPerBucket

/-- Choose which candidate block absorbs a fresh entry (map.rs L916-L930): prefer the
less-loaded block, ties to the first. -/
def chooseFreeSlot (st : MapState K V) (c1 c2 : Nat) : Option (Nat × Nat) :=
  match firstFreeIdx st c1, (if c1 = c2 then none else firstFreeIdx st c2) with
  | some i1, some i2 =>
      if loadOf st c1 ≤ loadOf st c2 then some (c1, i1) else some (c2, i2)
  | some i1, none => some (c1, i1)
  | none, some i2 => some (c2, i2)
  | none, none => none

/-! ## Insert -/

inductive InsertOutcome (V : Type) where
| updated (oldValue : V)
| placed
| splitRequired

/-- Semantic insert: search both candidates; overwrite on hit; otherwise place into a
free slot of a candidate block; otherwise report that a split is required (the
split transformation itself is stage 4; `TablePressure` admission semantics map to
`splitRequired`). -/
def opInsert [DecidableEq K] (st : MapState K V) (k : K) (v : V) :
    InsertOutcome V × MapState K V :=
  match findIn st (cand1 st k) k,
        (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k) with
  | some (j, (_, vo)), _ => (InsertOutcome.updated vo, setValue st (cand1 st k) j k v)
  | none, some (j, (_, vo)) => (InsertOutcome.updated vo, setValue st (cand2 st k) j k v)
  | none, none =>
      match chooseFreeSlot st (cand1 st k) (cand2 st k) with
      | some (b, j) => (InsertOutcome.placed, placeAt st k v b j)
      | none => (InsertOutcome.splitRequired, st)

/-- Semantic remove: delete on hit, otherwise leave the logical state unchanged
(`remove`, map.rs L967-L1005). -/
def opRemove [DecidableEq K] (st : MapState K V) (k : K) : Option V × MapState K V :=
  match findIn st (cand1 st k) k,
        (if cand1 st k = cand2 st k then none else findIn st (cand2 st k) k) with
  | some (j, e), _ => (some e.2, clearSlot st (cand1 st k) j)
  | none, some (j, e) => (some e.2, clearSlot st (cand2 st k) j)
  | none, none => (none, st)

end Lhm.Abs
