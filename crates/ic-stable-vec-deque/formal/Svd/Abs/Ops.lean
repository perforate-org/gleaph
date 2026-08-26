/-
Stage-3' operation definitions for the segmented block-ring (ADR 0086 as
built). Each definition mirrors its Rust counterpart; contracts are stated on
the observable reading list (`logicalList`) and will be added alongside their
proofs in the next increment.

Growth is modeled separately (`Svd.Abs.Grow`): the pushes here take the
not-full path; the full-push composition with `opGrow` will be stated in
`Grow.lean`.
-/
import Svd.Abs.Transfer

namespace Svd.Abs

variable {α : Type}

/-! ## Routing helpers -/

/-- Distinct logical positions inside a window `[0, w)` with `w ≤ virtCap`
never share a routed `(directory position, offset)` pair. -/
theorem routed_pair_inj {st : DequeState α} {a b w : Nat}
    (_hB : 0 < st.blockSlots) (hw : w ≤ st.virtCap) (ha : a < w) (hb : b < w)
    (hk : ((st.headOff + a) % st.virtCap) / st.blockSlots
        = ((st.headOff + b) % st.virtCap) / st.blockSlots)
    (hs : ((st.headOff + a) % st.virtCap) % st.blockSlots
        = ((st.headOff + b) % st.virtCap) % st.blockSlots) : a = b := by
  have hra := Nat.div_add_mod (((st.headOff + a) % st.virtCap)) st.blockSlots
  have hrb := Nat.div_add_mod (((st.headOff + b) % st.virtCap)) st.blockSlots
  rw [← hk, ← hs] at hrb
  have heq : (st.headOff + a) % st.virtCap = (st.headOff + b) % st.virtCap := by
    omega
  have hc3 : (a + st.headOff) % st.virtCap = (b + st.headOff) % st.virtCap := by
    have h1 : (a + st.headOff) = (st.headOff + a) := Nat.add_comm a st.headOff
    have h2 : (b + st.headOff) = (st.headOff + b) := Nat.add_comm b st.headOff
    rw [h1, h2]
    exact heq
  exact addModInjOnRange (N := st.virtCap) (w := st.headOff)
    (by omega) (by omega) hc3

/-- Windowed specialization used by `set`. -/
theorem routed_pair_inj_len {st : DequeState α} (inv : Inv st) {a b : Nat}
    (ha : a < st.len) (hb : b < st.len)
    (hk : ((st.headOff + a) % st.virtCap) / st.blockSlots
        = ((st.headOff + b) % st.virtCap) / st.blockSlots)
    (hs : ((st.headOff + a) % st.virtCap) % st.blockSlots
        = ((st.headOff + b) % st.virtCap) % st.blockSlots) : a = b :=
  routed_pair_inj inv.bsPos inv.lenLe ha hb hk hs

/-! ## Operation definitions -/

/-- `get`: out-of-range reads are `none`, otherwise the routed two-level lookup. -/
def opGet (st : DequeState α) (p : Nat) : Option α :=
  if p < st.len then contentOf st p else none

/-- `set`: overwrite the routed slot; header untouched. -/
def opSet (st : DequeState α) (p : Nat) (v : α) : DequeState α :=
  { st with
    blocks := updBlock st.blocks (st.dir (routeBlock st p))
      (routeSlot st p) (some v) }

/-- `push_back` without growth: write past current back and advance len. -/
def opPushBack (st : DequeState α) (v : α) : DequeState α :=
  { st with
    blocks := updBlock st.blocks (st.dir (routeBlock st st.len))
      (routeSlot st st.len) (some v)
    len := st.len + 1 }

/-- New front position for push_front: `(headOff + virtCap - 1) % virtCap`. -/
def prevHead (st : DequeState α) : Nat :=
  (st.headOff + st.virtCap - 1) % st.virtCap

/-- `push_front` without growth: rebase front backward, write, advance len. -/
def opPushFront (st : DequeState α) (v : α) : DequeState α :=
  { st with
    blocks := updBlock st.blocks (st.dir (prevHead st / st.blockSlots))
      (prevHead st % st.blockSlots) (some v)
    headOff := prevHead st
    len := st.len + 1 }

/-- Top-block retirement: drop last directory entry, shrink virtual capacity,
recycle base onto free list. -/
def retireTop (st : DequeState α) : DequeState α :=
  { st with
    numBlocks := st.numBlocks - 1
    virtCap := (st.numBlocks - 1) * st.blockSlots
    free := st.dir (st.numBlocks - 1) :: st.free }

/-- `pop_back`: read last slot, shrink; retire top block when drained; reset
front to 0 when emptied. -/
def opPopBack (st : DequeState α) : Option α × DequeState α :=
  match st.len with
  | 0 => (none, st)
  | n + 1 =>
      let r := (st.headOff + n) % st.virtCap
      let topStart := (st.numBlocks - 1) * st.blockSlots
      let retire := r / st.blockSlots + 1 = st.numBlocks ∧
        (n = 0 ∨ st.headOff + n ≤ topStart)
      let s1 : DequeState α := { st with len := n }
      let s2 := if retire then retireTop s1 else s1
      let s3 := if n = 0 then { s2 with headOff := 0 } else s2
      (contentOf st n, s3)

/-- `pop_front`: read front slot, shrink; retire top block when drained;
reset/advance front accordingly. -/
def opPopFront (st : DequeState α) : Option α × DequeState α :=
  match st.len with
  | 0 => (none, st)
  | n + 1 =>
      let wrapping := st.headOff + 1 = st.virtCap
      let drained := n = 0 ∨
        (if wrapping then n + st.blockSlots ≤ st.virtCap
          else (st.headOff + 1) % st.blockSlots = 0)
      let retire := drained ∧ st.headOff / st.blockSlots + 1 = st.numBlocks
      let s1 : DequeState α := { st with len := n }
      let s2 := if retire then retireTop s1 else s1
      let head' := if n = 0 then 0 else (st.headOff + 1) % st.virtCap
      let s3 := { s2 with headOff := head' }
      (st.blocks (st.dir (st.headOff / st.blockSlots))
        (st.headOff % st.blockSlots), s3)

end Svd.Abs
