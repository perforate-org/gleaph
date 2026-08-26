/- LogHead codec: transcription of `src/log_head.rs` and the overflow-log
byte helpers of `src/slab_index.rs` (L204-L242). Legacy `i32` API values are
modeled as `Int`, wire bytes as `Nat` (callers supply the `≤ 255` u8-range
hypothesis). Core Lean only.
-/
import Lar.Basic

namespace Lar

/-! ## Constants -/

/-- `DEFAULT_MAX_LOG_ENTRIES` (`lara/edge/log.rs` L59; identical definition in
`lara/edge_inline_property/log.rs` L29). -/
def maxLogEntries : Nat := 170

/-- Wire sentinel `LogHead::NONE` = `u8::MAX` (log_head.rs L11-L12,
slab_index.rs L18). -/
def logNoneByte : Nat := 255

/-! ## LogHead model (log_head.rs L7-L72) -/

/-- Abstract per-segment overflow-log head: no chain, or an entry index
(log_head.rs L6-L15). NOTE: mirroring Rust `from_byte`, the constructor does
not itself enforce the documented `[0, 170)` domain; enforcement lives at
record-validation time (labeled/record.rs L396-L399, L407-L412). -/
inductive LogHead where
  | none
  | valid (idx : Nat)

/-- `LogHead::from_index` (log_head.rs L19-L25). -/
def logHeadFromIndex (idx : Nat) : Option LogHead :=
  if idx < maxLogEntries then some (.valid idx) else none

/-- `LogHead::from_i32` (log_head.rs L29-L37). Any negative legacy value maps
to `none`; `0..170` maps to the index; `≥ 170` is rejected. -/
def logHeadFromI32 (head : Int) : Option LogHead :=
  if head < 0 then some .none
  else if head < (maxLogEntries : Int) then some (.valid head.toNat)
  else none

/-- `LogHead::to_i32` (log_head.rs L41-L43). -/
def logHeadToI32 : LogHead → Int
  | .none => -1
  | .valid idx => (idx : Int)

/-- `LogHead::as_byte` wire image (log_head.rs L47-L49). -/
def logHeadByte : LogHead → Nat
  | .none => logNoneByte
  | .valid idx => idx

/-- `LogHead::from_byte` (log_head.rs L65-L71). -/
def logHeadOfByte (byte : Nat) : LogHead :=
  if byte = logNoneByte then .none else .valid byte

theorem map_some_option {α β : Type} (f : α → β) (a : α) :
    (some a : Option α).map f = some (f a) := rfl

/-- Valid entry indices never collide with the NONE sentinel. -/
theorem validIdx_ne_logNone {idx : Nat} (hb : idx < maxLogEntries) :
    ¬ (idx = logNoneByte) := by
  show idx ≠ 255
  unfold maxLogEntries at hb
  omega

/-! ## Legacy-i32 interface properties -/

/-- Negative legacy values decode back to `-1` (sentinel canonicalization,
log_head.rs L30-L31). -/
theorem logHeadFromI32_negative (head : Int) (hneg : head < 0) :
    (logHeadFromI32 head).map logHeadToI32 = some (-1 : Int) := by
  unfold logHeadFromI32
  rw [if_pos hneg]
  rfl

/-- Valid indices `0..170` round-trip through the legacy `i32` view. -/
theorem logHeadFromI32_roundtrip {head : Int} (hnonneg : 0 ≤ head)
    (hbound : head < (maxLogEntries : Int)) :
    (logHeadFromI32 head).map logHeadToI32 = some head := by
  unfold logHeadFromI32
  rw [if_neg (show ¬ head < 0 from by omega), if_pos hbound]
  have hcast : ((head.toNat : Nat) : Int) = head := Int.toNat_of_nonneg hnonneg
  have hstep : (some (LogHead.valid head.toNat)).map logHeadToI32
      = some ((head.toNat : Nat) : Int) := rfl
  rw [hstep, hcast]

/-- Legacy values at or above `maxLogEntries` are rejected fail-closed
(log_head.rs L34-L35). -/
theorem logHeadFromI32_rejects_overflow (head : Int)
    (hover : (maxLogEntries : Int) ≤ head) : logHeadFromI32 head = none := by
  unfold logHeadFromI32
  rw [if_neg (show ¬ head < 0 from by omega),
    if_neg (show ¬ head < (maxLogEntries : Int) from by omega)]

/-! ## Wire-byte layer (slab_index.rs L204-L242) -/

/-- `try_encode_overflow_log_byte` (slab_index.rs L218-L220). -/
def tryEncodeOverflowLogByte (head : Int) : Option Nat :=
  (logHeadFromI32 head).map logHeadByte

/-- `decode_overflow_log_byte` (slab_index.rs L234-L236). -/
def decodeOverflowLogByte (byte : Nat) : Int :=
  logHeadToI32 (logHeadOfByte byte)

theorem logHeadByte_none : logHeadByte LogHead.none = logNoneByte := rfl

theorem decode_of_logNoneByte : decodeOverflowLogByte logNoneByte = -1 := rfl

theorem decode_of_valid {i : Nat} (hne : ¬ (i = logNoneByte)) :
    decodeOverflowLogByte i = ((i : Nat) : Int) := by
  unfold decodeOverflowLogByte logHeadOfByte
  rw [if_neg hne]
  rfl

theorem logHeadFromI32_eq_some_none {head : Int} (hneg : head < 0) :
    logHeadFromI32 head = some LogHead.none := by
  unfold logHeadFromI32
  rw [if_pos hneg]

theorem logHeadFromI32_eq_some_valid {head : Int} (h0 : 0 ≤ head)
    (hb : head < (maxLogEntries : Int)) :
    logHeadFromI32 head = some (LogHead.valid head.toNat) := by
  unfold logHeadFromI32
  rw [if_neg (show ¬ head < 0 from by omega), if_pos hb]

/-- Byte round-trip is the identity on the canonical head domain
(`from_byte ∘ as_byte = id` for NONE and every encodable index;
log_head.rs L47-L71). SUSPICION recorded in REPORT.md F6: the round trip
breaks exactly at `valid 255`, unreachable through constructors used by
validated records but reachable through raw `from_byte`. -/
theorem logHeadOfByte_of_logHeadByte_canonical (x : LogHead)
    (hx : ∀ i, x = LogHead.valid i → i < maxLogEntries) :
    logHeadOfByte (logHeadByte x) = x := by
  cases x with
  | none => rfl
  | valid idx =>
    have hb : idx < maxLogEntries := hx idx rfl
    simp [logHeadOfByte, logHeadByte, validIdx_ne_logNone hb]

/-- Full legacy round trip `i32 → wire byte → i32` canonicalizes negatives to
`-1` and is the identity on `0..170` (P5-sentinel-faithfulness,
slab_index.rs L218-L236). -/
theorem decode_of_tryEncode (head : Int)
    (hbound : head < (maxLogEntries : Int)) :
    (tryEncodeOverflowLogByte head).map decodeOverflowLogByte
      = some (if head < 0 then (-1 : Int) else head) := by
  rcases Int.lt_trichotomy head 0 with hneg | hzero | hpos
  · unfold tryEncodeOverflowLogByte
    rw [logHeadFromI32_eq_some_none hneg, map_some_option, map_some_option,
      logHeadByte_none, decode_of_logNoneByte, if_pos hneg]
  · subst hzero
    unfold tryEncodeOverflowLogByte
    rw [logHeadFromI32_eq_some_valid (by omega) (by omega), map_some_option,
      map_some_option]
    have hb0 : ((0 : Int).toNat : Nat) < maxLogEntries := by
      have hz : ((0 : Int).toNat : Nat) = 0 := by simp
      rw [hz]
      unfold maxLogEntries
      omega
    show some (decodeOverflowLogByte ((0 : Int).toNat)) = _
    rw [decode_of_valid (validIdx_ne_logNone hb0),
      Int.toNat_of_nonneg (by omega),
      if_neg (show ¬ ((0 : Int) < 0) from by omega)]
  · unfold tryEncodeOverflowLogByte
    rw [logHeadFromI32_eq_some_valid (by omega) hbound, map_some_option,
      map_some_option]
    have hcast : ((head.toNat : Nat) : Int) = head := Int.toNat_of_nonneg (by omega)
    have hne : ¬ (head.toNat = logNoneByte) := by
      intro heq
      have h255 : ((head.toNat : Nat) : Int) = 255 := by
        rw [heq]
        rfl
      rw [hcast] at h255
      unfold maxLogEntries at hbound
      omega
    show some (decodeOverflowLogByte head.toNat) = _
    rw [decode_of_valid hne, hcast, if_neg (show ¬ head < 0 from by omega)]

/-- Encoding fails exactly when the legacy value lies outside `[−∞, 170)`
(fail-closed bound, slab_index.rs L218-L220 + log_head.rs L29-L37). -/
theorem tryEncodeOverflowLogByte_none_iff (head : Int) :
    tryEncodeOverflowLogByte head = none ↔ (maxLogEntries : Int) ≤ head := by
  unfold tryEncodeOverflowLogByte logHeadFromI32
  simp only [Option.map_eq_none_iff]
  constructor
  · intro h
    by_cases hn : head < 0
    · rw [if_pos hn] at h; simp at h
    · rw [if_neg hn] at h
      by_cases hl : head < (maxLogEntries : Int)
      · rw [if_pos hl] at h; simp at h
      · rw [if_neg hl] at h; omega
  · intro hover
    rw [if_neg (show ¬ head < 0 from by omega),
      if_neg (show ¬ head < (maxLogEntries : Int) from by omega)]

theorem logHeadFromI32_eq_none_of_neg {head : Int} (hneg : head < 0) :
    logHeadFromI32 head = some LogHead.none := by
  unfold logHeadFromI32
  rw [if_pos hneg]

theorem logHeadFromI32_eq_valid {head : Int} (h0 : 0 ≤ head)
    (hb : head < (maxLogEntries : Int)) :
    logHeadFromI32 head = some (LogHead.valid head.toNat) := by
  unfold logHeadFromI32
  rw [if_neg (show ¬ head < 0 from by omega), if_pos hb]

/-- Transfer a legacy-domain bound to the encoded entry index.
Requires non-negativity so the truncating `toNat` is the identity. -/
theorem toNat_lt_of_intLt_max {head : Int} (h0 : 0 ≤ head)
    (hb : head < (maxLogEntries : Int)) :
    head.toNat < maxLogEntries := by
  have hcast : ((head.toNat : Nat) : Int) = head := Int.toNat_of_nonneg h0
  have hi : ((head.toNat : Nat) : Int) < 170 := by
    rw [hcast]
    exact hb
  unfold maxLogEntries
  omega

end Lar
