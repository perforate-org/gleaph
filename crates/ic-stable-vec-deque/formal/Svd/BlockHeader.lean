/-
Stage 2' — V1 segmented-block-ring header validity (ADR 0086 as built).

The header occupies a 128-byte prefix (`DATA_OFFSET`); `init` validates magic,
version, element bounds, the block/directory geometry, the virtual capacity,
the front position, the directory extent, the free-list head, and every
directory entry (vec_deque.rs L220-L300; header read/write L790-L850). This
module models the arithmetic/geometry subset with exact `Nat` arithmetic
(u64 overflow is assumed away as in the fixture's A3 assumption). Core Lean
only, no Mathlib.
-/
import Svd.Basic

namespace Svd

/-- Byte offset of the element data region (the 128-byte header prefix). -/
def DATA_OFFSET : Nat := 128

/-- Target byte size of one storage block (`TARGET_BLOCK_BYTES`). -/
def TARGET_BLOCK_BYTES : Nat := 256 * 1024

/-- Directory capacity of a freshly created deque. -/
def INITIAL_DIR_SLOTS : Nat := 16

/-- Free-list nil marker (`u64::MAX`). -/
def FREE_LIST_NIL : Nat := 2 ^ 64 - 1

/-- Fixed number of slots per block for a given slot size:
`max 1 (TARGET_BLOCK_BYTES / slotSize)` (`block_slots_for`). -/
def blockSlotsFor (slotSize : Nat) : Nat := max 1 (TARGET_BLOCK_BYTES / slotSize)

/-- Semantic mirror of the persisted 128-byte header fields. -/
structure HeaderV1 where
  len : Nat
  maxSize : Nat
  fixedSize : Bool
  headOff : Nat
  virtCap : Nat
  dirBase : Nat
  dirSlots : Nat
  numBlocks : Nat
  blockSlots : Nat
  freeHead : Nat

/-- **P2'-validity**: the checked geometry of `init` (magic/version and element
bounds are assumed as the caller's hypotheses; byte-encode/decode is out of
scope). `dir` is the directory contents; entries beyond `numBlocks` are
scratch but still must fit the allocated memory, exactly as `init` checks every
entry in `0..dirSlots`. -/
def HeaderValid (h : HeaderV1) (dir : Nat → Nat) (slotSize memBytes maxSize : Nat)
    (fixedSize : Bool) : Prop :=
  0 < h.blockSlots
    ∧ h.blockSlots = blockSlotsFor slotSize
    ∧ (∃ p, h.dirSlots = 2 ^ p)
    ∧ h.numBlocks ≤ h.dirSlots
    ∧ h.virtCap = h.numBlocks * h.blockSlots
    ∧ h.len ≤ h.virtCap
    ∧ (h.len = 0 → h.headOff = 0)
    ∧ (0 < h.len → h.headOff < h.virtCap)
    ∧ h.maxSize = maxSize
    ∧ h.fixedSize = fixedSize
    ∧ DATA_OFFSET ≤ h.dirBase
    ∧ (h.freeHead = FREE_LIST_NIL ∨ h.freeHead < memBytes)
    ∧ h.dirBase + h.dirSlots * 8 ≤ memBytes
    ∧ (∀ k, k < h.dirSlots → dir k ≤ memBytes)
    ∧ (∀ k, k < h.numBlocks → dir k + h.blockSlots * slotSize ≤ memBytes)

/-- The exact header written by `VecDeque::new` (`new`, vec_deque.rs L171-L200). -/
def initialHeader (maxSize : Nat) (fixedSize : Bool) (slotSize dirBase : Nat) : HeaderV1 :=
  { len := 0
    maxSize := maxSize
    fixedSize := fixedSize
    headOff := 0
    virtCap := 0
    dirBase := dirBase
    dirSlots := INITIAL_DIR_SLOTS
    numBlocks := 0
    blockSlots := blockSlotsFor slotSize
    freeHead := FREE_LIST_NIL }

/-- `new` writes a header that passes the same validation `init` applies, given
the allocator facts (the 128-byte header and the 16-entry directory fit, and
every directory slot is inside memory). -/
theorem initialHeader_valid {dir : Nat → Nat} {slotSize memBytes maxSize dirBase : Nat}
    {fixedSize : Bool}
    (hdBase : DATA_OFFSET ≤ dirBase)
    (hdEnd : dirBase + INITIAL_DIR_SLOTS * 8 ≤ memBytes)
    (hdir : ∀ k, k < INITIAL_DIR_SLOTS → dir k ≤ memBytes) :
    HeaderValid (initialHeader maxSize fixedSize slotSize dirBase) dir slotSize memBytes
      maxSize fixedSize := by
  unfold HeaderValid
  dsimp [initialHeader]
  refine ⟨by unfold blockSlotsFor; exact Nat.lt_of_lt_of_le (by decide) (Nat.le_max_left _ _),
    rfl, ⟨4, by decide⟩, by decide, by simp, by decide, by intro _; rfl,
    by intro h; omega, rfl, rfl, hdBase, Or.inl rfl, hdEnd, hdir, ?_⟩
  intro k hk
  omega

end Svd
