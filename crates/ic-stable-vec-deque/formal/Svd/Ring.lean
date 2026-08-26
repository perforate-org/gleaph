/-
Stage 1 — ring arithmetic for the V1 layout of `ic-stable-vec-deque`.

Logical index `i ∈ [0, len)` maps to physical slot `(head + i) % cap`
(vec_deque.rs L355-L359 `physical_index`; layout contract at vec_deque.rs L7-L41).
Capacity is an arbitrary positive `u64`, not a power of two, so these proofs are
over general modular arithmetic. Core Lean only.
-/
import Svd.Basic

namespace Svd

/-- Physical slot of logical index `logical` in a ring with front at `head` and
capacity `cap` (vec_deque.rs L355-L359 `physical_index`). -/
def physicalIndex (head logical cap : Nat) : Nat :=
  (head + logical) % cap

/-! ## Extent: routed slots stay inside the allocated ring -/

/-- P1 (extent): under a positive capacity and `len ≤ cap`, every logical index
`< len` routes to a physical slot `< cap` (mirrors the `debug_assert!(cap > 0)`
contract of vec_deque.rs L378-L387 `get`). -/
theorem physicalIndex_lt_cap {head len cap : Nat}
    (_hcap : 0 < cap) (_hlen : len ≤ cap) {logical : Nat} (_hlog : logical < len) :
    physicalIndex head logical cap < cap :=
  Nat.mod_lt _ _hcap

/-- Corollary: any logical index below the capacity routes inside the ring. -/
theorem physicalIndex_in_extent {head cap : Nat} (hcap : 0 < cap) {logical : Nat}
    (_hlog : logical < cap) : physicalIndex head logical cap < cap :=
  Nat.mod_lt _ hcap

/-! ## Injection on the occupied window -/

/-- Equal residues over a positive modulus come from arguments that are equal or
differ by a multiple of the modulus. -/
theorem eq_or_diff_mul_of_mod_eq {m a b : Nat} (h : a % m = b % m) :
    a = b ∨ (∃ q : Nat, b + q * m = a) ∨ (∃ q : Nat, a + q * m = b) := by
  rcases Nat.le_total a b with hle | hge
  · -- hardest case first: `b` is the larger argument (`b = a + k*m`)
    right; right
    have hmono := Nat.div_le_div_right (c := m) hle
    have hda := Nat.div_add_mod a m
    have hdb := Nat.div_add_mod b m
    refine ⟨b / m - a / m, ?_⟩
    have hq : b / m = a / m + (b / m - a / m) := by omega
    rw [hq, Nat.mul_add, Nat.mul_comm m (b / m - a / m)] at hdb
    omega
  · -- symmetric case: `a` is the larger argument
    right; left
    have hmono := Nat.div_le_div_right (c := m) hge
    have hda := Nat.div_add_mod a m
    have hdb := Nat.div_add_mod b m
    refine ⟨a / m - b / m, ?_⟩
    have hq : a / m = b / m + (a / m - b / m) := by omega
    rw [hq, Nat.mul_add, Nat.mul_comm m (a / m - b / m)] at hda
    omega

/-- P1-injection helper: on `[0, cap)` the routing function is injective for any
positive capacity (window restriction `len ≤ cap` then specializes it). -/
theorem physicalIndex_injective_of_lt_cap {head cap a b : Nat} (_hcap : 0 < cap)
    (_ha : a < cap) (_hb : b < cap)
    (heq : physicalIndex head a cap = physicalIndex head b cap) : a = b := by
  have hmod : (head + a) % cap = (head + b) % cap := heq
  rcases eq_or_diff_mul_of_mod_eq hmod with heq' | ⟨q, hq⟩ | ⟨q, hq⟩
  · omega
  · -- head + a = head + b + q * cap with a < cap forces q = 0
    cases q with
    | zero => omega
    | succ n =>
      exfalso
      rw [Nat.succ_mul] at hq
      omega
  · -- head + b = head + a + q * cap with b < cap forces q = 0
    cases q with
    | zero => omega
    | succ n =>
      exfalso
      rw [Nat.succ_mul] at hq
      omega

/-- P1-injection: on the occupied logical window `[0, len)`, routing is injective
once `len ≤ cap`: distinct logical indices never share a physical slot
(vec_deque.rs L355-L359 `physical_index` under the layout contract of L208-L218
`init`). -/
theorem physicalIndex_injective_on_window {head len cap a b : Nat}
    (_hcap : 0 < cap) (ha : a < len) (hb : b < len) (hlen : len ≤ cap)
    (heq : physicalIndex head a cap = physicalIndex head b cap) : a = b :=
  physicalIndex_injective_of_lt_cap _hcap (Nat.lt_of_lt_of_le ha hlen)
    (Nat.lt_of_lt_of_le hb hlen) heq

end Svd
