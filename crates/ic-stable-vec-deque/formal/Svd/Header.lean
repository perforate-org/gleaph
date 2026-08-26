/-
Stage 2 — the V1 header contract of `VecDeque::init` (vec_deque.rs L192-L232)
and validity of the fresh header written by `new` (vec_deque.rs L151-L169,
header fields L599-L608 `write_deque_header`).

Magic / version / element-bounds are assumed as hypotheses (the byte-compare
against `MAGIC = *b"SVD"` (L58), `LAYOUT_VERSION = 1` (L60), and
`bounds::<T>()` (L203) is out of the arithmetic scope). The layout checks that
remain are exactly the conjuncts of `HeaderValid`.
-/
import Svd.Basic

namespace Svd

/-- Persisted V1 header (vec_deque.rs L588-L597 `HeaderV1`). Magic, version,
`max_size`, and `is_fixed_size` are carried as opaque fields: their checks
(L197-L206) compare bytes against constants / type bounds and carry no
arithmetic content. -/
structure HeaderV1 where
  magic : Nat  -- 3-byte tag; checked by equality at L197-L199
  version : Nat -- byte; checked at L200-L202 against LAYOUT_VERSION = 1
  len : Nat
  maxSize : Nat
  isFixedSize : Bool
  head : Nat
  capacity : Nat

/-- Layout contract enforced by `VecDeque::init` after magic, version, and
element-bounds validation (vec_deque.rs L208-L226). A single-constructor
`Prop`-valued structure: the fields are the conjuncts, usable as projections. -/
structure HeaderValid (h : HeaderV1) (slotSize memSize : Nat) : Prop where
  /-- `cap == 0` branch (L208-L211): an empty ring has no entries and no
  offset front. -/
  capZero : h.capacity = 0 → h.len = 0 ∧ h.head = 0
  /-- `cap > 0` branch (L212-L214): occupied extent fits and the front is a
  real slot. -/
  capPos : h.capacity > 0 → h.len ≤ h.capacity ∧ h.head < h.capacity
  /-- Extra empty-deque check (L216-L218): zero length forces `head = 0`. -/
  emptyHead : h.len = 0 → h.head = 0
  /-- Allocated bytes must cover header plus data region for all slots
  (L220-L226; `DATA_OFFSET = 64`, L61). -/
  memFit : 64 + h.capacity * slotSize ≤ memSize

/-- The `SVD` magic tag as a natural number: the three ASCII bytes
`'S' 'V' 'D'` (vec_deque.rs L58) packed little-endian. -/
def svdMagic : Nat := 0x53 + 0x56 * 256 + 0x44 * 65536

/-- Fresh header written by `VecDeque::new` (vec_deque.rs L151-L169): V1 tag
values, empty ring (`len = 0`, `head = 0`, `capacity = 0`). The element-bounds
fields are whatever `bounds::<T>()` reports (L152), hence parameters here;
their checks are out of the arithmetic scope. -/
def initialHeader (maxSz : Nat) (fixed : Bool) : HeaderV1 :=
  { magic := svdMagic, version := 1, len := 0, maxSize := maxSz,
    isFixedSize := fixed, head := 0, capacity := 0 }

/-- P4-analog: the fresh header satisfies the layout contract for every slot
size and every allocated size covering the 64-byte header region
(vec_deque.rs L224-L226). -/
theorem initialHeader_valid (maxSz : Nat) (fixed : Bool) (slotSize memSize : Nat)
    (hmem : 64 ≤ memSize) :
    HeaderValid (initialHeader maxSz fixed) slotSize memSize := by
  have hcap : (initialHeader maxSz fixed).capacity = 0 := rfl
  constructor
  · intro _; exact ⟨rfl, rfl⟩
  · intro hc; omega
  · intro _; exact rfl
  · rw [hcap]; omega

end Svd
