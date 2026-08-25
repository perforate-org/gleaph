/-
Routing mathematics of the stable linear hash map.

Every definition mirrors a specific Rust function; the citation comment gives the file,
the function name, and the line range at audit target revision `0da342d62`.

Verified properties (SCOPE.md "Properties verified"):
- P1 `route_lt_base_plus_cursor`: routing never escapes `base + cursor`.
- P2 `split_stability_level_up` / `split_stability_cursor_adv`: the standard split moves
  an entry to its old bucket or to `old + base`.
- P3 `next_geometry_shape`: geometry steps preserve the cursor/bucket-count shape or fail
  closed.
- P5 `split_threshold_*`: threshold monotonicity and capacity bounds.
-/
import Lhm.Basic

namespace Lhm

/-! ## Constants (header.rs L15-L18, map.rs L15-L16) -/

/-- header.rs L15: `PRIMARY_SLOTS` -/
def PrimarySlots : Nat := 8

/-- header.rs L16-L17: `OVERFLOW_PAGE_COUNT`, `PAGES_PER_BUCKET = 1 + OVERFLOW_PAGE_COUNT` -/
def PagesPerBucket : Nat := 1 + 2

/-- header.rs L18: `SLOTS_PER_BUCKET = PRIMARY_SLOTS * PAGES_PER_BUCKET` -/
def SlotsPerBucket : Nat := PrimarySlots * PagesPerBucket

/-- map.rs L15: `INITIAL_LEVEL` -/
def InitialLevel : Nat := 3

/-! ## Bucket routing (map.rs L1963-L1971 `linear_bucket`) -/

/-- Low `level` bits of the hash: `hash & ((1 << level) - 1)`. -/
def lowBits (hash level : Nat) : Nat := hash % 2 ^ level

/-- Low `level + 1` bits: `hash & ((mask << 1) | 1)`. -/
def wideBits (hash level : Nat) : Nat := hash % 2 ^ (level + 1)

/-- Faithful transcription of map.rs L1963-L1971 (the local `let b` is inlined).
Requires `level < 64` for the Rust shift to be well-defined; callers obtain that from
`ValidControl` (see Control.lean). -/
def linearBucket (hash level cursor : Nat) : Nat :=
  if lowBits hash level < cursor then wideBits hash level else lowBits hash level

theorem linearBucket_at_zero (hash level : Nat) :
    linearBucket hash level 0 = lowBits hash level := by
  unfold linearBucket
  exact if_neg (by omega)

theorem linearBucket_eq_wide (hash level cursor : Nat)
    (hlt : lowBits hash level < cursor) :
    linearBucket hash level cursor = wideBits hash level := by
  unfold linearBucket
  exact if_pos hlt

theorem linearBucket_eq_low (hash level cursor : Nat)
    (hge : ¬ (lowBits hash level < cursor)) :
    linearBucket hash level cursor = lowBits hash level := by
  unfold linearBucket
  exact if_neg hge

theorem wideBits_mod (hash level : Nat) :
    wideBits hash level % 2 ^ level = lowBits hash level :=
  mod_mod_two_pow_succ hash level

/-- When the wide residue reaches into bit `level`, the low residue is the wide residue
minus the old base. -/
theorem lowBits_eq_sub_wide (hash level : Nat) (hge : 2 ^ level ≤ wideBits hash level) :
    lowBits hash level = wideBits hash level - 2 ^ level := by
  have hlt : wideBits hash level < 2 ^ (level + 1) := lt_mod_two_pow hash (level + 1)
  have hp1 : 2 ^ (level + 1) = 2 * 2 ^ level := two_pow_succ_eq level
  have hsub : wideBits hash level - 2 ^ level < 2 ^ level := by omega
  have key : wideBits hash level % 2 ^ level = wideBits hash level - 2 ^ level := by
    have hsplit :
        wideBits hash level = (wideBits hash level - 2 ^ level) + 2 ^ level * 1 := by omega
    conv => lhs; rw [hsplit]
    rw [Nat.add_mul_mod_self_left, Nat.mod_eq_of_lt hsub]
  calc lowBits hash level = wideBits hash level % 2 ^ level :=
      (wideBits_mod hash level).symm
    _ = wideBits hash level - 2 ^ level := key

/-- The wide residue equals the low residue, or the low residue plus the old base.
Arithmetic heart of the split-stability argument. -/
theorem wide_eq_low_or_low_add_base (hash level : Nat) :
    wideBits hash level = lowBits hash level ∨
      wideBits hash level = lowBits hash level + 2 ^ level := by
  rcases Nat.lt_or_ge (wideBits hash level) (2 ^ level) with hlow | hhigh
  · refine Or.inl ?_
    have h1 := wideBits_mod hash level
    rw [Nat.mod_eq_of_lt hlow] at h1
    exact h1
  · refine Or.inr ?_
    have hsplit : wideBits hash level
        = (wideBits hash level - 2 ^ level) + 2 ^ level := by
      have hlt : wideBits hash level < 2 ^ (level + 1) := lt_mod_two_pow hash (level + 1)
      have hp1 : 2 ^ (level + 1) = 2 * 2 ^ level := two_pow_succ_eq level
      omega
    rw [hsplit, ← lowBits_eq_sub_wide hash level hhigh]

/-- Pure-arithmetic core of P1, stated over abstract residues so the final step is a
plain linear argument. -/
theorem routing_bound_core (l w cursor base : Nat)
    (hf1 : l < base) (hf2 : w < 2 * base)
    (hf3 : w = l ∨ w = l + base) :
    (if l < cursor then w else l) < base + cursor := by
  split
  · rename_i hb
    rcases hf3 with e | e
    · subst e
      omega
    · subst e
      omega
  · omega

/-- **P1**: a routed bucket index is strictly below `2^level + cursor` — unconditionally,
for arbitrary hash, level, and cursor values. Under the control invariant
`physical_buckets = 2^level + split_cursor ∧ split_cursor < 2^level` (exactly what
`validate_control` enforces, map.rs L1047-L1068), the bound equals `physical_buckets`,
so every routed access lands inside the allocated extent. -/
theorem route_lt_base_plus_cursor (hash level cursor : Nat) :
    linearBucket hash level cursor < 2 ^ level + cursor := by
  have hf1 : lowBits hash level < 2 ^ level := lt_mod_two_pow hash level
  have hf2 : wideBits hash level < 2 * 2 ^ level := by
    rw [← two_pow_succ_eq]
    exact lt_mod_two_pow hash (level + 1)
  have hf3 : wideBits hash level = lowBits hash level ∨
      wideBits hash level = lowBits hash level + 2 ^ level :=
    wide_eq_low_or_low_add_base hash level
  unfold linearBucket
  exact routing_bound_core _ _ _ _ hf1 hf2 hf3

/-! ## Split stability (map.rs L1693-L1709 `next_geometry` drives these geometries) -/

/-- **P2a**: stepping the level (`level+1`, cursor `0`) sends each entry to its old
bucket or to `old + 2^level`. Unconditional: holds for arbitrary hashes and cursors. -/
theorem split_stability_level_up (hash level cursor : Nat) :
    linearBucket hash (level + 1) 0 = linearBucket hash level cursor ∨
      linearBucket hash (level + 1) 0 = linearBucket hash level cursor + 2 ^ level := by
  have hnew : linearBucket hash (level + 1) 0 = wideBits hash level :=
    linearBucket_at_zero hash (level + 1)
  rw [hnew]
  rcases wide_eq_low_or_low_add_base hash level with hw | hw
  · rcases Nat.lt_or_ge (lowBits hash level) cursor with hc | hc
    · rw [linearBucket_eq_wide _ _ _ hc]; exact Or.inl rfl
    · rw [linearBucket_eq_low _ _ _ (by omega), ← hw]; exact Or.inl rfl
  · rcases Nat.lt_or_ge (lowBits hash level) cursor with hc | hc
    · rw [linearBucket_eq_wide _ _ _ hc]; exact Or.inl rfl
    · rw [linearBucket_eq_low _ _ _ (by omega)]; exact Or.inr hw

/-- **P2b**: advancing the cursor at fixed level sends each entry to its old bucket or to
`old + 2^level`. Unconditional. -/
theorem split_stability_cursor_adv (hash level cursor : Nat) :
    linearBucket hash level (cursor + 1) = linearBucket hash level cursor ∨
      linearBucket hash level (cursor + 1) = linearBucket hash level cursor + 2 ^ level := by
  rcases Nat.lt_trichotomy (lowBits hash level) cursor with hlt | heq | hgt
  · have h1 : linearBucket hash level cursor = wideBits hash level :=
      linearBucket_eq_wide _ _ _ hlt
    have h2 : linearBucket hash level (cursor + 1) = wideBits hash level :=
      linearBucket_eq_wide _ _ _ (by omega)
    exact Or.inl (by rw [h1, h2])
  · have hold : linearBucket hash level cursor = lowBits hash level :=
      linearBucket_eq_low _ _ _ (by omega)
    have hnew : linearBucket hash level (cursor + 1) = wideBits hash level :=
      linearBucket_eq_wide _ _ _ (by omega)
    rcases wide_eq_low_or_low_add_base hash level with hw | hw
    · rw [hnew, hold, hw]
      exact Or.inl rfl
    · rw [hnew, hold]
      exact Or.inr hw
  · have h1 : linearBucket hash level cursor = lowBits hash level :=
      linearBucket_eq_low _ _ _ (by omega)
    have h2 : linearBucket hash level (cursor + 1) = lowBits hash level :=
      linearBucket_eq_low _ _ _ (by omega)
    exact Or.inl (by rw [h1, h2])

/-! ## Split geometry (map.rs L1693-L1709 `next_geometry`,
map.rs L1719-L1721 `base_buckets`) -/

/-- Result geometry of one standard split-pointer step. -/
structure Geometry where
  level : Nat
  cursor : Nat
  buckets : Nat

/-- Faithful transcription of `next_geometry`. `base_buckets(level)` is `2^level`
guarded by `level < 63`; the level increment carries the same guard, so both failure
paths return `none` (the caller maps that to `CapacityOverflow`). Conditions are
Prop-valued here; the Rust source compares u64s with `==`, same semantics. -/
def nextGeometry (level cursor buckets : Nat) : Option Geometry :=
  if level < 63 then
    if cursor + 1 = 2 ^ level then
      if level + 1 < 63 then some ⟨level + 1, 0, buckets + 1⟩ else none
    else some ⟨level, cursor + 1, buckets + 1⟩
  else none

/-- **P3**: a successful geometry step keeps `cursor < 2^level` (so the successor shape
is well-formed) and increments the bucket count by exactly one. -/
theorem next_geometry_shape (level cursor buckets : Nat) (hc : cursor < 2 ^ level) :
    ∀ g, nextGeometry level cursor buckets = some g →
      g.cursor < 2 ^ g.level ∧ g.buckets = buckets + 1 := by
  intro g hg
  rw [nextGeometry] at hg
  split at hg
  · split at hg
    · split at hg
      · cases hg
        dsimp only
        exact ⟨two_pow_pos _, rfl⟩
      · simp at hg
    · cases hg
      dsimp only
      exact ⟨by omega, rfl⟩
  · simp at hg

/-! ## Split threshold (map.rs L1711-L1717 `split_threshold`) -/

/-- Faithful transcription of `split_threshold` (`capacity * 3 / 4` with
`capacity = buckets * SLOTS_PER_BUCKET`). Rust's checked multiplications fail closed on
u64 overflow; per SCOPE.md A3 the model keeps exact arithmetic, which is the same
admission semantics away from the u64 ceiling. -/
def splitThreshold (physicalBuckets : Nat) : Nat :=
  physicalBuckets * SlotsPerBucket * 3 / 4

theorem slots_mul_mono {a b : Nat} (h : a ≤ b) :
    a * SlotsPerBucket ≤ b * SlotsPerBucket :=
  Nat.mul_le_mul h (Nat.le_refl _)

/-- Generic quarter-threshold facts, independent of the slot constant. -/
theorem quarter_div_mono {a b s : Nat} (h : a ≤ b) :
    a * s * 3 / 4 ≤ b * s * 3 / 4 :=
  Nat.div_le_div_right
    (Nat.mul_le_mul (Nat.mul_le_mul h (Nat.le_refl s)) (Nat.le_refl 3))

theorem quarter_div_le (b s : Nat) : b * s * 3 / 4 ≤ b * s := by
  have hdm := Nat.div_add_mod (b * s * 3) 4
  omega

theorem quarter_div_lt {b s : Nat} (hb : 0 < b) (hs : 0 < s) : b * s * 3 / 4 < b * s := by
  have hdm := Nat.div_add_mod (b * s * 3) 4
  have hx : 0 < b * s := Nat.mul_pos hb hs
  omega

/-- **P5a**: the threshold is monotone in the physical bucket count. -/
theorem split_threshold_mono {a b : Nat} (h : a ≤ b) :
    splitThreshold a ≤ splitThreshold b := by
  unfold splitThreshold
  exact quarter_div_mono h

/-- **P5b**: the threshold never exceeds the slot capacity. -/
theorem split_threshold_le_capacity (b : Nat) :
    splitThreshold b ≤ b * SlotsPerBucket := by
  unfold splitThreshold
  exact quarter_div_le b SlotsPerBucket

/-- **P5c**: with at least one physical bucket the threshold is strictly below capacity. -/
theorem split_threshold_lt_capacity (b : Nat) (hb : 0 < b) :
    splitThreshold b < b * SlotsPerBucket := by
  unfold splitThreshold
  have hs : 0 < SlotsPerBucket := by
    unfold SlotsPerBucket PrimarySlots PagesPerBucket
    decide
  exact quarter_div_lt hb hs

end Lhm
