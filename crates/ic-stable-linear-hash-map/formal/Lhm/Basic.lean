/-
Arithmetic foundations for the LHM model: powers of two and the two modular
identities that all routing proofs rest on. Core Lean only, no Mathlib.
-/
namespace Lhm

theorem two_pow_pos (k : Nat) : 0 < 2 ^ k := by
  induction k with
  | zero => simp
  | succ n ih => rw [Nat.pow_succ]; omega

theorem lt_mod_two_pow (x k : Nat) : x % 2 ^ k < 2 ^ k :=
  Nat.mod_lt x (two_pow_pos k)

theorem two_pow_succ_eq (k : Nat) : 2 ^ (k + 1) = 2 * 2 ^ k := by
  rw [Nat.pow_succ]; omega

/-- Reducing a residue by one more power of two is transparent:
`(x % 2^(k+1)) % 2^k = x % 2^k`. -/
theorem mod_mod_two_pow_succ (x k : Nat) :
    (x % 2 ^ (k + 1)) % 2 ^ k = x % 2 ^ k := by
  have hp : 2 ^ (k + 1) = 2 ^ k * 2 := Nat.pow_succ 2 k
  have hdm := Nat.div_add_mod x (2 ^ (k + 1))
  have hx : x = 2 ^ k * (2 * (x / 2 ^ (k + 1))) + x % 2 ^ (k + 1) := by
    rw [← Nat.mul_assoc, ← hp]
    exact hdm.symm
  have main : x % 2 ^ k = (x % 2 ^ (k + 1)) % 2 ^ k := by
    calc x % 2 ^ k
        = (2 ^ k * (2 * (x / 2 ^ (k + 1))) + x % 2 ^ (k + 1)) % 2 ^ k := by
            conv => lhs; rw [hx]
      _ = (x % 2 ^ (k + 1) + 2 ^ k * (2 * (x / 2 ^ (k + 1)))) % 2 ^ k := by
          rw [Nat.add_comm (2 ^ k * (2 * (x / 2 ^ (k + 1)))) (x % 2 ^ (k + 1))]
      _ = (x % 2 ^ (k + 1)) % 2 ^ k := by
          rw [Nat.add_mul_mod_self_left]
  exact main.symm

end Lhm
