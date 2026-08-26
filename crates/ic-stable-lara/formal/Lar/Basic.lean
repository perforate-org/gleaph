/- Arithmetic foundations for the LARA Stage-1 model: bit-field packing over
natural numbers. Wire words (`u64`) are modeled as `Nat` with range
hypotheses; every packed field transcribed in this project lies strictly below
`2^63`, so no wrap can occur and bitwise-or over disjoint field regions equals
addition (assumption A-L4 in SCOPE.md). Core Lean only, no Mathlib.
-/
namespace Lar

theorem two_pos : 0 < 2 := by omega

theorem pow_pos (bits : Nat) : 0 < 2 ^ bits := by
  induction bits with
  | zero => rw [Nat.pow_zero]; exact Nat.zero_lt_one
  | succ n ih => rw [Nat.pow_succ]; exact Nat.mul_pos ih two_pos

theorem lt_two_pow {bits x : Nat} (h : x < 2 ^ bits) : x % 2 ^ bits = x :=
  Nat.mod_eq_of_lt h

/-- Field at bit positions `[shift, shift + bits)` of a word. -/
def midBits (shift bits x : Nat) : Nat := (x / 2 ^ shift) % 2 ^ bits

/-! ## Packing one low field and one high field -/

/-- Pack a low field (below `2^shift`) and a high field into one word.
Mirrors Rust `lo | (hi << shift)` for disjoint regions (A-L4). -/
def pack2 (shift lo hi : Nat) : Nat := lo + hi * 2 ^ shift

theorem pack2_low {shift lo hi : Nat} (hlo : lo < 2 ^ shift) :
    (pack2 shift lo hi) % 2 ^ shift = lo := by
  unfold pack2
  rw [Nat.mul_comm hi (2 ^ shift),
    Nat.add_mul_mod_self_left lo (2 ^ shift) hi, lt_two_pow hlo]

theorem pack2_high {shift lo hi : Nat} (hlo : lo < 2 ^ shift) :
    (pack2 shift lo hi) / 2 ^ shift = hi := by
  unfold pack2
  rw [Nat.mul_comm hi (2 ^ shift),
    Nat.add_mul_div_left lo hi (pow_pos shift), Nat.div_eq_of_lt hlo, Nat.zero_add]

/-- Equal low fields out of an equal packed pair. -/
theorem eq_lo_of_pack2_eq {shift lo1 hi1 lo2 hi2 : Nat}
    (hlo1 : lo1 < 2 ^ shift) (hlo2 : lo2 < 2 ^ shift)
    (h : pack2 shift lo1 hi1 = pack2 shift lo2 hi2) : lo1 = lo2 := by
  have s1 : lo1 = (pack2 shift lo1 hi1) % 2 ^ shift := (pack2_low hlo1).symm
  rw [h] at s1
  rw [s1, pack2_low hlo2]

/-- Packing is injective: the split into `(low field, high field)` is unique. -/
theorem pack2_inj {shift lo1 hi1 lo2 hi2 : Nat} (hlo1 : lo1 < 2 ^ shift)
    (hlo2 : lo2 < 2 ^ shift) (h : pack2 shift lo1 hi1 = pack2 shift lo2 hi2) :
    lo1 = lo2 ∧ hi1 = hi2 := by
  have hlow := eq_lo_of_pack2_eq hlo1 hlo2 h
  refine ⟨hlow, ?_⟩
  unfold pack2 at h
  rw [hlow] at h
  have hc : hi1 * 2 ^ shift + lo2 = hi2 * 2 ^ shift + lo2 := by
    rw [Nat.add_comm (hi1 * 2 ^ shift) lo2, Nat.add_comm (hi2 * 2 ^ shift) lo2]
    exact h
  exact Nat.mul_right_cancel (pow_pos shift) (Nat.add_right_cancel hc)

/-! ## Replacing the low field of a word, preserving high bits -/

/-- Clear the low `shift` bits of `word` and install `lo`.
Mirrors Rust `(word & !MASK) | lo` (A-L4); callers guarantee `lo` fits. -/
def replaceLow (shift word lo : Nat) : Nat := word / 2 ^ shift * 2 ^ shift + lo

theorem replaceLow_high {shift word lo : Nat} (hlo : lo < 2 ^ shift) :
    (replaceLow shift word lo) / 2 ^ shift = word / 2 ^ shift := by
  unfold replaceLow
  rw [Nat.mul_comm (word / 2 ^ shift) (2 ^ shift)]
  rw [Nat.mul_add_div (pow_pos shift) (word / 2 ^ shift) lo,
    Nat.div_eq_of_lt hlo, Nat.add_zero]

theorem replaceLow_low {shift word lo : Nat} (hlo : lo < 2 ^ shift) :
    (replaceLow shift word lo) % 2 ^ shift = lo := by
  unfold replaceLow
  rw [Nat.mul_comm (word / 2 ^ shift) (2 ^ shift)]
  rw [Nat.mul_add_mod (2 ^ shift) (word / 2 ^ shift) lo, lt_two_pow hlo]

/-- Replacing the low field preserves any higher field `[shift, shift + bits)`. -/
theorem replaceLow_mid {shift bits word lo : Nat} (hlo : lo < 2 ^ shift) :
    midBits shift bits (replaceLow shift word lo) = midBits shift bits word := by
  unfold midBits
  rw [replaceLow_high hlo]

/-! ## Three-field packing (slot | meta | top regions of a u64 word) -/

/-- Pack a bottom field `[0, shift)`, a middle field `[shift, midShift)`,
and a top field from `midShift` up. All regions disjoint (A-L4). -/
def pack3 (shift midShift lo mid hi : Nat) : Nat :=
  pack2 shift lo (mid + hi * 2 ^ (midShift - shift))

theorem pack3_low {shift midShift lo mid hi : Nat}
    (_hs : shift ≤ midShift) (hlo : lo < 2 ^ shift) :
    (pack3 shift midShift lo mid hi) % 2 ^ shift = lo :=
  pack2_low hlo

theorem pack3_mid {shift midShift lo mid hi : Nat}
    (_hs : shift ≤ midShift) (hlo : lo < 2 ^ shift)
    (hmid : mid < 2 ^ (midShift - shift)) :
    midBits shift (midShift - shift) (pack3 shift midShift lo mid hi) = mid := by
  unfold pack3 midBits
  rw [pack2_high hlo, Nat.mul_comm hi (2 ^ (midShift - shift)),
    Nat.add_mul_mod_self_left mid (2 ^ (midShift - shift)) hi, lt_two_pow hmid]

theorem pack3_top {shift midShift lo mid hi : Nat}
    (_hs : shift ≤ midShift) (hlo : lo < 2 ^ shift)
    (hmid : mid < 2 ^ (midShift - shift)) :
    (pack3 shift midShift lo mid hi) / 2 ^ midShift = hi := by
  have hK : 0 < 2 ^ (midShift - shift) := pow_pos _
  have hdec : 2 ^ midShift = 2 ^ shift * 2 ^ (midShift - shift) := by
    rw [← Nat.pow_add]
    congr 1
    omega
  unfold pack3
  rw [hdec, ← Nat.div_div_eq_div_mul, pack2_high hlo,
    Nat.mul_comm hi (2 ^ (midShift - shift)),
    Nat.add_mul_div_left mid hi hK, Nat.div_eq_of_lt hmid, Nat.zero_add]

/-- Three-field packing is injective on its field ranges. -/
theorem pack3_inj {shift midShift lo1 mid1 hi1 lo2 mid2 hi2 : Nat}
    (_hs : shift ≤ midShift) (hlo1 : lo1 < 2 ^ shift) (hlo2 : lo2 < 2 ^ shift)
    (hmid1 : mid1 < 2 ^ (midShift - shift)) (hmid2 : mid2 < 2 ^ (midShift - shift))
    (h : pack3 shift midShift lo1 mid1 hi1 = pack3 shift midShift lo2 mid2 hi2) :
    lo1 = lo2 ∧ mid1 = mid2 ∧ hi1 = hi2 := by
  have hK : 0 < 2 ^ (midShift - shift) := pow_pos _
  -- view both sides as two-field packs over the same radixes
  have hp : pack2 shift lo1 (mid1 + hi1 * 2 ^ (midShift - shift))
      = pack2 shift lo2 (mid2 + hi2 * 2 ^ (midShift - shift)) :=
    show pack3 shift midShift lo1 mid1 hi1
      = pack3 shift midShift lo2 mid2 hi2 from h
  have hlow := eq_lo_of_pack2_eq hlo1 hlo2 hp
  rw [hlow] at hp
  -- with equal low fields, the remaining pair must match as a whole
  have hmids :=
    pack2_inj (shift := shift) (lo1 := lo2) (lo2 := lo2)
      (hi1 := mid1 + hi1 * 2 ^ (midShift - shift))
      (hi2 := mid2 + hi2 * 2 ^ (midShift - shift)) hlo2 hlo2 hp
  -- extract the middle field of each remaining pack
  have m1 : midBits shift (midShift - shift)
        (pack2 shift lo2 (mid1 + hi1 * 2 ^ (midShift - shift))) = mid1 := by
    unfold midBits
    rw [pack2_high hlo2, Nat.mul_comm hi1 (2 ^ (midShift - shift)),
      Nat.add_mul_mod_self_left mid1 (2 ^ (midShift - shift)) hi1, lt_two_pow hmid1]
  have m2 : midBits shift (midShift - shift)
        (pack2 shift lo2 (mid2 + hi2 * 2 ^ (midShift - shift))) = mid2 := by
    unfold midBits
    rw [pack2_high hlo2, Nat.mul_comm hi2 (2 ^ (midShift - shift)),
      Nat.add_mul_mod_self_left mid2 (2 ^ (midShift - shift)) hi2, lt_two_pow hmid2]
  have midEq : mid1 = mid2 := by
    rw [← m1, ← m2, hmids.2]
  refine ⟨hlow, midEq, ?_⟩
  rw [midEq] at hmids
  -- hmids.2 : mid2 + hi1 * 2^K = mid2 + hi2 * 2^K
  have htop : hi1 * 2 ^ (midShift - shift) + mid2
      = hi2 * 2 ^ (midShift - shift) + mid2 := by
    rw [Nat.add_comm (hi1 * 2 ^ (midShift - shift)) mid2,
      Nat.add_comm (hi2 * 2 ^ (midShift - shift)) mid2]
    exact hmids.2
  exact Nat.mul_right_cancel hK (Nat.add_right_cancel htop)

theorem two_pow_succ_lt (n : Nat) : 2 ^ n < 2 ^ (n + 1) := by
  rw [Nat.pow_succ]
  have hp := pow_pos n
  omega

theorem pow_lt_two_pow {m n : Nat} (h : m < n) : 2 ^ m < 2 ^ n := by
  induction n with
  | zero => omega
  | succ k ih =>
    rcases Nat.lt_or_ge m k with hlt | hge
    · exact Nat.lt_trans (ih (by omega)) (two_pow_succ_lt k)
    · have hmk : m = k := by omega
      rw [hmk]
      exact two_pow_succ_lt k

end Lar
