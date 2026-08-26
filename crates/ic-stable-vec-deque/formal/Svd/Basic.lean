/- Arithmetic foundations for the SVD model: modular and division identities
over arbitrary positive moduli (block sizes, virtual capacities, and directory
capacities are arbitrary positive `u64`s, not powers of two). Core Lean only,
no Mathlib.
-/
namespace Svd

/-- Adding a multiple of the modulus before reducing is transparent:
`(x + q * m) % m = x % m`. -/
theorem add_mul_mod (m x q : Nat) : (x + q * m) % m = x % m := by
  rw [Nat.mul_comm]
  exact Nat.add_mul_mod_self_left x m q

/-! ## Division/modulo decomposition toolkit for block routing

A virtual position decomposes uniquely as `r = B * k + s` with `s < B`, and the
decomposition is exactly `(r / B, r % B)`.
-/

theorem mulAddDiv {b k s : Nat} (hb : 0 < b) : (b * k + s) / b = k + s / b :=
  Nat.mul_add_div hb k s

theorem mulAddMod {b k s : Nat} : (b * k + s) % b = s % b :=
  Nat.mul_add_mod b k s

/-- Uniqueness of the `(quotient, remainder)` decomposition with remainder
strictly below one block. -/
theorem divModUnique {B k s : Nat} (hB : 0 < B) (hs : s < B) :
    (B * k + s) / B = k ∧ (B * k + s) % B = s := by
  constructor
  · rw [mulAddDiv hB, Nat.div_eq_of_lt hs, Nat.add_zero]
  · rw [mulAddMod, Nat.mod_eq_of_lt hs]

/-- Reducing two in-range values through one modulus is injective. -/
theorem modInjOnRange {V a b : Nat} (ha : a < V) (hb : b < V)
    (h : a % V = b % V) : a = b := by
  rw [Nat.mod_eq_of_lt ha, Nat.mod_eq_of_lt hb] at h
  exact h

/-- If shifting by `d` does not change a residue modulo `N`, and `d < N`, then
`d = 0`. The quotient `(x + d) / N` can only be `x / N` or `x / N + 1`; the
second forces `d = N`, contradicting `d < N`. -/
theorem eq_zero_of_mod_add_mod {N x d : Nat} (hN : 0 < N)
    (h : x % N = (x + d) % N) (hd : d < N) : d = 0 := by
  have hr : x % N < N := Nat.mod_lt x hN
  have h1 := Nat.div_add_mod x N        -- N * (x/N) + x%N = x
  have h2 := Nat.div_add_mod (x + d) N  -- N * ((x+d)/N) + (x+d)%N = x + d
  rw [← h] at h2                        -- N * ((x+d)/N) + x%N = x + d
  have hstep : (x + N) / N = x / N + 1 := by
    have h4 := Nat.div_add_mod (x + N) N
    have hs : (x + N) % N = x % N := by
      have h5 := add_mul_mod N x 1
      rwa [Nat.one_mul] at h5
    rw [hs] at h4                       -- N * ((x+N)/N) + x%N = x + N
    have heq : N * ((x + N) / N) = N * (x / N + 1) := by
      rw [Nat.mul_add, Nat.mul_one]
      omega                             -- links through h4 and h1
    exact Nat.eq_of_mul_eq_mul_left hN heq
  have hle1 : (x + d) / N ≤ x / N + 1 := by
    have hle := Nat.div_le_div_right (c := N) (show x + d ≤ x + N from by omega)
    omega
  have hge : x / N ≤ (x + d) / N :=
    Nat.div_le_div_right (c := N) (show x ≤ x + d from by omega)
  rcases Nat.lt_or_ge ((x + d) / N) (x / N + 1) with hc | hc
  · have heq : (x + d) / N = x / N := by omega
    rw [heq] at h2
    omega
  · have heq2 : (x + d) / N = x / N + 1 := by omega
    rw [heq2, Nat.mul_add, Nat.mul_one] at h2
    omega

/-- Rotation `(j + rot) % N` is injective on `[0, N)`. -/
theorem rotateInj {N rot : Nat} {j k : Nat} (hj : j < N) (hk : k < N)
    (h : (j + rot) % N = (k + rot) % N) : j = k := by
  have hN : 0 < N := by omega
  rcases Nat.lt_or_ge j k with hlt | hge
  · have hd : k - j < N := by omega
    have hz : k - j = 0 := by
      apply eq_zero_of_mod_add_mod hN (x := j + rot) (d := k - j)
      · have heq : j + rot + (k - j) = k + rot := by omega
        rw [heq]
        exact h
      · exact hd
    omega
  · have hd : j - k < N := by omega
    have hz : j - k = 0 := by
      apply eq_zero_of_mod_add_mod hN (x := k + rot) (d := j - k)
      · have heq : k + rot + (j - k) = j + rot := by omega
        rw [heq]
        exact h.symm
      · exact hd
    omega

/-- Canceling a common summand through a modulus is injective on `[0, N)`. -/
theorem addModInjOnRange {N w u v : Nat} (hu : u < N) (hv : v < N)
    (h : (u + w) % N = (v + w) % N) : u = v := by
  have hN : 0 < N := by omega
  rcases Nat.lt_or_ge u v with hlt | hge
  · have hd : v - u < N := by omega
    have hz : v - u = 0 := by
      apply eq_zero_of_mod_add_mod hN (x := u + w) (d := v - u)
      · have heq : u + w + (v - u) = v + w := by omega
        rw [heq]
        exact h
      · exact hd
    omega
  · have hd : u - v < N := by omega
    have hz : u - v = 0 := by
      apply eq_zero_of_mod_add_mod hN (x := v + w) (d := u - v)
      · have heq : v + w + (u - v) = u + w := by omega
        rw [heq]
        exact h.symm
      · exact hd
    omega

end Svd
