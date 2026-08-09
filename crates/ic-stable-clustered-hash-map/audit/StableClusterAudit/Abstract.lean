/-
Stage 1 — Abstract model and invariants for `StableClusteredHashMap`.

Audit mode (SCOPE.md §1). The Rust implementation in `src/map.rs` is the target; every
definition carries a reference comment `-- src/map.rs L<start>-<end>`.

Modeling choices (SCOPE.md §6): abstract memory (get/set), byte-level layout, header
magic, memory growth and `OutOfMemory`/`InsertError` paths are out of scope. Keys are an
abstract type; the hash is a deterministic function (SCOPE.md §5).
-/

import Mathlib

namespace StableCluster

/-!
## Keys and hashing
-/

-- Abstract key type. The implementation requires `K: Storable + PartialEq`; here we only
-- need decidable equality for probing. `Classical.decEq` supplies it non-constructively,
-- which is all the model requires.
-- src/map.rs L106: `impl<K: Storable + PartialEq, V: Storable, M: Memory>`
axiom Key : Type
noncomputable instance : DecidableEq Key := Classical.decEq Key

-- Deterministic hash of a key (rapidhash v3, constant seed). The hash internals are out
-- of scope; we treat it as a deterministic function of the key.
-- src/map.rs L83-L87: `fn hash_key` uses `rapidhash_v3_inline(..., &DEFAULT_RAPID_SECRETS)`.
axiom hash : Key → Nat

-- `bucket(key, n)` = lower `n` bits of the hash. `n = 0` yields 0, matching
-- `fn bucket` which returns 0 when `n == 0`. Modeled as `hash % 2^n`; the Fibonacci
-- multiplier (`FIB_CONST`) only permutes which bits are "lower n bits", so it is
-- irrelevant to the invariants.
-- src/map.rs L74-L81: `fn bucket`.
noncomputable def bucket (key : Key) (n : Nat) : Nat := hash key % 2 ^ n

/-!
## Abstract state
-/

-- Table geometry: `capacity(n) = 2^n + n` (bucket table + overflow area of size n).
-- src/map.rs L157-L161: `pub fn capacity`.
def capacity (n : Nat) : Nat := 2 ^ n + n
-- src/map.rs L152-L155: `pub fn buckets` returns `1 << log2_buckets`.
def buckets (n : Nat) : Nat := 2 ^ n

-- Empty distance marker (`u32::MAX`). Real distances are far smaller.
-- src/map.rs: `const EMPTY: u32 = u32::MAX`.
def EMPTY : Nat := 4294967295

-- The map state over abstract memory. `dist`, `keyAt`, `valAt` model the slot table;
-- header fields `len`, `log2_buckets` (n), and the incremental-resize boundary are
-- modeled directly.
-- src/map.rs L101-L104 (struct), L147-L191 (header accessors).
structure State where
  n : Nat                 -- log2_buckets
  len : Nat               -- number of entries
  remapEnd : Option Nat   -- mixed-range boundary; `none` = no resize in progress.
                          -- The implementation uses `u64::MAX` as the "none" marker.
                          -- src/map.rs L185-L192.
  dist : Nat → Nat        -- distance per slot i; `EMPTY` marks an empty slot.
  keyAt : Nat → Option Key
  valAt : Nat → Nat       -- value at slot i (meaningful only when occupied)

-- A slot is occupied iff its distance is not the EMPTY marker.
-- src/map.rs L279-L281: `is_empty_slot` returns `read_distance(i) == EMPTY`.
@[reducible] def IsOccupied (s : State) (i : Nat) : Prop := s.dist i ≠ EMPTY

-- Slot index within the table.
def InBounds (s : State) (i : Nat) : Prop := i < capacity s.n

-- Bucket of the entry at slot i, derived from position and distance.
-- src/map.rs L283-L286: `fn bucket_by_position` = `i - read_distance(i)`.
@[reducible] def BucketAt (s : State) (i : Nat) : Nat := i - s.dist i

-- True when slot i lies in the mixed range [0, remapEnd] of an in-progress resize.
-- Entries there still use the OLD table size (n-1).
-- src/map.rs L349-L370 (lookup mixed-range check), L544-L563 (remap_step).
def InMixedRange (s : State) (i : Nat) : Prop :=
  match s.remapEnd with
  | none => False
  | some e => i ≤ e

-- The bucket an entry at slot i *should* occupy: the new size (n) outside the mixed
-- range, the old size (n-1) inside it. Unoccupied slots yield 0 (never compared).
-- src/map.rs L351-L353 (`prev_bucket = bucket(hash, n - 1)` when `n > 0`).
noncomputable def ExpectedBucket (s : State) (i : Nat) : Nat :=
  match s.keyAt i, s.remapEnd with
  | none, _ => 0
  | some k, none => bucket k s.n
  | some k, some e => if i ≤ e then bucket k (s.n - 1) else bucket k s.n

/-!
## Cluster invariant (target (b))
-/

-- (B1) An occupied slot's distance never exceeds its position (so `BucketAt` does not
--      underflow).
def DistanceValid (s : State) : Prop :=
  ∀ i, i < capacity s.n → IsOccupied s i → s.dist i ≤ i

-- (B2) Ordered-cluster invariant (Amble & Knuth): over occupied slots, the derived
--      bucket `i - dist i` is non-decreasing as `i` increases. This is what makes the
--      probe in `lookup_index` correct and terminating (stop at first empty / out-of-
--      cluster slot).
--      src/map.rs L334-L348 (lookup_index scan), L289-L319 (cluster scanning).
def ClusterOrdered (s : State) : Prop :=
  ∀ i j, i < capacity s.n → j < capacity s.n → i < j →
    IsOccupied s i → IsOccupied s j → BucketAt s i ≤ BucketAt s j

-- (B3) Every occupied entry sits at the bucket its hash maps to under the appropriate
--      table size (old size inside the mixed range, new size outside it).
def EntryAtCorrectBucket (s : State) : Prop :=
  ∀ i, i < capacity s.n → IsOccupied s i → BucketAt s i = ExpectedBucket s i

-- (B4) NOT a structural invariant. The claim "distances are bounded by the overflow
--      area n" (old code comment) is false: a low bucket's cluster can grow past n
--      (see Counterexamples.lean). Kept only for reference.
def DistanceBounded (s : State) : Prop :=
  ∀ i, i < capacity s.n → IsOccupied s i → s.dist i ≤ s.n

-- The cluster invariant: the structural properties that make probing correct and
-- terminating. `DistanceBounded` is deliberately excluded (see Counterexamples.lean);
-- u32 distance fit is enforced at insert via `checked_distance` (which traps on
-- overflow), not by the structure.
def ClusterInvariant (s : State) : Prop :=
  DistanceValid s ∧ ClusterOrdered s ∧ EntryAtCorrectBucket s

/-!
## Target properties (SCOPE.md §4)
-/

-- The set of keys currently in the map.
def KeySet (s : State) : Key → Prop :=
  fun k => ∃ i, i < capacity s.n ∧ IsOccupied s i ∧ s.keyAt i = some k

-- (a) A resize (`size_up` followed by `remap_step`/`remap_position` steps) is a pure
--     remap: it must preserve the entry set and the entry count (no loss, duplication,
--     or misplacement).
--     src/map.rs L510-L542 (size_up), L544-L584 (remap_step / remap_position).
def ResizePreservesEntries (s s' : State) : Prop :=
  KeySet s = KeySet s' ∧ s.len = s'.len

-- (b) Every mutation (insert / remove) and every resize step preserves the cluster
--     invariant.
--     src/map.rs L411-L441 (insert), L479-L487 (remove), L447-L476
--     (insert_and_relocate), L491-L508 (remove_and_relocate).
def InsertPreservesInvariant (s s' : State) : Prop :=
  ClusterInvariant s → ClusterInvariant s'

def RemovePreservesInvariant (s s' : State) : Prop :=
  ClusterInvariant s → ClusterInvariant s'

def RemapStepPreservesInvariant (s s' : State) : Prop :=
  ClusterInvariant s → ClusterInvariant s'

-- Lookup correctness: `k` is found iff it occupies a slot at its expected bucket.
-- The concrete probing logic is transcribed in Map.lean (Stage 2); here we state the
-- property that `lookup_index` must satisfy.
-- src/map.rs L325-L372 (lookup_index).
def LookupFound (s : State) (k : Key) : Prop :=
  ∃ i, i < capacity s.n ∧ IsOccupied s i ∧ s.keyAt i = some k ∧
       BucketAt s i = ExpectedBucket s i

-- (c) Re-open mid-resize consistency: a persisted state (header + slots) reconstructs
--     a valid map whose lookup finds exactly the stored keys (under both the old and
--     new mappings).
--     src/map.rs L107-L126 (init), L349-L370 (mixed-range lookup).
def ReopenConsistent (s : State) : Prop :=
  ClusterInvariant s ∧ (∀ k, KeySet s k ↔ LookupFound s k)

/-!
## Assumptions (SCOPE.md §5)

The SCOPE assumptions are meta-assumptions about the environment; they are documented
here rather than axiomatized (an axiom stating `True` has no content). If a Stage 3
proof genuinely requires one, it is added here and managed centrally, per the audit
workflow:

- Single-threaded execution (canister); no concurrency.
- Abstract memory get/set is correct; no corruption or external tampering.
- `hash` is deterministic (this holds by construction: `hash` is a total function and
  `bucket` is derived from it, so the old/new table sizes agree on the same key).
- Arithmetic bounds hold: `len`/`capacity` fit in `u64`; distances fit in `u32`.
  Unlike the old `u16` storage, this is not a structural invariant of the clustering:
  it is enforced at insert by `checked_distance`, which traps on overflow (rolling back
  the whole IC message). Under IC stable-memory bounds and realistic key/value sizes
  the max distance is far below `u32::MAX`.
- Callers honor the documented aliasing rule (`&self` mutation, no aliasing while an
  iterator is alive).
-/

end StableCluster
