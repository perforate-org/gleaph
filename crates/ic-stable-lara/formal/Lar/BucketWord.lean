/- Bucket word packing: transcription of `src/labeled/slot_index.rs`
(L10-L75). Field layout: slot `[0,36)` / label key `[36,52)` /
overflow-log head byte `[52,60)` / reserved nibble `[60,64)`.
Core Lean only.
-/
import Lar.BucketLabelKey

namespace Lar

/-! ## Field layout constants (labeled/slot_index.rs L10-L15) -/

/-- Label key field shift. -/
def bucketLabelShift : Nat := 36

/-- Overflow-log head byte shift. -/
def bucketLogShift : Nat := 52

/-- Reserved nibble shift. -/
def bucketReservedShift : Nat := 60

theorem labelShift_le_logShift : bucketLabelShift ≤ bucketLogShift := by
  unfold bucketLabelShift bucketLogShift
  omega

theorem logShift_lt_reservedShift : bucketLogShift < bucketReservedShift := by
  unfold bucketLogShift bucketReservedShift
  omega

/-- A `u16` wire key fits its 16-bit field. -/
def keyFits (key : Nat) : Prop := key < 2 ^ (bucketLogShift - bucketLabelShift)

/-- Any encodable overflow-log head byte is below `2^8` (a valid index `< 170`
or the `0xFF` sentinel). -/
theorem headByte_lt_256 {logHead : Int} {hb : Nat}
    (hhb : tryEncodeOverflowLogByte logHead = some hb) : hb < 2 ^ 8 := by
  by_cases h1 : logHead < 0
  · unfold tryEncodeOverflowLogByte at hhb
    rw [logHeadFromI32_eq_none_of_neg h1, map_some_option] at hhb
    have hinj : logNoneByte = hb := Option.some.inj hhb
    rw [← hinj]
    unfold logNoneByte
    omega
  · by_cases h2 : logHead < (maxLogEntries : Int)
    · have h := logHeadFromI32_eq_valid (by omega) h2
      unfold tryEncodeOverflowLogByte at hhb
      rw [h, map_some_option] at hhb
      have hinj : logHead.toNat = hb := Option.some.inj hhb
      have ht := toNat_lt_of_intLt_max (by omega) h2
      unfold maxLogEntries at ht
      rw [← hinj]
      omega
    · exfalso
      unfold tryEncodeOverflowLogByte at hhb
      rw [logHeadFromI32_rejects_overflow logHead (by omega)] at hhb
      simp at hhb

/-! ## Packing and decoding (labeled/slot_index.rs L19-L75) -/

/-- Fallible `try_encode_bucket_word`: slot bound plus LogHead encoding
(labeled/slot_index.rs L30-L44). The three packed regions are disjoint, so
Rust's bitwise-or is addition here (A-L4). -/
def tryEncodeBucketWord (edgeStart key : Nat) (logHead : Int) : Option Nat :=
  if edgeStart ≤ slotIndexMask then
    (tryEncodeOverflowLogByte logHead).map
      (fun hb => pack3 bucketLabelShift bucketLogShift edgeStart key hb)
  else none

/-- `decode_bucket_label_key` raw field (labeled/slot_index.rs L61-L63). -/
def decodeBucketWordKey (word : Nat) : Nat :=
  midBits bucketLabelShift (bucketLogShift - bucketLabelShift) word

/-- Overflow-log head byte field (labeled/slot_index.rs L67-L69, before
`decode_overflow_log_byte`). -/
def decodeBucketWordHeadByte (word : Nat) : Nat :=
  midBits bucketLogShift (bucketReservedShift - bucketLogShift) word

/-- `bucket_word_has_zero_reserved_bits` as a proposition: bits 60–63 are zero
(labeled/slot_index.rs L73-L75). -/
def bucketWordHasZeroReservedBits (word : Nat) : Prop :=
  word / 2 ^ bucketReservedShift % 16 = 0

/-! ## Properties -/

theorem pow_reservedSplit : 2 ^ bucketReservedShift
    = 2 ^ bucketLogShift * 2 ^ 8 := by
  unfold bucketReservedShift bucketLogShift
  rw [← Nat.pow_add]

/-- Encoding round-trips all three fields (P1-roundtrip). -/
theorem bucketWord_decode_of_tryEncode {edgeStart key logHead hb w : Nat}
    (hs : edgeStart ≤ slotIndexMask)
    (hk : keyFits key)
    (hhb : tryEncodeOverflowLogByte logHead = some hb)
    (hw : tryEncodeBucketWord edgeStart key logHead = some w) :
    decodeSlotIndex w = edgeStart ∧ decodeBucketWordKey w = key ∧
      decodeBucketWordHeadByte w = hb := by
  have hslt : edgeStart < 2 ^ slotIndexBits :=
    Nat.lt_of_le_of_lt hs slotIndexMask_lt_two_pow
  unfold tryEncodeBucketWord at hw
  rw [if_pos hs, hhb, map_some_option] at hw
  have hpk : pack3 bucketLabelShift bucketLogShift edgeStart key hb = w :=
    Option.some.inj hw
  subst hpk
  refine ⟨?_, ?_, ?_⟩
  · exact pack3_low labelShift_le_logShift hslt
  · exact pack3_mid labelShift_le_logShift hslt hk
  · have hlt := headByte_lt_256 hhb
    unfold decodeBucketWordHeadByte midBits
    rw [pack3_top labelShift_le_logShift hslt hk,
      Nat.mod_eq_of_lt (show hb < 2 ^ (bucketReservedShift - bucketLogShift) from hlt)]

/-- Injection of the three-field packing on valid domains (P2-injection). -/
theorem bucketWord_encode_injective {s1 k1 b1 s2 k2 b2 : Nat}
    (hs1 : s1 ≤ slotIndexMask) (hk1 : keyFits k1)
    (hs2 : s2 ≤ slotIndexMask) (hk2 : keyFits k2)
    (h : pack3 bucketLabelShift bucketLogShift s1 k1 b1
       = pack3 bucketLabelShift bucketLogShift s2 k2 b2) :
    s1 = s2 ∧ k1 = k2 ∧ b1 = b2 := by
  have h1 : s1 < 2 ^ slotIndexBits := Nat.lt_of_le_of_lt hs1 slotIndexMask_lt_two_pow
  have h2 : s2 < 2 ^ slotIndexBits := Nat.lt_of_le_of_lt hs2 slotIndexMask_lt_two_pow
  exact pack3_inj labelShift_le_logShift h1 h2 hk1 hk2 h

/-- Encoded words always have a zero reserved nibble (P4-extent): dividing out
bits `[0, 60)` lands exactly on the head byte, below `2^8`. -/
theorem encodeBucketWord_reserved_zero {edgeStart key logHead hb w : Nat}
    (hs : edgeStart ≤ slotIndexMask)
    (hk : keyFits key)
    (hhb : tryEncodeOverflowLogByte logHead = some hb)
    (hw : tryEncodeBucketWord edgeStart key logHead = some w) :
    bucketWordHasZeroReservedBits w := by
  unfold tryEncodeBucketWord at hw
  rw [if_pos hs, hhb, map_some_option] at hw
  have hpk : pack3 bucketLabelShift bucketLogShift edgeStart key hb = w :=
    Option.some.inj hw
  rw [← hpk]
  clear hw hpk
  have htop : (pack3 bucketLabelShift bucketLogShift edgeStart key hb)
      / 2 ^ bucketLogShift = hb :=
    pack3_top labelShift_le_logShift
      (Nat.lt_of_le_of_lt hs slotIndexMask_lt_two_pow) hk
  have hlt := headByte_lt_256 hhb
  obtain ⟨r, hr, heq⟩ :
      ∃ r, r < 2 ^ bucketLogShift ∧
        (pack3 bucketLabelShift bucketLogShift edgeStart key hb)
          = hb * 2 ^ bucketLogShift + r := by
    refine ⟨(pack3 bucketLabelShift bucketLogShift edgeStart key hb)
        % 2 ^ bucketLogShift,
      Nat.mod_lt _ (pow_pos bucketLogShift), ?_⟩
    have hd0 : 2 ^ bucketLogShift
        * ((pack3 bucketLabelShift bucketLogShift edgeStart key hb)
          / 2 ^ bucketLogShift)
        + (pack3 bucketLabelShift bucketLogShift edgeStart key hb)
          % 2 ^ bucketLogShift
        = (pack3 bucketLabelShift bucketLogShift edgeStart key hb) :=
      Nat.div_add_mod _ _
    rw [htop] at hd0
    rw [Nat.mul_comm hb (2 ^ bucketLogShift)]
    exact hd0.symm
  unfold bucketWordHasZeroReservedBits
  rw [heq, Nat.mul_comm hb (2 ^ bucketLogShift)]
  rw [pow_reservedSplit, ← Nat.div_div_eq_div_mul,
    Nat.mul_add_div (pow_pos bucketLogShift), Nat.div_eq_of_lt hr, Nat.add_zero,
    Nat.div_eq_of_lt hlt]

end Lar
