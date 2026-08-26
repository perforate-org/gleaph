/-
Stage 3 transfer principle (scope Stage 3).

Bridges concrete slot contents and the abstract observable: logical position `p`
reads physical slot `(head + p) % cap` (vec_deque.rs L355-L359 `physical_index`),
and the observable reading list is exactly those readings for `p ∈ [0, len)`.
Under `Inv.occupied` every listed reading is `some e`, so this list is the deque's
element sequence (A5-analog byte-layer consistency).

Everything here is core Lean only, so the list accessors `readAt` / `flatRead` and
the updater `putAt` are defined locally instead of relying on stdlib API surface.
-/
import Svd.Abs.State

namespace Svd.Abs

variable {α : Type}

/-! ## Core-only list toolkit -/

/-- `l.readAt p` is the element of `l` at position `p`, or `none` out of range. -/
def readAt : List α → Nat → Option α
  | [], _ => none
  | a :: _, 0 => some a
  | _ :: t, n + 1 => readAt t n

theorem readAt_cons_zero (a : α) (l : List α) : readAt (a :: l) 0 = some a := rfl

theorem readAt_cons_succ (a : α) (l : List α) (n : Nat) :
    readAt (a :: l) (n + 1) = readAt l n := rfl

/-- Point update of a list at position `p` (no-op past the end). -/
def putAt : List α → Nat → α → List α
  | [], _, _ => []
  | _ :: t, 0, v => v :: t
  | a :: t, n + 1, v => a :: putAt t n v

theorem putAt_length : ∀ (l : List α) (p : Nat) (v : α), (putAt l p v).length = l.length := by
  intro l
  induction l with
  | nil => intro p _; cases p <;> rfl
  | cons _ t ih =>
      intro p v
      cases p with
      | zero => rfl
      | succ m =>
          show (putAt t m v).length + 1 = t.length + 1
          rw [ih m v]

/-- Updating position `p` makes it read `v`. -/
theorem readAt_putAt_self : ∀ (l : List α) (p : Nat) (v : α), p < l.length →
    readAt (putAt l p v) p = some v := by
  intro l
  induction l with
  | nil => intro p _ hv; exact absurd hv (Nat.not_lt_zero p)
  | cons a t ih =>
      intro p v hv
      cases p with
      | zero => exact readAt_cons_zero v t
      | succ m => exact ih m v (Nat.le_of_succ_le_succ hv)

/-- Updating position `p` leaves every other position untouched. -/
theorem readAt_putAt_ne : ∀ (l : List α) (p : Nat) (v : α) (q : Nat), q ≠ p →
    readAt (putAt l p v) q = readAt l q := by
  intro l
  induction l with
  | nil => intro _ _ q _; cases q <;> rfl
  | cons a t ih =>
      intro p v q hqp
      cases p with
      | zero =>
          cases q with
          | zero => exact absurd rfl hqp
          | succ m => exact readAt_cons_succ a t m
      | succ m =>
          cases q with
          | zero => exact rfl
          | succ k =>
              show readAt (putAt t m v) k = readAt t k
              exact ih m v k (fun h => hqp (by rw [h]))

/-- Two lists are equal once their lengths and all readings agree. -/
theorem eq_of_length_eq_of_readAt {l1 l2 : List α} (hlen : l1.length = l2.length)
    (hat : ∀ q, readAt l1 q = readAt l2 q) : l1 = l2 := by
  cases l1 with
  | nil =>
      cases l2 with
      | nil => exact rfl
      | cons b t => have := hlen; simp at this
  | cons a s =>
      cases l2 with
      | nil => have := hlen; simp at this
      | cons b t =>
          have h0' : some a = some b := by simpa [readAt_cons_zero] using hat 0
          have hab : a = b := Option.some.inj h0'
          have hlen' : s.length = t.length := by simpa using hlen
          have hrest : s = t :=
            eq_of_length_eq_of_readAt hlen' fun q => by
              simpa [readAt_cons_succ] using hat (q + 1)
          rw [hrest, hab]

/-! ## Flattened reading of option lists -/

/-- Reading an option list flattens one layer: the element itself, or `none` out
of range. This is the accessor the deque contracts are stated with. -/
def flatRead : List (Option α) → Nat → Option α
  | [], _ => none
  | x :: _, 0 => x
  | _ :: t, n + 1 => flatRead t n

theorem flatRead_cons_zero (x : Option α) (l : List (Option α)) :
    flatRead (x :: l) 0 = x := rfl

theorem flatRead_cons_succ (x : Option α) (l : List (Option α)) (n : Nat) :
    flatRead (x :: l) (n + 1) = flatRead l n := rfl

theorem flatRead_nil (p : Nat) : flatRead (α := α) [] p = none := by
  cases p <;> rfl

/-- Appending one entry adds it exactly at the old end position. -/
theorem flatRead_append_last : ∀ (l : List (Option α)) (x : Option α) (p : Nat),
    flatRead (l ++ [x]) p = if p = l.length then x else flatRead l p := by
  intro l
  induction l with
  | nil =>
      intro x p
      cases p with
      | zero => simp [flatRead_cons_zero]
      | succ m => simp [flatRead_nil, flatRead_cons_succ]
  | cons a t ih =>
      intro x p
      cases p with
      | zero => simp [flatRead_cons_zero]
      | succ m =>
          show flatRead (t ++ [x]) m =
            (if m + 1 = t.length + 1 then x else flatRead (a :: t) (m + 1))
          rw [ih x m, flatRead_cons_succ]
          rcases Nat.lt_or_ge m t.length with hlt | hge
          · rw [if_neg (by omega), if_neg (by omega)]
          · rcases Nat.eq_or_lt_of_le hge with heq | hgt
            · subst heq
              rw [if_pos rfl, if_pos rfl]
            · rw [if_neg (by omega), if_neg (by omega)]

/-! ## Observable reading list -/

/-- Slot readings at logical positions `[0, n)` in order, each routed through the
ring (`physical_index`, vec_deque.rs L355-L359). Elements are `Option α`: under
`Inv.occupied` they are all `some e`. -/
def contentUpTo (st : DequeState α) : Nat → List (Option α)
  | 0 => []
  | n + 1 => contentUpTo st n ++ [contentOf st n]

theorem contentUpTo_nil (st : DequeState α) : contentUpTo st 0 = [] := rfl

theorem contentUpTo_step (st : DequeState α) (n : Nat) :
    contentUpTo st (n + 1) = contentUpTo st n ++ [contentOf st n] := rfl

theorem contentUpTo_length (st : DequeState α) : ∀ n, (contentUpTo st n).length = n := by
  intro n
  induction n with
  | zero => rfl
  | succ m ih =>
      show ((contentUpTo st m ++ [contentOf st m]):List (Option α)).length = m + 1
      rw [List.length_append, ih]
      simp

theorem contentUpTo_congr {st s2 : DequeState α} :
    ∀ n, (∀ p, p < n → contentOf st p = contentOf s2 p) →
      contentUpTo st n = contentUpTo s2 n := by
  intro n
  induction n with
  | zero => intro _; rfl
  | succ m ih =>
      intro h
      show contentUpTo st m ++ [contentOf st m] = contentUpTo s2 m ++ [contentOf s2 m]
      rw [ih (fun p hp => h p (Nat.lt_succ_of_lt hp)), h m (Nat.lt_succ_self m)]

theorem contentUpTo_at (st : DequeState α) : ∀ n p,
    flatRead (contentUpTo st n) p = if p < n then contentOf st p else none := by
  intro n
  induction n with
  | zero => intro p; cases p <;> simp [contentUpTo_nil, flatRead_nil]
  | succ m ih =>
      intro p
      rw [contentUpTo_step, flatRead_append_last, contentUpTo_length]
      rcases Nat.lt_or_ge p m with hlt | hge
      · rw [if_neg (by omega : p ≠ m), ih p, if_pos (Nat.lt_succ_of_lt hlt)]
        exact if_pos hlt
      · rcases Nat.lt_or_ge p (m + 1) with hlt2 | hge2
        · -- `m ≤ p < m + 1` forces `p = m`: rewrite the index, not the variable
          have hp : p = m := by omega
          rw [hp, if_pos rfl, if_pos (by omega)]
        · rw [if_neg (by omega : p ≠ m), if_neg (by omega : ¬ p < m + 1), ih p,
            if_neg (by omega)]

/-- The deque's observable content: readings at logical positions `[0, len)`. -/
def logicalList (st : DequeState α) : List (Option α) :=
  contentUpTo st st.len

theorem logicalList_length (st : DequeState α) : (logicalList st).length = st.len :=
  contentUpTo_length _ _

theorem logicalList_at (st : DequeState α) (p : Nat) :
    flatRead (logicalList st) p = if p < st.len then contentOf st p else none :=
  contentUpTo_at st st.len p

/-- Transfer principle: a state update that keeps `len` and changes only finitely
many routed readings produces exactly the updated observable list. -/
theorem logicalList_eq_of_contentOf {st s2 : DequeState α} (hlen : s2.len = st.len)
    (h : ∀ p, p < s2.len → contentOf s2 p = contentOf st p) :
    logicalList s2 = logicalList st := by
  unfold logicalList
  rw [hlen]
  exact contentUpTo_congr _ (fun p hp => h p (by rw [hlen]; exact hp))

/-! ## Flat-level update and comparison -/

/-- Updating position `p` makes it read `x`. -/
theorem flatRead_putAt_self : ∀ (l : List (Option α)) (p : Nat) (x : Option α),
    p < l.length → flatRead (putAt l p x) p = x := by
  intro l
  induction l with
  | nil => intro p _ hx; exact absurd hx (Nat.not_lt_zero p)
  | cons a t ih =>
      intro p x hx
      cases p with
      | zero => exact flatRead_cons_zero x t
      | succ m => exact ih m x (Nat.le_of_succ_le_succ hx)

/-- Updating position `p` leaves every other position untouched. -/
theorem flatRead_putAt_ne : ∀ (l : List (Option α)) (p : Nat) (x : Option α) (q : Nat),
    q ≠ p → flatRead (putAt l p x) q = flatRead l q := by
  intro l
  induction l with
  | nil => intro _ _ q _; exact flatRead_nil q
  | cons a t ih =>
      intro p x q hqp
      cases p with
      | zero =>
          cases q with
          | zero => exact absurd rfl hqp
          | succ m => exact flatRead_cons_succ x t m
      | succ m =>
          cases q with
          | zero => exact rfl
          | succ k =>
              show flatRead (putAt t m x) k = flatRead t k
              exact ih m x k (fun h => hqp (by rw [h]))

/-- Two option lists are equal once their lengths and all flattened readings
agree. -/
theorem eq_of_length_eq_of_flatRead {l1 l2 : List (Option α)}
    (hlen : l1.length = l2.length) (hat : ∀ q, flatRead l1 q = flatRead l2 q) :
    l1 = l2 := by
  cases l1 with
  | nil =>
      cases l2 with
      | nil => exact rfl
      | cons b t => have := hlen; simp at this
  | cons a s =>
      cases l2 with
      | nil => have := hlen; simp at this
      | cons b t =>
          have h0' : some a = some b := by simpa [flatRead_cons_zero] using hat 0
          have hab : a = b := Option.some.inj h0'
          have hlen' : s.length = t.length := by simpa using hlen
          have hrest : s = t :=
            eq_of_length_eq_of_flatRead hlen' fun q => by
              simpa [flatRead_cons_succ] using hat (q + 1)
          rw [hrest, hab]

/-- Reading past a tail shift equals reading one position later. -/
theorem flatRead_tail (l : List (Option α)) (q : Nat) :
    flatRead l.tail q = flatRead l (q + 1) := by
  cases l with
  | nil => cases q <;> rfl
  | cons a t => rw [List.tail_cons, flatRead_cons_succ]

end Svd.Abs
