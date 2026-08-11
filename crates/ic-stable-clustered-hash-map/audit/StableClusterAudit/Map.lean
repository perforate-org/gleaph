/-
Stage 2 — Faithful transcription of the `StableClusteredHashMap` operations.

Audit mode (SCOPE.md §1). Transcribes `src/map.rs` into Lean: cluster scanning,
`lookup_index`, and the insert / remove / incremental-resize transitions. Abstract
memory (get/set) per SCOPE §6; byte-level layout, allocation, and error paths are out
of scope. Every definition cites the Rust line range. Invariant-preservation obligations
are handled in Stage 3, where residual obligations are marked with `sorry` and comments.
-/

import Mathlib
import StableClusterAudit.Abstract

open StableCluster

namespace StableCluster

/-!
## Cluster-scanning helpers (src/map.rs L289-L319)

These are pure scans; termination is by the remaining slots below `capacity`.
-/

-- `end_of_cluster_by_position`: the end (tail + 1) of the cluster of bucket `b`,
-- scanning from `i`. Stops at an empty slot or a slot not in bucket `b`.
-- src/map.rs L289-L300.
def endOfClusterFrom (s : State) (b : Nat) (i : Nat) : Nat :=
  if i < capacity s.n then
    if IsOccupied s i ∧ BucketAt s i = b then
      endOfClusterFrom s b (i + 1)
    else i
  else i
termination_by capacity s.n - i
decreasing_by
  -- From `i < capacity s.n`, `capacity s.n - (i+1) < capacity s.n - i`.
  omega

-- `end_of_cluster_by_position`: the end of the cluster containing `position`.
-- src/map.rs L289-L300.
def endOfCluster (s : State) (position : Nat) : Nat :=
  endOfClusterFrom s (BucketAt s position) position

-- `tail_of_cluster_by_position`: the last slot of the cluster containing `position`.
-- src/map.rs L302-L305.
def tailOfCluster (s : State) (position : Nat) : Nat :=
  endOfCluster s position - 1

-- `find_insert_position`: where a new entry with bucket `b` should be inserted: the end
-- of bucket `b`'s cluster, or the end of the previous cluster if `b`'s cluster does not
-- start at `b`. Scans from `i`.
-- src/map.rs L309-L319.
def findInsertPositionFrom (s : State) (b : Nat) (i : Nat) : Nat :=
  if i < capacity s.n then
    if IsOccupied s i ∧ BucketAt s i ≤ b then
      findInsertPositionFrom s b (i + 1)
    else i
  else i
termination_by capacity s.n - i
decreasing_by
  omega

def findInsertPosition (s : State) (b : Nat) : Nat :=
  findInsertPositionFrom s b b

/-!
## Probing: `lookup_index` (src/map.rs L325-L372)

Returns the slot holding `key` if present. Modeled as a function (the Rust method also
returns the insert position and bucket, which only matter for insert; the found slot is
what the invariants need). Scans the new table from `b`, then the old table in the mixed
range `[0, remapEnd]` during an in-progress resize.
-/

-- Scan the table for `key` starting at slot `i`, within the run for bucket `b`.
-- Faithful to the main scan in `lookup_index` (src/map.rs L334-L348): stop at an empty
-- slot, a slot whose bucket exceeds `b`, or a match at bucket `b`.
noncomputable def scanFor (s : State) (key : Key) (b : Nat) (i : Nat) : Option Nat :=
  if i < capacity s.n then
    if s.dist i = EMPTY then none
    else if BucketAt s i > b then none
    else if BucketAt s i = b ∧ s.keyAt i = some key then some i
    else scanFor s key b (i + 1)
  else none
termination_by capacity s.n - i
decreasing_by
  omega

-- The mixed-range scan: after a resize, an entry still in `[0, remapEnd]` uses the OLD
-- bucket `n-1`. Faithful to src/map.rs L349-L370.
noncomputable def scanMixedRange (s : State) (key : Key) (prevBucket : Nat) (remapEnd : Nat) (j : Nat) :
    Option Nat :=
  if j ≤ remapEnd then
    if s.dist j = EMPTY then none
    else if BucketAt s j > prevBucket then none
    else if BucketAt s j = prevBucket ∧ s.keyAt j = some key then some j
    else scanMixedRange s key prevBucket remapEnd (j + 1)
  else none
termination_by remapEnd + 1 - j
decreasing_by
  omega

-- `lookup_index`'s found-result: the slot holding `key`, or `none`.
-- src/map.rs L325-L372.
noncomputable def lookupIndex (s : State) (key : Key) : Option Nat :=
  let b := bucket key s.n
  if s.len = 0 then none
  else
    match scanFor s key b b with
    | some i => some i
    | none =>
        match s.remapEnd with
        | some e => if s.n > 0 ∧ bucket key (s.n - 1) ≤ e
            then scanMixedRange s key (bucket key (s.n - 1)) e (bucket key (s.n - 1))
            else none
        | none => none

/-!
## Mutating transitions (faithful to the per-step logic)

The mutators run loops over the abstract memory. They are transcribed as transition
relations on the `State`, faithful to the assignments in the Rust code; the loop is
captured as a step predicate. These are the basis for the Stage 3 invariant-preservation
proofs (targets (a) and (b)).
-/

-- `insert_and_relocate`: writes `entry` at `position`, displacing the head of the next
-- cluster and pushing it down by the cluster length until an empty slot is reached.
-- src/map.rs L447-L476.
--
-- One relocation step (an intermediate state of the loop): at an occupied `position`, the
-- occupant `t` is read, its distance would grow by `(next - position)`, the new `entry` is
-- written at `position`, and the loop continues from `next` with `entry := t`. In this
-- single step only `position` changes; the displaced `t` is NOT yet written anywhere (it is
-- the pending entry). Hence a single step does not preserve the entry set or
-- `ClusterInvariant`; only the whole chain (steps followed by a terminating
-- `RelocateWrite`) does.
-- src/map.rs L468-L474.
structure RelocateStep (s s' : State) (entry : Key) (value : Nat) (entryDist : Nat)
    (position : Nat) where
  n : s.n = s'.n
  -- `insert_and_relocate` (without a resize) never touches the resize boundary.
  remapEnd : s'.remapEnd = s.remapEnd
  tKey : Key
  tVal : Nat
  tDist : Nat
  next : Nat
  occT : s.keyAt position = some tKey
  valT : s.valAt position = tVal
  distT : s.dist position = tDist
  next_is_end : next = endOfCluster s position
  -- the displaced occupant's distance grows by the shift; it becomes the next pending entry
  tDistShifted : tDist + (next - position) < EMPTY
  -- the new entry lands at `position` with its own distance
  entryAt : s'.keyAt position = some entry
  entryVal : s'.valAt position = value
  entryDistAt : s'.dist position = entryDist
  entryDist_le : entryDist ≤ position
  keyAt_other : ∀ i, i ≠ position → s'.keyAt i = s.keyAt i
  valAt_other : ∀ i, i ≠ position → s'.valAt i = s.valAt i
  dist_other : ∀ i, i ≠ position → s'.dist i = s.dist i

-- `insert_and_relocate` base case: an empty slot at `position` is written directly, with
-- the entry's distance = `position - bucket(entry, n)` so it sits at its home bucket.
-- `find_insert_position` guarantees `position` is the end of the bucket's cluster, which is
-- what keeps the table ordered (target (b)). src/map.rs L464-L466.
structure RelocateWrite (s s' : State) (entry : Key) (value : Nat) (position : Nat) : Prop where
  n : s.n = s'.n
  slotEmpty : s.dist position = EMPTY
  distFit : position - bucket entry s.n < EMPTY
  b_le_pos : bucket entry s.n ≤ position
  keyAt : s'.keyAt position = some entry
  valAt : s'.valAt position = value
  dist : s'.dist position = position - bucket entry s.n
  keyAt_other : ∀ i, i ≠ position → s'.keyAt i = s.keyAt i
  valAt_other : ∀ i, i ≠ position → s'.valAt i = s.valAt i
  dist_other : ∀ i, i ≠ position → s'.dist i = s.dist i

-- `position` is a valid insertion point for a new entry of bucket `b`: the slot is empty
-- and every occupied slot below / above it has a bucket ≤ / ≥ `b`, so inserting at
-- `position` keeps the table ordered. This is what `find_insert_position` guarantees.
-- src/map.rs L309-L319.
def IsInsertionPoint (s : State) (position b : Nat) : Prop :=
  s.dist position = EMPTY ∧
  (∀ i, i < position → IsOccupied s i → BucketAt s i ≤ b) ∧
  (∀ i, i > position → i < capacity s.n → IsOccupied s i → b ≤ BucketAt s i)

-- `position` is an order boundary for bucket `b`: every occupied slot below it has bucket
-- ≤ `b` and every occupied slot above it has bucket ≥ `b`. Writing an entry of bucket `b`
-- at `position` keeps the table ordered. This is the property `find_insert_position` gives
-- for the relocation step, where the displaced cluster lies at / above `position`.
-- src/map.rs L309-L319 (find_insert_position), L468-L474 (relocation step).
def IsOrderBoundary (s : State) (position b : Nat) : Prop :=
  (∀ i, i < position → IsOccupied s i → BucketAt s i ≤ b) ∧
  (∀ i, i > position → i < capacity s.n → IsOccupied s i → b ≤ BucketAt s i)

-- The relocation chain of `insert_and_relocate` (src/map.rs L447-L476): zero or more
-- `RelocateStep`s (each displacing the next entry) followed by a terminating
-- `RelocateWrite`. `key`/`value` is the initial pending entry at `position`; the displaced
-- occupant becomes the pending entry of the next step.
inductive InsertRelocate : State → State → Key → Nat → Nat → Prop where
  | done {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (h : RelocateWrite s s' key value position) : InsertRelocate s s' key value position
  | step {s s' : State} {key : Key} {value : Nat} {position : Nat} (mid : State) (entryDist : Nat)
      (hstep : RelocateStep s mid key value entryDist position)
      (hnext : InsertRelocate mid s' hstep.tKey hstep.tVal hstep.next) :
      InsertRelocate s s' key value position

-- Faithful no-resize execution trace for `insert_and_relocate`. The normal insert computes its
-- initial pending distance at src/map.rs L445-L451, and `checked_distance` enforces the strict
-- `EMPTY` bound at L88-L98. Every loop frame starts inside the table, explicitly excluding the
-- `position >= capacity` / `size_up` branch at L467-L476.
inductive InsertRelocateTrace : State → State → Key → Nat → Nat → Nat → Prop where
  -- Empty-slot terminal write, src/map.rs L477-L480. The equality keeps the pending-distance
  -- index faithful to the distance written by `RelocateWrite`.
  | done {s s' : State} {key : Key} {value pendingDist position : Nat}
      (hw : RelocateWrite s s' key value position)
      (hpos : position < capacity s.n)
      (hentry : pendingDist < EMPTY)
      (hdist : pendingDist = position - bucket key s.n) :
      InsertRelocateTrace s s' key value pendingDist position
  -- Occupied-slot relocation, src/map.rs L481-L487. Passing `pendingDist` directly to
  -- `RelocateStep` ties the current in-memory entry's distance to the modeled write; the strict
  -- progress and capacity bounds record that the next loop frame remains in this no-resize trace.
  | step {s s' : State} {key : Key} {value pendingDist position : Nat}
      (mid : State)
      (hstep : RelocateStep s mid key value pendingDist position)
      (hpos : position < capacity s.n)
      (hentry : pendingDist < EMPTY)
      (nextDist : Nat)
      (hnextDist : nextDist = hstep.tDist + (hstep.next - position))
      (hprogress : position < hstep.next)
      (hnextPos : hstep.next < capacity mid.n)
      (hnext : InsertRelocateTrace mid s' hstep.tKey hstep.tVal nextDist hstep.next) :
      InsertRelocateTrace s s' key value pendingDist position

-- Well-formedness carried through the insert chain: at each step, `position` is an order
-- boundary for the pending entry's bucket, the pending entry is at its home bucket and
-- precedes the displaced entry. This is what `find_insert_position` and the loop guarantee.
inductive InsertRelocateOK : {s s' : State} → {key : Key} → {value : Nat} → {position : Nat} →
    InsertRelocate s s' key value position → Prop where
  | done {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (hw : RelocateWrite s s' key value position)
      (hslot : s.dist position = EMPTY) (hbound : IsOrderBoundary s position (bucket key s.n)) :
      InsertRelocateOK (InsertRelocate.done hw)
  | step {s s' : State} {key : Key} {value : Nat} {position : Nat} (mid : State) (entryDist : Nat)
      (hstep : RelocateStep s mid key value entryDist position)
      (hnext : InsertRelocate mid s' hstep.tKey hstep.tVal hstep.next)
      (hbound : IsOrderBoundary s position (bucket key s.n))
      (hbucket : position - entryDist = bucket key s.n)
      (hprec : position - entryDist ≤ position - hstep.tDist)
      (hok : InsertRelocateOK hnext) :
      InsertRelocateOK (InsertRelocate.step mid entryDist hstep hnext)

-- Length coherence is a cardinality fact about the slot writes, separate from the bucket-order
-- obligations in `InsertRelocateOK`. The production insert writes one previously empty slot at
-- the terminating `write_entry`; relocation steps overwrite occupied slots. These explicit bounds
-- record the facts needed to derive that cardinality change from the chain. src/map.rs L447-L487.
inductive InsertRelocateOccupancyOK : {s s' : State} → {key : Key} → {value : Nat} →
    {position : Nat} → InsertRelocate s s' key value position → Prop where
  | done {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (hw : RelocateWrite s s' key value position)
      (hpos : position < capacity s.n) :
      InsertRelocateOccupancyOK (InsertRelocate.done hw)
  | step {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (mid : State) (entryDist : Nat)
      (hstep : RelocateStep s mid key value entryDist position)
      (hnext : InsertRelocate mid s' hstep.tKey hstep.tVal hstep.next)
      (hpos : position < capacity s.n)
      (hentry : entryDist < EMPTY)
      (hok : InsertRelocateOccupancyOK hnext) :
      InsertRelocateOccupancyOK (InsertRelocate.step mid entryDist hstep hnext)

-- No-holes preservation needs one scan-contiguity fact for each pending entry in the chain:
-- every slot from its home bucket to the current write position is occupied. This is separate
-- from both the order certificate and the occupancy certificate because neither relation records
-- the scan's stop-at-empty guard. src/map.rs L309-L319, L447-L487.
inductive InsertRelocateNoHolesOK : {s s' : State} → {key : Key} → {value : Nat} →
    {position : Nat} → InsertRelocate s s' key value position → Prop where
  | done {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (hw : RelocateWrite s s' key value position)
      (hpos : position < capacity s.n)
      (hprefix : ∀ j, bucket key s.n ≤ j → j < position → IsOccupied s j) :
      InsertRelocateNoHolesOK (InsertRelocate.done hw)
  | step {s s' : State} {key : Key} {value : Nat} {position : Nat}
      (mid : State) (entryDist : Nat)
      (hstep : RelocateStep s mid key value entryDist position)
      (hnext : InsertRelocate mid s' hstep.tKey hstep.tVal hstep.next)
      (hpos : position < capacity s.n)
      (hprefix : ∀ j, bucket key s.n ≤ j → j < position → IsOccupied s j)
      (hentry : entryDist < EMPTY)
      (hok : InsertRelocateNoHolesOK hnext) :
      InsertRelocateNoHolesOK (InsertRelocate.step mid entryDist hstep hnext)

-- `remove_and_relocate`: empties `position` and shifts the tail of the next cluster up,
-- subtracting the shift from its distance. src/map.rs L504-L520.
-- One step: the slot `position` is freed and the tail `next` of the cluster at
-- `position + 1` is moved into it.
def UnRelocateStep (s s' : State) (position : Nat) : Prop :=
  ∃ tailKey tailVal tailDist next,
    next = tailOfCluster s (position + 1) ∧
    s.keyAt next = some tailKey ∧ s.valAt next = tailVal ∧ s.dist next = tailDist ∧
    tailDist ≥ (next - position) ∧
    s'.keyAt position = some tailKey ∧ s'.valAt position = tailVal ∧
    s'.dist position = tailDist - (next - position) ∧
    s'.dist next = EMPTY ∧
    (∀ i, i ≠ position → i ≠ next → s'.keyAt i = s.keyAt i) ∧
    (∀ i, i ≠ position → i ≠ next → s'.valAt i = s.valAt i) ∧
    (∀ i, i ≠ position → i ≠ next → s'.dist i = s.dist i)

-- The inner remove loop writes slots only. The public `remove` decrements `len` after
-- `remove_and_relocate` returns, so `n`, `len`, and the active-remap boundary are stable
-- throughout this relation. src/map.rs L497-L498, L504-L520.
structure SameRemoveHeader (s s' : State) : Prop where
  n : s'.n = s.n
  len : s'.len = s.len
  remapEnd : s'.remapEnd = s.remapEnd

-- A single write performed by the remove loop changes at most the current hole. In the
-- continue case this is important: `write_entry(position, tail)` does not clear the old
-- tail slot. That stale duplicate becomes the next loop hole and is cleared or overwritten
-- only by the following iteration. src/map.rs L515-L520.
structure RemoveFrame (s s' : State) (position : Nat) : Prop where
  header : SameRemoveHeader s s'
  keyAt_other : ∀ i, i ≠ position → s'.keyAt i = s.keyAt i
  valAt_other : ∀ i, i ≠ position → s'.valAt i = s.valAt i
  dist_other : ∀ i, i ≠ position → s'.dist i = s.dist i

-- Continue branch of `remove_and_relocate`. The current position is not the last slot,
-- the following slot is occupied and displaced from its home bucket, and the tail of that
-- cluster is copied into the current hole with its distance reduced by the shift. The old
-- tail slot is deliberately untouched by this transition. src/map.rs L506-L510, L515-L520.
structure RemoveContinue (s s' : State) (position next : Nat) : Prop where
  frame : RemoveFrame s s' position
  position_lt_last : position < capacity s.n - 1
  nextDist_not_empty : s.dist (position + 1) ≠ EMPTY
  nextDist_not_home : s.dist (position + 1) ≠ 0
  next_is_tail : next = tailOfCluster s (position + 1)
  position_lt_next : position < next
  next_lt_capacity : next < capacity s.n
  next_key_present : ∃ tailKey, s.keyAt next = some tailKey
  keyAt_position : s'.keyAt position = s.keyAt next
  valAt_position : s'.valAt position = s.valAt next
  shift_le_tailDist : next - position ≤ s.dist next
  distAt_position : s'.dist position = s.dist next - (next - position)

-- `write_distance(position, EMPTY)` changes only the current distance cell; key/value
-- bytes at the now-empty slot remain stale but are semantically ignored. src/map.rs
-- L506-L513.
structure ClearCurrentHole (s s' : State) (position : Nat) : Prop where
  frame : RemoveFrame s s' position
  position_lt_capacity : position < capacity s.n
  keyAt_position : s'.keyAt position = s.keyAt position
  valAt_position : s'.valAt position = s.valAt position
  distAt_position : s'.dist position = EMPTY

-- Stop branch: clear the current hole when it is the last table slot, or when the next
-- slot is empty or is at its home bucket. The second disjunct carries the exact bound
-- needed for the implementation's `position + 1` read. src/map.rs L506-L513.
structure RemoveStop (s s' : State) (position : Nat) : Prop where
  clear : ClearCurrentHole s s' position
  guard : position = capacity s.n - 1 ∨
    (position < capacity s.n - 1 ∧
      (s.dist (position + 1) = EMPTY ∨ s.dist (position + 1) = 0))

-- The complete bounded `remove_and_relocate` loop: either stop and clear the current
-- hole, or copy the next cluster's tail into it and continue from the old tail slot.
-- src/map.rs L504-L520.
inductive RemoveRelocate : State → State → Nat → Prop where
  | stop {s s' : State} {position : Nat} (h : RemoveStop s s' position) :
      RemoveRelocate s s' position
  | step {s mid s' : State} {position next : Nat}
      (h : RemoveContinue s mid position next)
      (rest : RemoveRelocate mid s' next) : RemoveRelocate s s' position

-- Compiler-checked statement of the stale-tail behavior in the continue transition.
-- src/map.rs L515-L520.
lemma RemoveContinue.oldTailUnchanged {s s' : State} {position next : Nat}
    (h : RemoveContinue s s' position next) :
    s'.keyAt next = s.keyAt next ∧ s'.valAt next = s.valAt next ∧
      s'.dist next = s.dist next := by
  have hne : next ≠ position := Nat.ne_of_gt h.position_lt_next
  exact ⟨h.frame.keyAt_other next hne, h.frame.valAt_other next hne,
    h.frame.dist_other next hne⟩

-- Header metadata is stable across the entire inner remove chain. The caller's later
-- `set_len` is outside this relation. src/map.rs L497-L498, L504-L520.
lemma RemoveRelocate.sameHeader {s s' : State} {position : Nat}
    (h : RemoveRelocate s s' position) : SameRemoveHeader s s' := by
  induction h with
  | stop hstop => exact hstop.clear.frame.header
  | step hcontinue _rest ih =>
      exact
        { n := ih.n.trans hcontinue.frame.header.n
          len := ih.len.trans hcontinue.frame.header.len
          remapEnd := ih.remapEnd.trans hcontinue.frame.header.remapEnd }

-- Certificate for the found branch of public `remove` after its leading `remap_step`.
-- `s` is the state immediately before `lookup_index`; the settled field deliberately
-- excludes an active resize because the current invariant is not preserved by every
-- active-boundary remove chain. `lookup` records the concrete position selected by the
-- modeled lookup without claiming here that the Rust lookup constructs this equality.
-- After the faithful slot-relocation chain, the only remaining public-operation write is
-- `set_len(len - 1)`, represented as an exact record update. src/map.rs L491-L520.
inductive PublicRemoveSettled : State → State → Key → Prop where
  | found {s s' : State} {key : Key} {position : Nat} {afterRelocate : State}
      (lookup : lookupIndex s key = some position) (settled : s.remapEnd = none)
      (relocate : RemoveRelocate s afterRelocate position)
      (setLen : s' = { afterRelocate with len := afterRelocate.len - 1 }) :
      PublicRemoveSettled s s' key

-- Bounded helper retained for the intentionally weak one-step relation. It adds only
-- header stability and does not turn `UnRelocateStep` into the faithful chain above.
-- src/map.rs L504-L520.
structure UnRelocateStepWithStableHeader (s s' : State) (position : Nat) : Prop where
  header : SameRemoveHeader s s'
  step : UnRelocateStep s s' position

/-!
## Incremental resize (src/map.rs L523-L597)

`size_up` doubles the bucket table and starts a remap (sets `remapEnd` to the previous
capacity). `remap_position` relocates an entry whose bucket changed, and `remap_step`
advances the boundary. These are transcribed as relations; targets (a) (entry
preservation) and (c) (re-open) build on them.
-/

-- `size_up`: grows in place to `2^(n+1) + (n+1)`, keeping all entries, and starts the
-- incremental remap at `remapEnd = prev_capacity`.
-- src/map.rs L526-L554.
structure SizeUp (s s' : State) : Prop where
  n : s'.n = s.n + 1
  len : s'.len = s.len
  remapEnd : s'.remapEnd = some (capacity s.n)
  -- the key/value content is preserved (only geometry grows; remap later relocates)
  keyAt : ∀ i, s'.keyAt i = s.keyAt i
  valAt : ∀ i, s'.valAt i = s.valAt i
  -- old region keeps its distances; the newly grown region [capacity n, capacity (n+1))
  -- is cleared (EMPTY), matching `clear_region` in src/map.rs L542-L549.
  distOld : ∀ i, i < capacity s.n → s'.dist i = s.dist i
  distNew : ∀ i, capacity s.n ≤ i → i < capacity s'.n → s'.dist i = EMPTY

-- `remap_step`: processes positions from the bottom of the mixed range, relocating
-- entries whose bucket changed under the new size, until the boundary reaches 0.
-- src/map.rs L559-L597.
-- A single remap of `position`: if the entry is not yet at its new bucket, remove it and
-- reinsert at the new bucket; otherwise just shrink the boundary.
def RemapOne (s s' : State) (position : Nat) : Prop :=
  -- entry's current bucket (old) vs its bucket under the new size
  (∃ k, s.keyAt position = some k ∧
    BucketAt s position ≠ bucket k s.n ∧
    -- remove_and_relocate(position) then insert at find_insert_position(new bucket)
    s'.keyAt position = none ∧
    (∃ newPos, newPos = findInsertPosition s (bucket k s.n) ∧ s'.keyAt newPos = some k) ∧
    s'.len = s.len) ∨
  -- entry already at its new bucket (or slot empty): boundary shrinks, content unchanged
  (∀ i, s'.keyAt i = s.keyAt i) ∧ (∀ i, s'.valAt i = s.valAt i) ∧
  (∀ i, s'.dist i = s.dist i) ∧ s'.len = s.len

-- `remap_step` shrinks `remapEnd` while keeping every entry: entries are only relocated
-- to their new buckets, never added or dropped, so the entry set and count are preserved.
-- This is the invariant that makes target (a) hold for the remap. (The fine-grained
-- `RemapOne` move below does not alone imply it, because the relocation chain displaces
-- other entries; the implementation guarantees it by removing and re-inserting exactly one
-- entry.) src/map.rs L559-L597.
structure RemapStep (s s' : State) : Prop where
  keySet : KeySet s = KeySet s'
  len : s'.len = s.len
  boundary : match s.remapEnd, s'.remapEnd with
    | some e, some e' => e' < e
    | some _, none => True
    | none, none => True
    | none, some _ => False

end StableCluster
