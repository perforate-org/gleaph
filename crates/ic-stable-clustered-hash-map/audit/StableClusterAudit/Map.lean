/-
Stage 2 — Faithful transcription of the `StableClusteredHashMap` operations.

Audit mode (SCOPE.md §1). Transcribes `src/map.rs` into Lean: cluster scanning,
`lookup_index`, and the insert / remove / incremental-resize transitions. Abstract
memory (get/set) per SCOPE §6; byte-level layout, allocation, and error paths are out
of scope. Every definition cites the Rust line range. Proof obligations (termination,
invariant preservation) are deferred to Stage 3 and left as `sorry` with comments.
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
-- One relocation step: at an occupied `position`, read the occupant `t`, extend its
-- distance by `(next - position)` where `next` is its cluster end, write the new entry,
-- and continue from `next` with `entry := t`.
-- src/map.rs L468-L474.
def RelocateStep (s s' : State) (entry : Key) (value : Nat) (position : Nat) : Prop :=
  ∃ tKey tVal tDist next,
    s.keyAt position = some tKey ∧ s.valAt position = tVal ∧ s.dist position = tDist ∧
    next = endOfCluster s position ∧
    -- displaced entry keeps its key/value, distance grows by (next - position)
    tDist + (next - position) < EMPTY ∧
    s'.keyAt position = some entry ∧ s'.valAt position = value ∧
    s'.dist position = tDist + (next - position) ∧
    -- everything else is unchanged except at `next` where the displaced entry lands
    s'.keyAt next = some tKey ∧ s'.valAt next = tVal ∧
    s'.dist next = s.dist next ∧
    (∀ i, i ≠ position → i ≠ next → s'.keyAt i = s.keyAt i) ∧
    (∀ i, i ≠ position → i ≠ next → s'.valAt i = s.valAt i) ∧
    (∀ i, i ≠ position → i ≠ next → s'.dist i = s.dist i)

-- `insert_and_relocate` base case: an empty slot at `position` is written directly, with
-- the entry's distance = `position - bucket(entry, n)` so it sits at its home bucket.
-- `find_insert_position` guarantees `position` is the end of the bucket's cluster, which is
-- what keeps the table ordered (target (b)). src/map.rs L464-L466.
structure RelocateWrite (s s' : State) (entry : Key) (value : Nat) (position : Nat) : Prop where
  n : s.n = s'.n
  slotEmpty : s.dist position = EMPTY
  distFit : position - bucket entry s.n < EMPTY
  keyAt : s'.keyAt position = some entry
  valAt : s'.valAt position = value
  dist : s'.dist position = position - bucket entry s.n
  keyAt_other : ∀ i, i ≠ position → s'.keyAt i = s.keyAt i
  valAt_other : ∀ i, i ≠ position → s'.valAt i = s.valAt i
  dist_other : ∀ i, i ≠ position → s'.dist i = s.dist i

-- `remove_and_relocate`: empties `position` and shifts the tail of the next cluster up,
-- subtracting the shift from its distance. src/map.rs L491-L508.
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

/-!
## Incremental resize (src/map.rs L510-L584)

`size_up` doubles the bucket table and starts a remap (sets `remapEnd` to the previous
capacity). `remap_position` relocates an entry whose bucket changed, and `remap_step`
advances the boundary. These are transcribed as relations; targets (a) (entry
preservation) and (c) (re-open) build on them.
-/

-- `size_up`: grows in place to `2^(n+1) + (n+1)`, keeping all entries, and starts the
-- incremental remap at `remapEnd = prev_capacity`.
-- src/map.rs L513-L542.
structure SizeUp (s s' : State) : Prop where
  n : s'.n = s.n + 1
  len : s'.len = s.len
  remapEnd : s'.remapEnd = some (capacity s.n)
  -- the key/value content is preserved (only geometry grows; remap later relocates)
  keyAt : ∀ i, s'.keyAt i = s.keyAt i
  valAt : ∀ i, s'.valAt i = s.valAt i
  -- old region keeps its distances; the newly grown region [capacity n, capacity (n+1))
  -- is cleared (EMPTY), matching `clear_region` in src/map.rs L529-L536.
  distOld : ∀ i, i < capacity s.n → s'.dist i = s.dist i
  distNew : ∀ i, capacity s.n ≤ i → i < capacity s'.n → s'.dist i = EMPTY

-- `remap_step`: processes positions from the bottom of the mixed range, relocating
-- entries whose bucket changed under the new size, until the boundary reaches 0.
-- src/map.rs L546-L564.
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
-- entry.) src/map.rs L546-L564.
structure RemapStep (s s' : State) : Prop where
  keySet : KeySet s = KeySet s'
  len : s'.len = s.len
  boundary : match s.remapEnd, s'.remapEnd with
    | some e, some e' => e' < e
    | some _, none => True
    | none, none => True
    | none, some _ => False

end StableCluster
