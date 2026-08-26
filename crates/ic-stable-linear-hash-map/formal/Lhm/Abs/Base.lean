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

/-- A strict gap between prefix counts certifies an untouched selected slot. -/
theorem countMatch_gt_exists_true (p : Nat → Bool) :
    ∀ n i, i ≤ n → countMatch p i < countMatch p n → ∃ m, i ≤ m ∧ m < n ∧ p m = true := by
  intro n
  induction n with
  | zero =>
      intro i _ h
      exfalso
      have hc0 : countMatch p 0 = 0 := rfl
      have hci := countMatch_le p i
      omega
  | succ k ih =>
      intro i hi hlt
      cases hb : p k with
      | true =>
          rcases Nat.lt_or_ge i (k + 1) with hik | hik2
          · exact ⟨k, by omega, Nat.lt_succ_self k, hb⟩
          · exfalso
            have hieq : i = k + 1 := by omega
            subst hieq
            omega
      | false =>
          rcases Nat.lt_or_ge i k with hik | hik2
          · rw [countMatch_succ_of_false p k hb] at hlt
            obtain ⟨m, hm1, hm2, hm3⟩ := ih i (by omega) (by omega)
            exact ⟨m, hm1, by omega, hm3⟩
          · exfalso
            rw [countMatch_succ_of_false p k hb] at hlt
            have hm := countMatch_mono p i k hik2
            have hieq : i = k := by omega
            subst hieq
            omega

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

/-
## Redistributing one counted range between two images

Stage-4 tools: a split relocates one bucket's slots to one of two destination
buckets, preserving per-slot identity. These lemmas account for that redistribution
additively.
-/

/-- Every `p`-position belongs to exactly one of `q` / `r` and vice versa, so the
counts add up: `count q + count r = count p`. -/
theorem countMatch_split (p q r : Nat → Bool) :
    ∀ S,
      (∀ i, i < S → q i = true → p i = true ∧ r i = false) →
      (∀ i, i < S → r i = true → p i = true ∧ q i = false) →
      (∀ i, i < S → p i = true → q i = true ∨ r i = true) →
      countMatch q S + countMatch r S = countMatch p S := by
  intro S
  induction S with
  | zero => intro _ _ _; rfl
  | succ m ih =>
      intro h1 h2 h3
      have ih' : countMatch q m + countMatch r m = countMatch p m :=
        ih (fun i hi hqi => h1 i (Nat.lt_succ_of_lt hi) hqi)
          (fun i hi hri => h2 i (Nat.lt_succ_of_lt hi) hri)
          (fun i hi hpi => h3 i (Nat.lt_succ_of_lt hi) hpi)
      by_cases hpT : p m = true
      · obtain hd : q m = true ∨ r m = true :=
          h3 m (Nat.lt_succ_self m) hpT
        rcases hd with hd | hd
        · have hrF := (h1 m (Nat.lt_succ_self m) hd).2
          rw [countMatch_succ_of_true q m hd, countMatch_succ_of_false r m hrF,
            countMatch_succ_of_true p m hpT]
          omega
        · have hqF := (h2 m (Nat.lt_succ_self m) hd).2
          rw [countMatch_succ_of_false q m hqF, countMatch_succ_of_true r m hd,
            countMatch_succ_of_true p m hpT]
          omega
      · have hpF : p m = false := by simpa using hpT
        have hqF : q m = false := by
          by_cases hc : q m = true
          · exact absurd ((h1 m (Nat.lt_succ_self m) hc).1) hpT
          · simpa using hc
        have hrF : r m = false := by
          by_cases hc : r m = true
          · exact absurd ((h2 m (Nat.lt_succ_self m) hc).1) hpT
          · simpa using hc
        rw [countMatch_succ_of_false q m hqF, countMatch_succ_of_false r m hrF,
          countMatch_succ_of_false p m hpF]
        exact ih'

/-! ## Packing matched slots contiguously (stage-4 split repacking)

A split redistributes the source block's entries between two destination blocks,
re-packing each destination's share into slots `[0, …)` in ascending source order —
exactly what the `entries_from_image` enumeration plus `append_entry_to_image`
compute (map.rs L1787-L1806, L1489-L1490). The tools below are generic: `f` is one
block's flattened content function and `p` selects the entries routed to one
destination. -/

variable {α : Type}

/-- Predicate form of "slot `i` holds an entry selected by `p`". -/
def blockPred (f : Nat → Option α) (p : α → Bool) : Nat → Bool :=
  fun i =>
    match f i with
    | some e => p e
    | none => false

/-- Forward scan for the first selected slot in `[i, stop)`. `depth` bounds the
number of examined slots (callers instantiate it with `stop`); keeping `stop`
absolute through the recursion is what makes the window claims induction-friendly. -/
def scanTo (f : Nat → Option α) (p : α → Bool) : Nat → Nat → Nat → Option (Nat × α)
  | 0, _, _ => none
  | depth + 1, stop, i =>
      if _h : i < stop then
        match f i with
        | some e => if p e then some (i, e) else scanTo f p depth stop (i + 1)
        | none => scanTo f p depth stop (i + 1)
      else none

/-- A failed scan saw no selected slot in its window. The reachability hypothesis
`stop ≤ i + d` says the depth budget covers the whole window (callers use
`d = stop`, where it is trivial). -/
theorem scanTo_none (f : Nat → Option α) (p : α → Bool) :
    ∀ d stop i m, stop ≤ i + d → scanTo f p d stop i = none →
      i ≤ m → m < stop → blockPred f p m = false := by
  intro d
  induction d with
  | zero => intro stop i m hd _ _ _; exfalso; omega
  | succ k ih =>
      intro stop i m hd h hi hm
      simp only [scanTo] at h
      by_cases hic : i < stop
      · rw [dif_pos hic] at h
        cases hf : f i with
        | none =>
            simp only [hf] at h
            rcases Nat.lt_or_ge m (i + 1) with hlt | hge
            · have hmeq : m = i := by omega
              subst hmeq
              simp only [blockPred, hf]
            · exact ih stop (i + 1) m (by omega) h hge (by omega)
        | some e =>
            by_cases hp : p e = true
            · simp only [hf, hp] at h
              exact absurd h (by simp)
            · simp only [hf] at h
              rw [if_neg hp] at h
              rcases Nat.eq_or_lt_of_le hi with he | hgt
              · subst he; simp [blockPred, hf, hp]
              · exact ih stop (i + 1) m (by omega) h (by omega) (by omega)
      · rw [dif_neg hic] at h
        exact absurd hm (by omega)

/-- A successful scan lands on a genuinely selected slot inside its window. -/
theorem scanTo_sound (f : Nat → Option α) (p : α → Bool) :
    ∀ d stop i k e, scanTo f p d stop i = some (k, e) →
      i ≤ k ∧ k < stop ∧ blockPred f p k = true ∧ f k = some e := by
  intro d
  induction d with
  | zero =>
      intro stop i k e h
      simp only [scanTo] at h
      exact absurd h (by simp)
  | succ k ih =>
      intro stop i ke e h
      simp only [scanTo] at h
      by_cases hic : i < stop
      · rw [dif_pos hic] at h
        cases hf : f i with
        | none =>
            simp only [hf] at h
            obtain ⟨h1, h2, h3, h4⟩ := ih stop (i + 1) ke e h
            exact ⟨by omega, by omega, h3, h4⟩
        | some ex =>
            by_cases hp : p ex = true
            · simp only [hf, hp] at h
              obtain ⟨rfl, rfl⟩ := h
              have hb : blockPred f p i = true := by
                simp only [blockPred, hf]
                exact hp
              exact ⟨Nat.le_refl _, by omega, hb, hf⟩
            · simp only [hf] at h
              rw [if_neg hp] at h
              obtain ⟨h1, h2, h3, h4⟩ := ih stop (i + 1) ke e h
              exact ⟨by omega, by omega, h3, h4⟩
      · rw [dif_neg hic] at h
        exact absurd h (by simp)

/-- A successful scan lands on the *first* selected slot of its window. -/
theorem scanTo_first (f : Nat → Option α) (p : α → Bool) :
    ∀ d stop i k e, scanTo f p d stop i = some (k, e) →
      ∀ m, i ≤ m → m < k → blockPred f p m = false := by
  intro d
  induction d with
  | zero =>
      intro stop i k e h
      simp only [scanTo] at h
      exact absurd h (by simp)
  | succ j ih =>
      intro stop i k e h m hm1 hm2
      simp only [scanTo] at h
      by_cases hic : i < stop
      · rw [dif_pos hic] at h
        cases hf : f i with
        | none =>
            simp only [hf] at h
            rcases Nat.lt_or_ge m (i + 1) with hlt | hge
            · have hmeq : m = i := by omega
              subst hmeq
              simp only [blockPred, hf]
            · exact ih stop (i + 1) k e h m hge hm2
        | some ex =>
            by_cases hp : p ex = true
            · simp only [hf, hp] at h
              obtain ⟨rfl, rfl⟩ := h
              exact absurd hm2 (by omega)
            · simp only [hf] at h
              rw [if_neg hp] at h
              rcases Nat.lt_or_ge m (i + 1) with hlt | hge
              · have hmeq : m = i := by omega
                subst hmeq
                simp [blockPred, hf, hp]
              · exact ih stop (i + 1) k e h m hge hm2
      · rw [dif_neg hic] at h
        exact absurd h (by simp)

/-- Counting skips an all-false stretch unchanged. -/
theorem countMatch_none_between (p : Nat → Bool) :
    ∀ b a, a ≤ b → (∀ m, a ≤ m → m < b → p m = false) → countMatch p b = countMatch p a := by
  intro b
  induction b with
  | zero =>
      intro a ha _
      have : a = 0 := by omega
      subst this
      rfl
  | succ k ih =>
      intro a ha hnone
      rcases Nat.lt_or_ge a k with hlt | hge
      · have hk := hnone k (Nat.le_of_lt hlt) (Nat.lt_succ_self k)
        have hprev := ih a (Nat.le_of_lt hlt)
          (fun m hm hm2 => hnone m hm (by omega))
        rw [countMatch_succ_of_false p k hk, hprev]
      · rcases Nat.lt_or_ge a (k + 1) with hlt2 | hge2
        · have haeq : a = k := by omega
          subst haeq
          rw [countMatch_succ_of_false p a
            (hnone a (Nat.le_refl a) (Nat.lt_succ_self a))]
        · have haeq : a = k + 1 := by omega
          subst haeq
          rfl

/-- The `j`-th selected entry of `[i, stop)` together with its source slot. Output
index `j` is zero-based; `nthPack f p n 0 j` is output slot `j` of a destination
image packed from source slots `[0, n)`. -/
def nthPack (f : Nat → Option α) (p : α → Bool) (n i j : Nat) : Option (Nat × α) :=
  match j with
  | 0 => scanTo f p n n i
  | j + 1 =>
      match scanTo f p n n i with
      | none => none
      | some (k, _) => nthPack f p n (k + 1) j

/-- Full specification of the packed selection: output `j` draws the entry at the
source slot whose prefix-count of selections is exactly `j`. -/
theorem nthPack_spec (f : Nat → Option α) (p : α → Bool) (n : Nat) :
    ∀ i j k e, nthPack f p n i j = some (k, e) →
      i ≤ k ∧ k < n ∧ blockPred f p k = true ∧ f k = some e ∧
        countMatch (blockPred f p) k = countMatch (blockPred f p) i + j := by
  intro i j
  revert i
  induction j with
  | zero =>
      intro i k e h
      obtain ⟨h1, h2, h3, h4⟩ := scanTo_sound f p n n i k e h
      refine ⟨h1, h2, h3, h4, ?_⟩
      rw [countMatch_none_between (blockPred f p) k i h1
        (fun m hm hm2 => scanTo_first f p n n i k e h m hm hm2)]
      omega
  | succ j ih =>
      intro i k e h
      simp only [nthPack] at h
      cases hx : scanTo f p n n i with
      | none => rw [hx] at h; exact absurd h (by simp)
      | some r =>
          obtain ⟨k0, e0⟩ := r
          simp only [hx] at h
          obtain ⟨h1, h2, h3, h4, h5⟩ := ih (k0 + 1) k e h
          obtain ⟨hs1, hs2, hs3, _⟩ := scanTo_sound f p n n i k0 e0 hx
          have hc0 : countMatch (blockPred f p) k0 = countMatch (blockPred f p) i :=
            countMatch_none_between _ k0 i hs1
              (fun m hm hm2 => scanTo_first f p n n i k0 e0 hx m hm hm2)
          refine ⟨by omega, by omega, h3, h4, ?_⟩
          rw [countMatch_succ_of_true _ k0 hs3, hc0] at h5
          omega

/-- Every output slot below the selection count is filled. -/
theorem nthPack_surj (f : Nat → Option α) (p : α → Bool) (n : Nat) :
    ∀ i j, i ≤ n → countMatch (blockPred f p) i + j < countMatch (blockPred f p) n →
      ∃ k e, nthPack f p n i j = some (k, e) := by
  intro i j
  revert i
  induction j with
  | zero =>
      intro i hle hgap
      have hexists : ∃ m, i ≤ m ∧ m < n ∧ blockPred f p m = true :=
        countMatch_gt_exists_true (blockPred f p) n i hle (by omega)
      obtain ⟨m, hm1, hm2, hm3⟩ := hexists
      cases hx : scanTo f p n n i with
      | none =>
          exfalso
          rcases Nat.lt_or_ge i n with hilt | hige
          · have hfail := scanTo_none f p n n i m (by omega) hx hm1 hm2
            rw [hfail] at hm3
            simp at hm3
          · have hieq : i = n := by omega
            subst hieq
            omega
      | some r =>
          obtain ⟨kk, ee⟩ := r
          exact ⟨kk, ee, hx⟩
  | succ j ih =>
      intro i hle hgap
      cases hx : scanTo f p n n i with
      | none =>
          exfalso
          rcases Nat.lt_or_ge i n with hilt | hige
          · have hreach : n ≤ i + n := by omega
            have hnb := countMatch_none_between (blockPred f p) n i hle
              (fun m hm hm2 => scanTo_none f p n n i m hreach hx hm hm2)
            omega
          · have hieq : i = n := by omega
            subst hieq
            omega
      | some r =>
          obtain ⟨k0, e0⟩ := r
          obtain ⟨hs1, hs2, hs3, _⟩ := scanTo_sound f p n n i k0 e0 hx
          have hc0 : countMatch (blockPred f p) k0 = countMatch (blockPred f p) i :=
            countMatch_none_between _ k0 i hs1
              (fun m hm hm2 => scanTo_first f p n n i k0 e0 hx m hm hm2)
          have hrec := ih (k0 + 1) (by omega)
            (by rw [countMatch_succ_of_true _ k0 hs3, hc0]; omega)
          obtain ⟨k, e, hk⟩ := hrec
          exact ⟨k, e, by simpa [nthPack, hx] using hk⟩

/-- Content of output slot `j` of a packed image (source slot dropped). -/
def packImg (f : Nat → Option α) (p : α → Bool) (n j : Nat) : Option α :=
  match nthPack f p n 0 j with
  | some (_, e) => some e
  | none => none

/-- Packed output slot `j` carries an entry drawn from a real selected source slot,
and `j` equals that slot's prefix selection count. -/
theorem packImg_spec (f : Nat → Option α) (p : α → Bool) (n j : Nat) (e : α)
    (h : packImg f p n j = some e) :
    ∃ k, k < n ∧ f k = some e ∧ p e = true ∧
      countMatch (blockPred f p) k = j := by
  unfold packImg at h
  cases hx : nthPack f p n 0 j with
  | none => rw [hx] at h; simp at h
  | some r =>
      obtain ⟨k, e'⟩ := r
      obtain ⟨h1, h2, h3, h4, h5⟩ := nthPack_spec f p n 0 j k e' hx
      have heq : e' = e := by
        rw [hx] at h
        have hpair : some e' = some e := h
        exact Option.some.inj hpair
      subst heq
      refine ⟨k, h2, ?_, ?_, ?_⟩
      · exact h4
      · have hb : blockPred f p k = p e' := by
          show (match f k with | some x => p x | none => false) = p e'
          rw [h4]
        rw [h3] at hb
        exact hb.symm
      · have h0 : countMatch (blockPred f p) 0 = 0 := rfl
        omega

/-- Output slots below the selection count are filled. -/
theorem packImg_surj (f : Nat → Option α) (p : α → Bool) (n j : Nat)
    (h : j < countMatch (blockPred f p) n) :
    ∃ e, packImg f p n j = some e := by
  obtain ⟨k, e, hk⟩ := nthPack_surj f p n 0 j (Nat.zero_le n) (by
    have : countMatch (blockPred f p) 0 = 0 := rfl
    simpa [this] using h)
  exact ⟨e, by
    unfold packImg
    rw [hk]⟩

/-- Occupancy of a packed image is exactly the prefix selection count. -/
theorem packImg_isSome (f : Nat → Option α) (p : α → Bool) (n j : Nat) :
    (packImg f p n j).isSome = decide (j < countMatch (blockPred f p) n) := by
  by_cases hx : j < countMatch (blockPred f p) n
  · obtain ⟨e, he⟩ := packImg_surj f p n j hx
    rw [he]; simp [hx]
  · have hnone : packImg f p n j = none := by
      cases hcase : packImg f p n j with
      | none => rfl
      | some e =>
          exfalso
          obtain ⟨k, hk1, hk2, hk3, hk4⟩ := packImg_spec f p n j e hcase
          have hbp : blockPred f p k = true := by
            show (match f k with | some x => p x | none => false) = true
            rw [hk2]
            exact hk3
          have hc1 : countMatch (blockPred f p) (k + 1)
              = countMatch (blockPred f p) k + 1 :=
            countMatch_succ_of_true _ k hbp
          have hm1 := countMatch_mono (blockPred f p) n (k + 1) (by omega)
          omega
    rw [hnone]; simp [hx]

/-- Counting a truncated "below `c`" predicate yields `c` when the range covers it. -/
theorem countMatch_decide_lt (c : Nat) :
    ∀ S, c ≤ S → countMatch (fun j => decide (j < c)) S = c := by
  intro S
  induction S with
  | zero =>
      intro h
      have : c = 0 := by omega
      subst this
      rfl
  | succ m ih =>
      intro h
      by_cases hcm : c ≤ m
      · rw [countMatch_succ_of_false _ m (by
          rw [decide_eq_false_iff_not]; omega), ih hcm]
      · have hceq : c = m + 1 := by omega
        subst hceq
        exact countMatch_full _ _ (fun _ hj => decide_eq_true hj)

/-- Sum agreement everywhere except one index, kept in additive form. -/
theorem totalLoads_except (f g : Nat → Nat) :
    ∀ n j, j < n → (∀ b, b < n → b ≠ j → f b = g b) →
      totalLoads f n + g j = totalLoads g n + f j := by
  intro n
  induction n with
  | zero => intro j hj; exact absurd hj (Nat.not_lt_zero j)
  | succ m ih =>
      intro j hj hagree
      rcases Nat.lt_or_ge j m with hjm | hjge
      · have him := ih j hjm (fun b hb hne => hagree b (Nat.lt_succ_of_lt hb) hne)
        have hlast : f m = g m := hagree m (Nat.lt_succ_self m) (by omega)
        rw [totalLoads_succ, totalLoads_succ] <;> omega
      · have hjeq : j = m := by omega
        rw [hjeq]
        have hcongr : totalLoads f m = totalLoads g m :=
          totalLoads_congr f g m (fun b hb => hagree b (Nat.lt_succ_of_lt hb) (by omega))
        rw [totalLoads_succ, totalLoads_succ, hcongr] <;> omega

end Lhm.Abs
