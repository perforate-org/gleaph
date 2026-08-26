/- LabelBucket wire-record validation: transcription of the fail-closed
decoder checks of `src/labeled/record.rs` (L382-L459). Little-endian chunk
extraction is trusted per assumption A-L3, so the decoder takes the
already-extracted fixed-width fields. Core Lean only.
-/
import Lar.BucketWord

namespace Lar

/-! ## Record image (labeled/record.rs L28-L45) -/

/-- Wire image of one 29-byte LabelBucket record after LE field extraction.
Field ranges (`u64/u32/u40/u16/u8`) are carried as hypotheses where they
matter; the validation below only relies on the byte-range of the two log
fields and the packed word. -/
structure LabelBucketImage where
  /-- Packed bucket word (slot | label key | log head byte | reserved). -/
  word : Nat
  /-- Logical live edge count for this label bucket (`u32`). -/
  degree : Nat
  /-- Stored edge-slab width (`u32`). -/
  storedSlots : Nat
  /-- Stored inline-property-bytes slab slots (`u32`). -/
  inlineSlabSlots : Nat
  /-- Byte offset into the inline-property-bytes store (`u40`). -/
  inlineOffset : Nat
  /-- Physical byte width per inline property slot (`0` = no values, `u16`). -/
  inlineWidth : Nat
  /-- Per-bucket inline-property-bytes overflow log head byte (`u8`). -/
  inlineLogByte : Nat
  /-- Entries in the ordered inline-property-bytes-log suffix (`u8`). -/
  inlineLogLen : Nat

/-! ## Error taxonomy (labeled/record.rs L442-L459) -/

/-- Invalid LabelBucket wire or field combinations. -/
inductive LabelBucketFieldError where
  | /-- Bits 60–63 of the packed word are reserved and must be zero. -/
    reservedBitsSet
  | /-- `edge_start` does not fit in the 36-bit slot index (constructor side). -/
    slotIndexOverflow
  | /-- Overflow log head byte is not `0xFF` and not in `0..170`. -/
    overflowLogHeadOutOfRange
  | /-- Inline-property-bytes offset does not fit in the 40-bit space. -/
    inlinePropertyBytesOffsetOverflow
  | /-- Value overflow log head byte out of range. -/
    inlinePropertyBytesLogHeadOutOfRange
  | /-- Value overflow log length byte out of range. -/
    inlinePropertyBytesLogLenOutOfRange
  | /-- Value overflow log head and length disagree. -/
    inlinePropertyBytesLogStateMismatch
  | /-- Inline-property-bytes state requires a non-zero schema width. -/
    inlinePropertyBytesStateWithoutSchema

/-! ## Fail-closed decoder (labeled/record.rs L388-L437) -/

open Classical in
/-- `LabelBucket::try_read_from` after LE extraction: rejects, in order,
reserved nibble set, out-of-range head bytes, out-of-range offsets, log
length overflow, log head/len disagreement, and value state without schema. -/
noncomputable
def tryReadFromFields (f : LabelBucketImage) :
    Except LabelBucketFieldError LabelBucketImage :=
  if ¬bucketWordHasZeroReservedBits f.word then .error .reservedBitsSet
  else if decodeBucketWordHeadByte f.word ≠ logNoneByte
      ∧ maxLogEntries ≤ decodeBucketWordHeadByte f.word then
    .error .overflowLogHeadOutOfRange
  else if ¬byteOffsetFits f.inlineOffset then
    .error .inlinePropertyBytesOffsetOverflow
  else if f.inlineLogByte ≠ logNoneByte ∧ maxLogEntries ≤ f.inlineLogByte then
    .error .inlinePropertyBytesLogHeadOutOfRange
  else if maxLogEntries < f.inlineLogLen then
    .error .inlinePropertyBytesLogLenOutOfRange
  else if (f.inlineLogByte = logNoneByte) ≠ (f.inlineLogLen = 0) then
    .error .inlinePropertyBytesLogStateMismatch
  else if f.inlineWidth = 0 ∧ (f.inlineSlabSlots ≠ 0 ∨ f.inlineLogLen ≠ 0) then
    .error .inlinePropertyBytesStateWithoutSchema
  else .ok f

/-- Wire validity: exactly the conjunction of the decoder's acceptance
conditions, in source order (P6-fail-closed-validation). -/
def LabelBucketWireValid (f : LabelBucketImage) : Prop :=
  bucketWordHasZeroReservedBits f.word
  ∧ ¬(decodeBucketWordHeadByte f.word ≠ logNoneByte
      ∧ maxLogEntries ≤ decodeBucketWordHeadByte f.word)
  ∧ byteOffsetFits f.inlineOffset
  ∧ ¬(f.inlineLogByte ≠ logNoneByte ∧ maxLogEntries ≤ f.inlineLogByte)
  ∧ ¬(maxLogEntries < f.inlineLogLen)
  ∧ ¬((f.inlineLogByte = logNoneByte) ≠ (f.inlineLogLen = 0))
  ∧ ¬(f.inlineWidth = 0 ∧ (f.inlineSlabSlots ≠ 0 ∨ f.inlineLogLen ≠ 0))

/-- Sufficiency: every wire-valid image decodes successfully and reproduces
itself (scan correctness at the record boundary). -/
theorem tryReadFromFields_ok_of_wireValid {f : LabelBucketImage}
    (hv : LabelBucketWireValid f) : tryReadFromFields f = Except.ok f := by
  obtain ⟨h1, h2, h3, h4, h5, h6, h7⟩ := hv
  unfold tryReadFromFields
  rw [if_neg (fun hc => hc h1), if_neg h2, if_neg (fun hc => hc h3),
    if_neg h4, if_neg h5, if_neg h6, if_neg h7]

/-- Necessity: an accepted image satisfies every acceptance condition, so
validation is complete — no malformed image passes (storage safety). -/
theorem wireValid_of_tryReadFromFields_ok {f : LabelBucketImage}
    (hok : tryReadFromFields f = Except.ok f) : LabelBucketWireValid f := by
  unfold tryReadFromFields at hok
  by_cases hc1 : ¬bucketWordHasZeroReservedBits f.word
  · rw [if_pos hc1] at hok; simp at hok
  · rw [if_neg hc1] at hok
    by_cases hc2 : decodeBucketWordHeadByte f.word ≠ logNoneByte
        ∧ maxLogEntries ≤ decodeBucketWordHeadByte f.word
    · rw [if_pos hc2] at hok; simp at hok
    · rw [if_neg hc2] at hok
      by_cases hc3 : ¬byteOffsetFits f.inlineOffset
      · rw [if_pos hc3] at hok; simp at hok
      · rw [if_neg hc3] at hok
        by_cases hc4 : f.inlineLogByte ≠ logNoneByte
            ∧ maxLogEntries ≤ f.inlineLogByte
        · rw [if_pos hc4] at hok; simp at hok
        · rw [if_neg hc4] at hok
          by_cases hc5 : maxLogEntries < f.inlineLogLen
          · rw [if_pos hc5] at hok; simp at hok
          · rw [if_neg hc5] at hok
            by_cases hc6 : (f.inlineLogByte = logNoneByte)
                ≠ (f.inlineLogLen = 0)
            · rw [if_pos hc6] at hok; simp at hok
            · rw [if_neg hc6] at hok
              by_cases hc7 : f.inlineWidth = 0
                  ∧ (f.inlineSlabSlots ≠ 0 ∨ f.inlineLogLen ≠ 0)
              · rw [if_pos hc7] at hok; simp at hok
              · rw [if_neg hc7] at hok
                exact ⟨Classical.byContradiction hc1, hc2,
                  Classical.byContradiction hc3, hc4, hc5, hc6, hc7⟩

end Lar
