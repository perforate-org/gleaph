/- BucketLabelKey wire semantics: transcription of
`src/labeled/bucket_label_key.rs` (L11-L115). The raw `u16` wire value is a
`Nat`; callers supply the u16-range hypothesis where it matters. Core Lean.
-/
import Lar.SlotIndex

namespace Lar

/-! ## Constants (bucket_label_key.rs L11-L14) -/

/-- MSB: directed bucket / default directed bypass. -/
def bucketLabelDirectedBit : Nat := 0x8000

/-- Mask for the low 15 label-index bits. -/
def bucketLabelIndexMask : Nat := 0x7FFF

theorem directedBit_eq : bucketLabelDirectedBit = 2 ^ 15 := rfl

theorem two_pow_15_val : (2 : Nat) ^ 15 = 32768 := rfl

theorem indexMask_lt : bucketLabelIndexMask < 2 ^ 15 := by
  have h := two_pow_15_val
  unfold bucketLabelIndexMask
  omega

/-! ## Constructors and accessors (bucket_label_key.rs L58-L93) -/

/-- `directed_from_index`: `(index & 0x7FFF) | 0x8000`, modeled as addition on
disjoint regions (A-L4). -/
def keyDirectedFromIndex (idx : Nat) : Nat := (idx % 2 ^ 15) + 2 ^ 15

/-- `undirected_from_index`: `index & 0x7FFF`. -/
def keyUndirectedFromIndex (idx : Nat) : Nat := idx % 2 ^ 15

/-- `label_index`: the low 15 bits. -/
def keyLabelIndex (key : Nat) : Nat := key % 2 ^ 15

/-- `is_directed`: the MSB is set (`u16` values, so this is `≥ 2^15`). -/
def keyIsDirected (key : Nat) : Prop := bucketLabelDirectedBit ≤ key

/-! ## Properties -/

/-- Directed constructor round-trips through the index accessor
(bucket_label_key.rs L77-L81 + L58-L60). -/
theorem keyLabelIndex_of_directed (idx : Nat) :
    keyLabelIndex (keyDirectedFromIndex idx) = idx % 2 ^ 15 := by
  have h15 : (2 : Nat) ^ 15 = 32768 := rfl
  unfold keyLabelIndex keyDirectedFromIndex
  rw [h15]
  omega

/-- Undirected constructor round-trips through the index accessor. -/
theorem keyLabelIndex_of_undirected (idx : Nat) :
    keyLabelIndex (keyUndirectedFromIndex idx) = idx % 2 ^ 15 :=
  lt_two_pow (Nat.mod_lt _ (pow_pos 15))

/-- Directed keys are exactly the ones with the MSB set, i.e. `≥ 0x8000`
(bucket_label_key.rs L85-L93). -/
theorem keyIsDirected_of_directed (idx : Nat) :
    keyIsDirected (keyDirectedFromIndex idx) := by
  unfold keyIsDirected keyDirectedFromIndex bucketLabelDirectedBit
  omega

/-- Undirected constructor output never carries the MSB
(bucket_label_key.rs L85-L87). -/
theorem not_keyIsDirected_of_undirected (idx : Nat) :
    ¬ keyIsDirected (keyUndirectedFromIndex idx) := by
  have hlt := Nat.mod_lt idx (pow_pos 15)
  unfold keyIsDirected keyUndirectedFromIndex bucketLabelDirectedBit
  omega

/-- Ordering contract of the derived `Ord` (raw `u16` order): every undirected
key sorts before every directed key at full index rank
(bucket_label_key.rs L5-L6, L148-L152). -/
theorem undirected_lt_directed {u d : Nat} (hu : u < bucketLabelDirectedBit)
    (hd : bucketLabelDirectedBit ≤ d) : u < d := by
  have := directedBit_eq ▸ hd
  omega

/-- Directed construction is injective up to the masked index:
equal wires imply equal canonical indices (P2-injection). -/
theorem keyDirected_injective {i j : Nat}
    (h : keyDirectedFromIndex i = keyDirectedFromIndex j) :
    i % 2 ^ 15 = j % 2 ^ 15 := by
  unfold keyDirectedFromIndex at h
  have hpos := pow_pos 15
  omega

end Lar
