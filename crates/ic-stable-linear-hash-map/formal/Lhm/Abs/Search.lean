/-
Stage 3 search semantics: first-match over flattened slot indices, two-choice
lookup (`get_with_hot`, map.rs L1251-L1258; `get`, map.rs L860-L862), and the
completeness facts that make two-choice search a global key oracle under `Inv`.
-/
import Lhm.Abs.State

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## First match in `[0, n)` -/

def firstMatchAux (p : Nat → Bool) : Nat → Nat → Option Nat
  | _, 0 => none
  | i, n + 1 => if p i then some i else firstMatchAux p (i + 1) n

/-- First index in `[i, i+n)` with `p`, scanning upward. -/
def firstMatch (p : Nat → Bool) (n : Nat) : Option Nat :=
  firstMatchAux p 0 n

theorem firstMatchAux_found {p : Nat → Bool} :
    ∀ n i j, firstMatchAux p i n = some j → j < i + n ∧ i ≤ j ∧ p j = true := by
  intro n
  induction n with
  | zero => intro _ _ h; simp [firstMatchAux] at h
  | succ m ih =>
      intro i j h
      rw [firstMatchAux] at h
      split at h
      · rename_i hp
        injection h with hEq
        subst hEq
        exact ⟨by omega, Nat.le_refl _, hp⟩
      · rename_i _ _
        obtain ⟨hlt, hge, hp⟩ := ih (i + 1) j h
        refine ⟨by omega, Nat.le_trans (Nat.le_succ i) hge, hp⟩

theorem firstMatch_found {p : Nat → Bool} {n i : Nat} (h : firstMatch p n = some i) :
    i < n ∧ p i = true := by
  have hres := firstMatchAux_found n 0 i h
  refine ⟨by omega, hres.2.2⟩

theorem firstMatchAux_none {p : Nat → Bool} :
    ∀ n i, firstMatchAux p i n = none → ∀ d, d < n → p (i + d) = false := by
  intro n
  induction n with
  | zero => intro _ _ d hd; exact absurd hd (Nat.not_lt_zero d)
  | succ m ih =>
      intro i h d hd
      have hf : p i = false := by
        cases hb : p i with
        | false => rfl
        | true =>
            rw [firstMatchAux, hb] at h
            simp at h
      rw [firstMatchAux, hf] at h
      rcases Nat.eq_zero_or_pos d with hd0 | hdpos
      · subst hd0
        simpa using hf
      · have hlt : d - 1 < m := by omega
        have hrec := ih (i + 1) h (d - 1) hlt
        have hidx : i + d = (i + 1) + (d - 1) := by omega
        rw [hidx]
        exact hrec

theorem firstMatch_none_all_false {p : Nat → Bool} {n : Nat}
    (h : firstMatch p n = none) : ∀ i, i < n → p i = false := by
  intro i hi
  have hrec := firstMatchAux_none n 0 (by simpa [firstMatch] using h) i hi
  simpa using hrec

theorem firstMatch_some_of_exists {p : Nat → Bool} {n : Nat}
    (h : ∃ i, i < n ∧ p i = true) : ∃ j, firstMatch p n = some j := by
  cases hm : firstMatch p n with
  | some j => exact ⟨j, rfl⟩
  | none =>
      obtain ⟨i, hi, hpi⟩ := h
      have hall := firstMatch_none_all_false hm i hi
      rw [hall] at hpi
      exact Bool.noConfusion hpi

/-! ## Two-choice lookup -/

/-- Key-match predicate of bucket `b`: compares stored keys, values ignored. -/
def keyPred [DecidableEq K] (st : MapState K V) (b : Nat) (k : K) : Nat → Bool :=
  fun j => decide ((st.buckets b j).map Prod.fst = some k)

theorem keyPred_true_spec [DecidableEq K] {st : MapState K V} {b : Nat} {k : K} {j : Nat}
    (h : keyPred st b k j = true) : ∃ e, st.buckets b j = some e ∧ e.1 = k := by
  unfold keyPred at h
  cases hb : st.buckets b j with
  | none => rw [hb] at h; simp at h
  | some e0 =>
      rw [hb] at h
      simp at h
      exact ⟨e0, rfl, h⟩

theorem keyPred_true_of_loc [DecidableEq K] {st : MapState K V} {b : Nat} {k : K} {j : Nat}
    {e : K × V} (hloc : st.buckets b j = some e) (hkey : e.1 = k) :
    keyPred st b k j = true := by
  rw [keyPred, hloc]
  simp [hkey]

/-- Search one candidate bucket for `k`: first matching slot plus its entry.
Flattened transcription of `find_in_bucket` (map.rs L1310-L1328); the page-major scan
order is identical. -/
def findIn [DecidableEq K] (st : MapState K V) (b : Nat) (k : K) :
    Option (Nat × (K × V)) :=
  match firstMatch (keyPred st b k) SlotsPerBucket with
  | some j => (st.buckets b j).map (fun e => (j, e))
  | none => none

theorem findIn_some_spec [DecidableEq K] {st : MapState K V} {b : Nat} {k : K}
    {j : Nat} {e : K × V} (h : findIn st b k = some (j, e)) :
    j < SlotsPerBucket ∧ st.buckets b j = some e ∧ e.1 = k := by
  unfold findIn at h
  split at h
  · rename_i j0 hm
    obtain ⟨_, hpredj⟩ := firstMatch_found hm
    obtain ⟨e0, hloc0, hkey0⟩ := keyPred_true_spec hpredj
    rw [hloc0] at h
    simp at h
    obtain ⟨rfl, rfl⟩ := h
    exact ⟨(firstMatch_found hm).1, hloc0, hkey0⟩
  · exact absurd h (by simp)

theorem findIn_none_spec [DecidableEq K] {st : MapState K V} {b : Nat} {k : K}
    (h : findIn st b k = none) :
    ∀ j e, j < SlotsPerBucket → st.buckets b j = some e → e.1 ≠ k := by
  intro j e hjb hloc heq
  have hpred : keyPred st b k j = true := keyPred_true_of_loc hloc heq
  obtain ⟨j0, hfm⟩ := firstMatch_some_of_exists (p := keyPred st b k)
    ⟨j, hjb, hpred⟩
  obtain ⟨e0, hloc0, _⟩ := keyPred_true_spec (firstMatch_found hfm).2
  unfold findIn at h
  rw [hfm] at h
  simp [hloc0] at h

theorem findIn_some_of_present [DecidableEq K] {st : MapState K V} {b : Nat} {k : K}
    (hpresent : ∃ j e, j < SlotsPerBucket ∧ st.buckets b j = some e ∧ e.1 = k) :
    ∃ j e, findIn st b k = some (j, e) := by
  obtain ⟨j, e, hjb, hloc, hkey⟩ := hpresent
  have hpred : keyPred st b k j = true := keyPred_true_of_loc hloc hkey
  obtain ⟨j0, hfm⟩ := firstMatch_some_of_exists (p := keyPred st b k)
    ⟨j, hjb, hpred⟩
  obtain ⟨e0, hloc0, _⟩ := keyPred_true_spec (firstMatch_found hfm).2
  refine ⟨j0, e0, ?_⟩
  unfold findIn
  rw [hfm]
  simp [hloc0]

/-! ## Uniqueness corollaries -/

/-- Under `Inv`, a key stored at one location cannot be found anywhere else: a hit
elsewhere would put two copies of one key in the scanned region. -/
theorem findIn_none_of_unique_elsewhere [DecidableEq K] {st : MapState K V}
    (inv : Inv st) {b j : Nat} {e : K × V} (hjb : j < SlotsPerBucket)
    (hloc : st.buckets b j = some e) {c : Nat} (hcne : c ≠ b) :
    findIn st c e.1 = none := by
  cases hm : findIn st c e.1 with
  | none => rfl
  | some w =>
      obtain ⟨j0, e0⟩ := w
      obtain ⟨hj0, hloc0, hkey0⟩ := findIn_some_spec hm
      obtain ⟨hb_eq, _⟩ := inv.unique c j0 e0 b j e hj0 hjb hloc0 hloc hkey0
      exact absurd hb_eq hcne

/-! ## `get` -/

/-- Final logical effect of `get` (map.rs L860-L862 via L1251-L1258): search the first
candidate, then the second when distinct. The read-consistency epoch dance is
stage-5 material. -/
def opGet [DecidableEq K] (st : MapState K V) (k : K) : Option V :=
  match findIn st (cand1 st k) k with
  | some (_, (_, v)) => some v
  | none =>
      if cand1 st k = cand2 st k then none
      else (findIn st (cand2 st k) k).map (fun w => w.2.2)

theorem opGet_some [DecidableEq K] {st : MapState K V} {k : K} {j : Nat} {kk : K} {vv : V}
    (h : findIn st (cand1 st k) k = some (j, kk, vv)) : opGet st k = some vv := by
  unfold opGet
  rw [h]

theorem opGet_none_eq [DecidableEq K] {st : MapState K V} {k : K}
    (hn : findIn st (cand1 st k) k = none) (hc12 : cand1 st k = cand2 st k) :
    opGet st k = none := by
  unfold opGet
  rw [hn, if_pos hc12]

theorem opGet_none_ne [DecidableEq K] {st : MapState K V} {k : K}
    (hn : findIn st (cand1 st k) k = none) (hc12 : cand1 st k ≠ cand2 st k) :
    opGet st k = (findIn st (cand2 st k) k).map (fun w => w.2.2) := by
  unfold opGet
  rw [hn, if_neg hc12]

theorem opGet_sound [DecidableEq K] {st : MapState K V} {k : K} {v : V}
    (h : opGet st k = some v) :
    ∃ b j e, st.buckets b j = some e ∧ e.1 = k ∧ e.2 = v := by
  cases hf1 : findIn st (cand1 st k) k with
  | some w =>
      obtain ⟨j, kk, vv⟩ := w
      obtain ⟨_, hloc, hkey⟩ := findIn_some_spec hf1
      have hop := opGet_some hf1
      rw [hop] at h
      injection h with hv
      exact ⟨_, j, _, hloc, hkey, hv⟩
  | none =>
      by_cases hc12 : cand1 st k = cand2 st k
      · have hop := opGet_none_eq hf1 hc12
        rw [hop] at h
        exact absurd h (by simp)
      · cases hf2 : findIn st (cand2 st k) k with
        | none =>
            have hop := opGet_none_ne hf1 hc12
            rw [hop, hf2] at h
            simp at h
        | some w =>
            obtain ⟨j, kk, vv⟩ := w
            have hop := opGet_none_ne hf1 hc12
            obtain ⟨_, hloc, hkey⟩ := findIn_some_spec hf2
            rw [hop, hf2] at h
            simp at h
            exact ⟨_, j, _, hloc, hkey, h⟩

/-- Search completeness under `Inv`: any stored pair is found by the two-choice
lookup, returning exactly its own value. Placement puts every copy inside its own
candidate pair; uniqueness prevents the other candidate from shadowing it. -/
theorem opGet_complete [DecidableEq K] {st : MapState K V} (inv : Inv st)
    {b j : Nat} {kv : K × V} (hjb : j < SlotsPerBucket)
    (hloc : st.buckets b j = some kv) :
    opGet st kv.1 = some kv.2 := by
  obtain ⟨_, _, hcand⟩ := inv.placed b j kv hloc
  have hpresent : ∃ j' e', j' < SlotsPerBucket ∧ st.buckets b j' = some e' ∧ e'.1 = kv.1 :=
    ⟨j, kv, hjb, hloc, rfl⟩
  cases hf1 : findIn st (cand1 st kv.1) kv.1 with
  | some w =>
      obtain ⟨j0, e0⟩ := w
      have hop := opGet_some hf1
      obtain ⟨hj0, hloc0, hkey0⟩ := findIn_some_spec hf1
      obtain ⟨hb_eq, hj_eq⟩ := inv.unique (cand1 st kv.1) j0 e0 b j kv hj0 hjb
        hloc0 hloc hkey0
      rw [hb_eq, hj_eq] at hloc0
      have he0 : e0 = kv := Option.some.inj (hloc0.symm.trans hloc)
      simp [hop, he0]
  | none =>
      rcases hcand with hb | hb
      · exfalso
        rw [hb] at hpresent
        obtain ⟨_j0, _e0, hf2⟩ := findIn_some_of_present hpresent
        rw [hf1] at hf2
        contradiction
      · by_cases hc12 : cand1 st kv.1 = cand2 st kv.1
        · exfalso
          rw [hb, ← hc12] at hpresent
          obtain ⟨_j0, _e0, hf2⟩ := findIn_some_of_present hpresent
          rw [hf1] at hf2
          contradiction
        · have hop := opGet_none_ne hf1 hc12
          rw [hb] at hpresent
          obtain ⟨j0, e0, hf2⟩ := findIn_some_of_present hpresent
          obtain ⟨hj0, hloc0, hkey0⟩ := findIn_some_spec hf2
          obtain ⟨hb_eq, hj_eq⟩ := inv.unique (cand2 st kv.1) j0 e0 b j kv hj0 hjb
            hloc0 hloc hkey0
          rw [hb_eq, hj_eq] at hloc0
          have he0 : e0 = kv := Option.some.inj (hloc0.symm.trans hloc)
          simp [hop, hf2, he0]

end Lhm.Abs
