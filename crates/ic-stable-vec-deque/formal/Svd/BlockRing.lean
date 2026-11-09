/-
Stage 1' — block-routing arithmetic for the V1 segmented block-ring
(ADR 0086 as built).

Logical index `i` sits at virtual position `r = (headOff + i) % virtCap`
(vec_deque.rs L394-L430 `virtual_index`/`slot_addr`; layout contract
L35-L58). Block `k = r / B`, slot `k' = r % B`, address
`dir k + k' * slotSize`. Core Lean only, no Mathlib.
-/
import Svd.Basic

namespace Svd

/-- Virtual position of logical index `i` (`(headOff + i) % virtCap`). -/
def virtualIndex (headOff virtCap i : Nat) : Nat := (headOff + i) % virtCap

/-- Directory position of a routed virtual position. -/
def blockOf (blockSlots r : Nat) : Nat := r / blockSlots

/-- Slot offset of a routed virtual position inside its block. -/
def slotOf (blockSlots r : Nat) : Nat := r % blockSlots

/-- Physical slot address through the directory. -/
def slotAddr (dir : Nat → Nat) (blockSlots slotSize r : Nat) : Nat :=
  dir (blockOf blockSlots r) + slotOf blockSlots r * slotSize

/-- Directory position after rotating the live entries by `rotateBy`
positions (`rotate_directory`, vec_deque.rs L471-L485): entry `k` becomes
entry `(k + rotateBy) % numBlocks`. -/
def rotateIndex (numBlocks rotateBy k : Nat) : Nat := (k + rotateBy) % numBlocks

/-! ## Routing extent and injection -/

/-- P1'-extent: a routed virtual position is inside `virtCap`. -/
theorem virtualIndex_lt {headOff virtCap i : Nat} (_hV : 0 < virtCap) :
    virtualIndex headOff virtCap i < virtCap :=
  Nat.mod_lt _ _hV

/-- P1'-injection: on the window `[0, len)` with `len ≤ virtCap`, distinct
logical indices never share a virtual position. -/
theorem virtualIndex_injective {headOff virtCap len a b : Nat} (_hV : 0 < virtCap)
    (hlen : len ≤ virtCap) (ha : a < len) (hb : b < len)
    (h : virtualIndex headOff virtCap a = virtualIndex headOff virtCap b) : a = b := by
  unfold virtualIndex at h
  have h' : (a + headOff) % virtCap = (b + headOff) % virtCap := by
    rw [Nat.add_comm a headOff, Nat.add_comm b headOff]
    exact h
  exact addModInjOnRange (N := virtCap) (w := headOff) (by omega) (by omega) h'

/-! ## Block/slot decomposition -/

/-- The block/slot decomposition reconstructs the virtual position. -/
theorem virtual_pos_decomp {blockSlots r : Nat} (_hB : 0 < blockSlots) :
    blockSlots * blockOf blockSlots r + slotOf blockSlots r = r := by
  unfold blockOf slotOf
  exact Nat.div_add_mod r blockSlots

/-- A routed position inside `virtCap = numBlocks * blockSlots` names a live
directory entry. -/
theorem blockOf_lt {blockSlots numBlocks virtCap r : Nat} (_hB : 0 < blockSlots)
    (hvc : virtCap = numBlocks * blockSlots) (hr : r < virtCap) :
    blockOf blockSlots r < numBlocks := by
  unfold blockOf
  rw [hvc] at hr
  exact div_lt_of_lt_mul' (by rwa [Nat.mul_comm] at hr)

/-- The slot offset is inside one block. -/
theorem slotOf_lt {blockSlots r : Nat} (hB : 0 < blockSlots) :
    slotOf blockSlots r < blockSlots :=
  Nat.mod_lt r hB

/-- P1'-address: a routed slot address is inside memory when its block base
plus one block of slots fits. -/
theorem slotAddr_lt_memBytes {dir : Nat → Nat} {blockSlots slotSize memBytes r : Nat}
    (hB : 0 < blockSlots) (hslot : 0 < slotSize)
    (hbase : dir (blockOf blockSlots r) + blockSlots * slotSize ≤ memBytes) :
    slotAddr dir blockSlots slotSize r < memBytes := by
  unfold slotAddr
  have hk := slotOf_lt (blockSlots := blockSlots) (r := r) hB
  have hmul : slotOf blockSlots r * slotSize < blockSlots * slotSize :=
    Nat.mul_lt_mul_of_pos_right hk hslot
  omega

/-! ## Directory rotation -/

/-- Rotation maps every live directory position into the live range. -/
theorem rotateIndex_lt {numBlocks rotateBy k : Nat} (_hn : 0 < numBlocks) :
    rotateIndex numBlocks rotateBy k < numBlocks :=
  Nat.mod_lt _ _hn

/-- P1'-rotation: rotation is injective on the live directory range, so it
permutes live entries without collapsing two blocks onto one. -/
theorem rotateIndex_injective {numBlocks rotateBy a b : Nat} (_hn : 0 < numBlocks)
    (ha : a < numBlocks) (hb : b < numBlocks)
    (h : rotateIndex numBlocks rotateBy a = rotateIndex numBlocks rotateBy b) : a = b := by
  unfold rotateIndex at h
  exact addModInjOnRange (N := numBlocks) (w := rotateBy) ha hb h

end Svd
