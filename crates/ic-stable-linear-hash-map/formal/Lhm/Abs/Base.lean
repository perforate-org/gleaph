/-
Stage 3 foundations: bounded counting over slot indices.

These are generic tools; nothing here knows about hash maps yet. All bounds are plain
`Nat`s so the proofs stay inside Lean core (no Mathlib).
-/
namespace Lhm.Abs

/-! ## Counting matching positions in `[0, n)` -/

/-- Number of indices `i < n` satisfying `p i`. -/
def countMatch (p : Nat → Bool) : Nat → Nat
  | 0 => 0
  | n + 1 => countMatch p n + (if p n then 1 else 0)

theorem countMatch_succ (p : Nat → Bool) (m : Nat) :
    countMatch p (m + 1) = countMatch p m + (if p m then 1 else 0) := rfl

theorem countMatch_succ_of_true (p : Nat → Bool) (m : Nat) (h : p m = true) :
    countMatch p (m + 1) = countMatch p m + 1 := by
  rw [countMatch_succ, h] <;> simp

theorem countMatch_succ_of_false (p : Nat → Bool) (m : Nat) (h : p m = false) :
    countMatch p (m + 1) = countMatch p m := by
  rw [countMatch_succ, h] <;> simp

theorem countMatch_le (p : Nat → Bool) (n : Nat) : countMatch p n ≤ n := by
  induction n with
  | zero => simp [countMatch]
  | succ m ih =>
      rw [countMatch_succ]
      split <;> omega

/-- Agreement off the counted range gives equal counts. -/
theorem countMatch_congr (p q : Nat → Bool) (n : Nat) (h : ∀ i, i < n → p i = q i) :
    countMatch p n = countMatch q n := by
  induction n with
  | zero => rfl
  | succ m ih =>
      rw [countMatch_succ, countMatch_succ,
        ih (fun i hi => h i (Nat.lt_succ_of_lt hi)), h m (Nat.lt_succ_self m)]

/-- Counting every position yields `n`. -/
theorem countMatch_full (p : Nat → Bool) (n : Nat) (h : ∀ i, i < n → p i = true) :
    countMatch p n = n := by
  induction n with
  | zero => rfl
  | succ m ih =>
      rw [countMatch_succ_of_true p m (h m (Nat.lt_succ_self m)),
        ih (fun i hi => h i (Nat.lt_succ_of_lt hi))]

/-- Fewer than `n` matches means some position fails. -/
theorem countMatch_lt_exists_false (p : Nat → Bool) (n : Nat) (h : countMatch p n < n) :
    ∃ i, i < n ∧ p i = false := by
  induction n with
  | zero => exact absurd h (by simp [countMatch])
  | succ m ih =>
      cases hp : p m with
      | false => exact ⟨m, Nat.lt_succ_self m, hp⟩
      | true =>
          rw [countMatch_succ_of_true p m hp] at h
          have hm : countMatch p m < m := by omega
          obtain ⟨i, hi, hpi⟩ := ih hm
          exact ⟨i, Nat.lt_succ_of_lt hi, hpi⟩

/-! ## Single-position changes -/

/-- One flipped-on position increases the count by exactly one. -/
theorem countMatch_plus_one (p q : Nat → Bool) (n : Nat) :
    ∀ j, j < n → (∀ i, i < n → i ≠ j → p i = q i) → q j = false → p j = true →
      countMatch p n = countMatch q n + 1 := by
  induction n with
  | zero => intro j hj; exact absurd hj (Nat.not_lt_zero j)
  | succ m ih =>
      intro j hj hagree hfrom hto
      rcases Nat.lt_or_ge j m with hjm | hjge
      · have him := ih j hjm
          (fun i hi hne => hagree i (Nat.lt_succ_of_lt hi) hne) hfrom hto
        have hlast := hagree m (Nat.lt_succ_self m) (by omega)
        rw [countMatch_succ, countMatch_succ, him, hlast] <;> omega
      · have hjeq : j = m := by omega
        subst hjeq
        have hcongr : countMatch p j = countMatch q j :=
          countMatch_congr p q j
            (fun i hi => hagree i (Nat.lt_succ_of_lt hi) (by omega))
        rw [countMatch_succ, countMatch_succ, hcongr, hfrom, hto] <;> simp

/-- One flipped-off position decreases the count by exactly one. -/
theorem countMatch_minus_one (p q : Nat → Bool) (n : Nat) :
    ∀ j, j < n → (∀ i, i < n → i ≠ j → p i = q i) → q j = true → p j = false →
      countMatch p n + 1 = countMatch q n := by
  intro j hj hagree hfrom hto
  have hs := countMatch_plus_one q p n j hj
    (fun i hi hne => (hagree i hi hne).symm) hto hfrom
  omega

/-! ## Summing per-bucket values over `[0, n)` -/

/-- Sum of `f b` for `b < n`. -/
def totalLoads (f : Nat → Nat) : Nat → Nat
  | 0 => 0
  | n + 1 => totalLoads f n + f n

theorem totalLoads_succ (f : Nat → Nat) (m : Nat) :
    totalLoads f (m + 1) = totalLoads f m + f m := rfl

theorem totalLoads_congr (f g : Nat → Nat) (n : Nat) (h : ∀ b, b < n → f b = g b) :
    totalLoads f n = totalLoads g n := by
  induction n with
  | zero => rfl
  | succ m ih =>
      rw [totalLoads_succ, totalLoads_succ,
        ih (fun b hb => h b (Nat.lt_succ_of_lt hb)), h m (Nat.lt_succ_self m)]

theorem totalLoads_full_zero (f : Nat → Nat) (n : Nat) (h : ∀ b, b < n → f b = 0) :
    totalLoads f n = 0 := by
  induction n with
  | zero => rfl
  | succ m ih =>
      rw [totalLoads_succ, ih (fun b hb => h b (Nat.lt_succ_of_lt hb)),
        h m (Nat.lt_succ_self m)] <;> omega

theorem totalLoads_plus_one (f g : Nat → Nat) (n : Nat) :
    ∀ j, j < n → (∀ b, b < n → b ≠ j → f b = g b) → f j = g j + 1 →
      totalLoads f n = totalLoads g n + 1 := by
  induction n with
  | zero => intro j hj; exact absurd hj (Nat.not_lt_zero j)
  | succ m ih =>
      intro j hj hagree hdelta
      rcases Nat.lt_or_ge j m with hjm | hjge
      · have him := ih j hjm
          (fun b hb hne => hagree b (Nat.lt_succ_of_lt hb) hne) hdelta
        have hlast := hagree m (Nat.lt_succ_self m) (by omega)
        rw [totalLoads_succ, totalLoads_succ, him, hlast] <;> omega
      · have hjeq : j = m := by omega
        subst hjeq
        have hcongr : totalLoads f j = totalLoads g j :=
          totalLoads_congr f g j
            (fun b hb => hagree b (Nat.lt_succ_of_lt hb) (by omega))
        rw [totalLoads_succ, totalLoads_succ, hcongr, hdelta] <;> omega

theorem totalLoads_minus_one (f g : Nat → Nat) (n : Nat) :
    ∀ j, j < n → (∀ b, b < n → b ≠ j → f b = g b) → f j + 1 = g j →
      totalLoads f n + 1 = totalLoads g n := by
  intro j hj hagree hdelta
  have hs := totalLoads_plus_one g f n j hj
    (fun b hb hne => (hagree b hb hne).symm) (by omega)
  omega

/-- Prefix-monotonicity of the counter. -/
theorem countMatch_mono (p : Nat → Bool) : ∀ n m, m ≤ n → countMatch p m ≤ countMatch p n := by
  intro n
  induction n with
  | zero =>
      intro m hm
      have hm0 : m = 0 := by omega
      subst hm0
      simp [countMatch]
  | succ k ih =>
      intro m hm
      rcases Nat.lt_or_ge m k with hlt | hge
      · have hprev := ih m (by omega)
        have hstep : countMatch p k ≤ countMatch p k + (if p k then 1 else 0) := by
          split <;> omega
        calc countMatch p m ≤ countMatch p k := hprev
          _ ≤ countMatch p k + (if p k then 1 else 0) := hstep
          _ = countMatch p (k + 1) := (countMatch_succ p k).symm
      · rcases Nat.lt_or_ge m (k + 1) with hlt2 | hge2
        · have hmk : m = k := by omega
          rw [hmk]
          have hprev := ih k (Nat.le_refl k)
          have hstep : countMatch p k ≤ countMatch p k + (if p k then 1 else 0) := by
            split <;> omega
          calc countMatch p k ≤ countMatch p k + (if p k then 1 else 0) := hstep
            _ = countMatch p (k + 1) := (countMatch_succ p k).symm
        · have hm1 : m = k + 1 := by omega
          rw [hm1]
          exact Nat.le_refl _

theorem countMatch_all_false (p : Nat → Bool) :
    ∀ n, (∀ i, i < n → p i = false) → countMatch p n = 0 := by
  intro n
  induction n with
  | zero => intro _; rfl
  | succ m ih =>
      intro h
      rw [countMatch_succ_of_false p m (h m (Nat.lt_succ_self m)),
        ih (fun i hi => h i (Nat.lt_succ_of_lt hi))]

theorem totalLoads_ge_single (f : Nat → Nat) :
    ∀ n b, b < n → f b ≤ totalLoads f n := by
  intro n
  induction n with
  | zero => intro b hb; exact absurd hb (Nat.not_lt_zero b)
  | succ m ih =>
      intro b hb
      rcases Nat.lt_or_ge b m with hlt | hge
      · have hprev := ih b hlt
        calc f b ≤ totalLoads f m := hprev
          _ ≤ totalLoads f m + f m := Nat.le_add_right _ _
      · have hb_eq : b = m := by omega
        subst hb_eq
        exact Nat.le_add_left _ _

end Lhm.Abs
