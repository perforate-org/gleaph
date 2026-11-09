/- 36-bit CSR slab slot indices, locator words, vertex tail28, and 40-bit
byte-offset arithmetic: transcription of `src/slab_index.rs` (L1-L239; re-anchored at `d75d2467`).
Wire words are `Nat`; legacy `i32` values are `Int`. Core Lean only.
-/
import Lar.Basic

namespace Lar

/-! ## Constants (slab_index.rs L4-L25, L158-L160) -/

/-- Width of a global slab slot index (`base_slot_start`, `edge_start`, …). -/
def slotIndexBits : Nat := 36

/-- All-ones mask for a valid slot index (`0xF_FFFF_FFFF`). -/
def slotIndexMask : Nat := 2 ^ slotIndexBits - 1

/-- Maximum exclusive end of a slot range (`2^36`). -/
def maxSlotExclusiveEnd : Nat := slotIndexMask + 1

/-- Width of metadata packed above a slot index in a labeled locator word. -/
def meta28Bits : Nat := 28

/-- All-ones mask for the 28-bit metadata region. -/
def meta28Mask : Nat := 2 ^ meta28Bits - 1

/-- Width of a global byte offset in `EdgeInlinePropertyBytesStore`. -/
def byteOffsetBits : Nat := 40

/-- All-ones mask for a valid byte offset (`2^40 − 1`, slab_index.rs L23). -/
def byteOffsetMask : Nat := 2 ^ byteOffsetBits - 1

/-- `byte_offset_fits` (slab_index.rs L40-L43): fits the 40-bit space. -/
def byteOffsetFits (offset : Nat) : Prop := offset ≤ byteOffsetMask

/-- Maximum `(spill_run + 1)` encoding for unlabeled vertex tail28
(slab_index.rs L163). -/
def vertexTailRunMask : Nat := 2 ^ 27 - 1

/-- Legacy `i32` exclusive upper bound (`2^31`) used to model `as u32`. -/
def i32Bound : Int := 2147483648

theorem slotIndexMask_lt_two_pow : slotIndexMask < 2 ^ slotIndexBits := by
  have hpos := pow_pos slotIndexBits
  unfold slotIndexMask
  omega

theorem meta28Mask_lt_two_pow : meta28Mask < 2 ^ meta28Bits := by
  have hpos := pow_pos meta28Bits
  unfold meta28Mask
  omega

theorem byteOffsetMask_lt_two_pow : byteOffsetMask < 2 ^ byteOffsetBits := by
  have hpos := pow_pos byteOffsetBits
  unfold byteOffsetMask
  omega

theorem vertexTailRunMask_lt_two_pow : vertexTailRunMask < 2 ^ 27 := by
  have hpos := pow_pos 27
  unfold vertexTailRunMask
  omega

theorem maxSlotExclusiveEnd_eq_two_pow : maxSlotExclusiveEnd = 2 ^ slotIndexBits := by
  unfold maxSlotExclusiveEnd slotIndexMask
  have hpos := pow_pos slotIndexBits
  omega

/-! ## Checked addition (slab_index.rs L52-L56, L119-L123) -/

/-- `checked_add_slot_index` (slab_index.rs L119-L121). The u64 `checked_add`
cannot fail for in-range inputs (`lhs, rhs ≤ 2^36 − 1 ⇒ sum < 2^64`), so the
only failure mode is the 36-bit filter. -/
def checkedAddSlotIndex (lhs rhs : Nat) : Option Nat :=
  if lhs + rhs ≤ slotIndexMask then some (lhs + rhs) else none

/-- Success criterion of `checked_add_slot_index`: exactly `sum ≤ mask`,
never a silent wrap (P4-extent). -/
theorem checkedAddSlotIndex_spec {lhs rhs : Nat} (_hl : lhs ≤ slotIndexMask)
    (_hr : rhs ≤ slotIndexMask) :
    checkedAddSlotIndex lhs rhs = some (lhs + rhs) ↔ lhs + rhs ≤ slotIndexMask := by
  unfold checkedAddSlotIndex
  by_cases hf : lhs + rhs ≤ slotIndexMask
  · rw [if_pos hf]
    simp
    omega
  · rw [if_neg hf]
    simp
    omega

/-- `checked_add_byte_offset` (slab_index.rs L52-L54). -/
def checkedAddByteOffset (lhs rhs : Nat) : Option Nat :=
  if lhs + rhs ≤ byteOffsetMask then some (lhs + rhs) else none

theorem checkedAddByteOffset_spec {lhs rhs : Nat} (_hl : lhs ≤ byteOffsetMask)
    (_hr : rhs ≤ byteOffsetMask) :
    checkedAddByteOffset lhs rhs = some (lhs + rhs) ↔ lhs + rhs ≤ byteOffsetMask := by
  unfold checkedAddByteOffset
  by_cases hf : lhs + rhs ≤ byteOffsetMask
  · rw [if_pos hf]
    simp
    omega
  · rw [if_neg hf]
    simp
    omega

/-! ## Locator words (slab_index.rs L64-L108) -/

/-- Lower 36 bits of a packed locator or bucket word (slab_index.rs L70-L72). -/
def decodeSlotIndex (word : Nat) : Nat := word % 2 ^ slotIndexBits

/-- Upper 28 bits of a labeled vertex locator word (slab_index.rs L70-L74). -/
def decodeMeta28 (word : Nat) : Nat := midBits slotIndexBits meta28Bits word

/-- Fallible locator packing (slab_index.rs L82-L89). -/
def tryEncodeLocatorWord (slot meta28 : Nat) : Option Nat :=
  if slot ≤ slotIndexMask ∧ meta28 ≤ meta28Mask then
    some (pack2 slotIndexBits slot meta28)
  else none

/-- Fallible slot replacement (slab_index.rs L97-L104). -/
def tryReplaceSlotIndex (word slot : Nat) : Option Nat :=
  if slot ≤ slotIndexMask then some (replaceLow slotIndexBits word slot) else none

/-- Encoding round-trips: both fields decode back unchanged (P1-roundtrip). -/
theorem locator_decode_of_tryEncode {slot meta28 w : Nat}
    (hs : slot ≤ slotIndexMask) (hm : meta28 ≤ meta28Mask)
    (hw : tryEncodeLocatorWord slot meta28 = some w) :
    decodeSlotIndex w = slot ∧ decodeMeta28 w = meta28 := by
  have hslt : slot < 2 ^ slotIndexBits := Nat.lt_of_le_of_lt hs slotIndexMask_lt_two_pow
  have hmlt : meta28 < 2 ^ meta28Bits := Nat.lt_of_le_of_lt hm meta28Mask_lt_two_pow
  unfold tryEncodeLocatorWord at hw
  rw [if_pos ⟨hs, hm⟩] at hw
  have hpk : pack2 slotIndexBits slot meta28 = w := Option.some.inj hw
  subst hpk
  refine ⟨?_, ?_⟩
  · exact pack2_low hslt
  · unfold decodeMeta28 midBits
    rw [pack2_high hslt]
    exact lt_two_pow hmlt

/-- Locator encoding is injective on the valid field domain (P2-injection). -/
theorem locator_encode_injective {slot1 meta28_1 slot2 meta28_2 : Nat}
    (hs1 : slot1 ≤ slotIndexMask) (hm1 : meta28_1 ≤ meta28Mask)
    (hs2 : slot2 ≤ slotIndexMask) (hm2 : meta28_2 ≤ meta28Mask)
    (h : tryEncodeLocatorWord slot1 meta28_1 = tryEncodeLocatorWord slot2 meta28_2) :
    slot1 = slot2 ∧ meta28_1 = meta28_2 := by
  have h1 : slot1 < 2 ^ slotIndexBits := Nat.lt_of_le_of_lt hs1 slotIndexMask_lt_two_pow
  have h2 : slot2 < 2 ^ slotIndexBits := Nat.lt_of_le_of_lt hs2 slotIndexMask_lt_two_pow
  unfold tryEncodeLocatorWord at h
  rw [if_pos ⟨hs1, hm1⟩, if_pos ⟨hs2, hm2⟩] at h
  exact pack2_inj h1 h2 (Option.some.inj h)

/-- Replacing the slot field preserves the meta28 field (P3-noninterference,
via `replaceLow_mid`). -/
theorem replaceSlot_preserves_meta28 {word slot w : Nat} (hs : slot ≤ slotIndexMask)
    (hw : tryReplaceSlotIndex word slot = some w) :
    decodeMeta28 w = decodeMeta28 word := by
  unfold tryReplaceSlotIndex at hw
  rw [if_pos hs] at hw
  have hrw : replaceLow slotIndexBits word slot = w := Option.some.inj hw
  subst hrw
  exact replaceLow_mid (Nat.lt_of_le_of_lt hs slotIndexMask_lt_two_pow)

/-- Replacing the slot field installs exactly the new slot (P3-noninterference). -/
theorem replaceSlot_sets_slot {word slot w : Nat} (hs : slot ≤ slotIndexMask)
    (hw : tryReplaceSlotIndex word slot = some w) :
    decodeSlotIndex w = slot := by
  unfold tryReplaceSlotIndex at hw
  rw [if_pos hs] at hw
  have hrw : replaceLow slotIndexBits word slot = w := Option.some.inj hw
  subst hrw
  exact replaceLow_low (Nat.lt_of_le_of_lt hs slotIndexMask_lt_two_pow)

/-! ## Vertex tail28 (slab_index.rs L154-L199) -/

theorem toNat_lt_i32Bound {h : Int} (h0 : 0 ≤ h) (hb : h < i32Bound) :
    h.toNat < 2147483648 := by
  have hc : ((h.toNat : Nat) : Int) = h := Int.toNat_of_nonneg h0
  have hi : ((h.toNat : Nat) : Int) < i32Bound := by rw [hc]; exact hb
  unfold i32Bound at hi
  omega

/-- The `wrapping_add(1)` of slab_index.rs L169 cannot wrap for any `i32`
value (`max 2^31 − 1`, plus one stays below `2^32`). -/
theorem tailEnc_noWrap {spillRun : Int} (h0 : 0 ≤ spillRun) (hdom : spillRun < i32Bound) :
    (spillRun.toNat + 1) % 4294967296 = spillRun.toNat + 1 := by
  have hb := toNat_lt_i32Bound h0 hdom
  exact Nat.mod_eq_of_lt (by omega)

/-- `pack_vertex_tail28` (slab_index.rs L167-L181): bit 0 tombstone,
bits 1–27 carry `(spill_run + 1)` with `0` meaning "no spill run". -/
def packVertexTail28 (spillRun : Int) (tombstone : Bool) : Nat :=
  (if spillRun < 0 then 0 else (spillRun.toNat + 1) % 4294967296) * 2
    + (if tombstone then 1 else 0)

/-- `unpack_vertex_tail28` (slab_index.rs L185-L190): tombstone is bit 0,
the payload is bits 1–27, `payload = 0` decodes to `-1`. -/
def unpackVertexTail28 (raw : Nat) : Int × Bool :=
  ((if raw / 2 % 2 ^ 27 = 0 then (-1 : Int)
    else ((((raw / 2 % 2 ^ 27) - 1) : Nat) : Int)),
    raw % 2 == 1)

/-- `try_pack_vertex_tail28` (slab_index.rs L191-L199): rejects non-negative
run ids whose `(run + 1)` exceeds the 27-bit payload. -/
def tryPackVertexTail28 (spillRun : Int) (tombstone : Bool) : Option Nat :=
  if 0 ≤ spillRun ∧ (spillRun.toNat + 1) % 4294967296 > vertexTailRunMask then none
  else some (packVertexTail28 spillRun tombstone)

theorem div2_mul2_add {m b : Nat} (hb : b < 2) :
    (m * 2 + b) / 2 = m ∧ (m * 2 + b) % 2 = b := by
  rw [Nat.mul_comm m 2]
  refine ⟨?_, ?_⟩
  · rw [Nat.mul_add_div (pow_pos 1), Nat.div_eq_of_lt hb, Nat.add_zero]
  · rw [Nat.mul_add_mod, Nat.mod_eq_of_lt hb]

/-- Unpack ∘ pack recovers the canonicalized pair: any negative becomes `-1`,
non-negative indices are preserved exactly (P5-sentinel-faithfulness). The
acceptance hypothesis mirrors successful `try_pack`, preventing 27-bit
payload truncation. -/
theorem unpack_pack_canonical (spillRun : Int) (tombstone : Bool)
    (hdom : spillRun < i32Bound)
    (hacc : spillRun < 0 ∨ spillRun.toNat + 1 ≤ vertexTailRunMask) :
    unpackVertexTail28 (packVertexTail28 spillRun tombstone)
      = (if spillRun < 0 then (-1 : Int) else spillRun, tombstone) := by
  by_cases hneg : spillRun < 0
  · -- negative: encoding writes payload 0 ("no spill run"), decoded back as -1
    rw [if_pos hneg]
    have hpk : packVertexTail28 spillRun tombstone = (if tombstone then 1 else 0) := by
      unfold packVertexTail28
      rw [if_pos hneg]
      cases tombstone <;> simp
    cases tombstone <;> simp [unpackVertexTail28, hpk]
  · -- non-negative: enc = index + 1 without wrap
    rw [if_neg hneg]
    have h0 : 0 ≤ spillRun := by omega
    have hn := tailEnc_noWrap h0 hdom
    rcases hacc with hcontra | hidx
    · exact absurd hcontra hneg
    · have hcast : ((spillRun.toNat : Nat) : Int) = spillRun :=
        Int.toNat_of_nonneg h0
      have hmod27 : (spillRun.toNat + 1) % 2 ^ 27 = spillRun.toNat + 1 :=
        Nat.mod_eq_of_lt
          (Nat.lt_of_le_of_lt hidx vertexTailRunMask_lt_two_pow)
      cases tombstone
      · have hsplit := div2_mul2_add (m := spillRun.toNat + 1) (b := 0) (by omega)
        simp [unpackVertexTail28, packVertexTail28, hn, hmod27, if_neg hneg, hcast]
      · have hsplit := div2_mul2_add (m := spillRun.toNat + 1) (b := 1) (by omega)
        simp [unpackVertexTail28, packVertexTail28, hn, hmod27, hsplit.1, hsplit.2,
          if_neg hneg, hcast]

/-- `try_pack_vertex_tail28` fails exactly for non-negative run ids whose
`(run + 1)` exceeds the 27-bit payload (fail-closed bound,
slab_index.rs L194-L202). -/
theorem tryPackVertexTail28_none_iff (spillRun : Int) (tombstone : Bool)
    (hdom : spillRun < i32Bound) :
    tryPackVertexTail28 spillRun tombstone = none
      ↔ 0 ≤ spillRun ∧ spillRun.toNat + 1 > vertexTailRunMask := by
  unfold tryPackVertexTail28
  constructor
  · intro h
    by_cases hc : 0 ≤ spillRun ∧ (spillRun.toNat + 1) % 4294967296 > vertexTailRunMask
    · rw [if_pos hc] at h
      refine ⟨hc.1, ?_⟩
      have hn := tailEnc_noWrap hc.1 hdom
      omega
    · rw [if_neg hc] at h
      exact absurd h (by simp)
  · rintro ⟨h0, hgt⟩
    refine if_pos ⟨h0, ?_⟩
    rw [tailEnc_noWrap h0 hdom]
    exact hgt

end Lar
