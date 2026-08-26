/-
Stage 5: mutation-epoch fencing, failure atomicity, write-before-publish
ordering, and the insert retry-loop progress measure.

Modeled here (each definition cites its Rust source at audit target revision
`0da342d62`):

1. The even/odd mutation fence as explicit `MapState` transitions:
   `beginMutationAt` opens odd (map.rs L1229-L1243), `guardFinish` closes even
   (map.rs L375-L383, control.rs L134-L136), `entryGate` is the parity check
   every mutating and reading entry point performs first (map.rs L892-L894,
   L973-L975, L607-L609, L757-L759, L689-L691; readers via
   `read_consistent_hot` L1203-L1219 over `read_hot_with_epoch`,
   control.rs L96-L116).
2. Failure atomicity per ADR 0067: call results carry the store in the failure
   branch too, so "a prewrite error leaves logical bytes and the even epoch
   unchanged" is a contentful equation, not a vacuity. Every fallible step of
   the modeled paths precedes the first write (map.rs L946, L999, L1663-L1664,
   L716, L776), which is exactly what the atomicity theorems reflect.
3. The byte-layer surface: `RawStore` vs `publishedView`, clear/reset zeroing
   only the initial blocks (map.rs L721-L724, L781-L784) while stale bytes
   persist physically (REPORT.md finding 4), and `apply_split`'s two-phase
   commit — complete block images before publishing growth (map.rs
   L1665-L1675). Stages 3-4 model only the published view; this section shows
   that abstraction is sound.
4. The insert retry-loop progress measure (REPORT.md finding 5): each
   maintenance split publishes `physical_buckets + 1`, and `remSplits` counts
   down to the geometry cap where `nextGeometry` fails closed.
-/

import Lhm.Abs.Split

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Error taxonomy and call results -/

/-- Error kinds reachable on the modeled mutation paths. Mirrors the relevant
variants of `MutationError` (map.rs L44-L76): `InProgress`, `EpochExhausted`,
the grow errors (`OutOfMemory` / `CapacityOverflow` via `map_grow_error`,
map.rs L1723-L1728), and plain `CapacityOverflow`. `TablePressure` arises in
`plan_split` (L1482, L1490, L1608) strictly before any write; it is carried for
taxonomy completeness. -/
inductive MutErr where
  | inProgress
  | epochExhausted
  | outOfMemory
  | capacityOverflow
  | tablePressure

/-- Outcome of one modeled call: the error kind, if any, **together with the
store exactly as the call leaves it**. Carrying the store through failures is
what makes failure atomicity a contentful statement — an unfaithful
transcription could return a half-written store on an error path, and the
atomicity theorems below forbid it. -/
inductive CallResult (K V : Type) where
  | ok (state : MapState K V) : CallResult K V
  | fail (err : MutErr) (state : MapState K V) : CallResult K V

/-! ## The mutation fence (map.rs L1221-L1249, L333-L336, L375-L383) -/

/-- u64 ceiling for the epoch counter: the `checked_add(2)` at map.rs L1237
fails above it (`EpochExhausted`). Per SCOPE.md A3 the model keeps exact
naturals and models that overflow as this explicit bound. -/
def U64Max : Nat := 2 ^ 64 - 1

/-- Faithful transcription of `begin_mutation_at` (map.rs L1229-L1243):
quiescence re-read (`idle_epoch`, L1221-L1227), observed-epoch match, u64
headroom — in that order — then the single write flipping the persisted epoch
odd (`write_mutation_epoch`, control.rs L134-L136). All three checks precede
the write, so every failure branch carries the store untouched. -/
def beginMutationAt (st : MapState K V) (observed : Nat) : CallResult K V :=
  if st.mutationEpoch % 2 = 0 then
    if st.mutationEpoch = observed then
      if st.mutationEpoch + 2 ≤ U64Max then
        .ok { st with mutationEpoch := st.mutationEpoch + 1 }
      else
        .fail .epochExhausted st
    else
      .fail .inProgress st
  else
    .fail .inProgress st

/-- Mirror of `MutationGuard::finish` (struct map.rs L333-L336, method
L375-L383): close the fence by writing the completed epoch. The completed epoch
is always `observed + 2`, so a committed mutation advances the persisted epoch
by exactly 2 — the convention stages 3-4 baked into their transformers. -/
def guardFinish (st : MapState K V) (completed : Nat) : MapState K V :=
  { st with mutationEpoch := completed }

/-- Entry-point parity gate shared by insert (map.rs L892-L894), remove
(L973-L975), maintenance_step (L607-L609), clear (L757-L759), reset
(L689-L691) — and by readers, since `read_consistent_hot` (L1203-L1219)
rejects odd epochs read through `read_hot_with_epoch` (control.rs L96-L116).
An odd persisted epoch — the footprint of an interrupted guard window — rejects
with `InProgress` before any inspection or write. Open-time validation rejects
it as `RecoveryRequired` (validate_control, map.rs L1048-L1050); that is
`ValidControl` conjunct 8 / `Inv.geomEpochEven`. -/
def entryGate (st : MapState K V) : CallResult K V :=
  if st.mutationEpoch % 2 = 0 then .ok st else .fail .inProgress st

/-- Guarded commit of a logical transform `body`: open the fence at the current
even epoch, apply `body` to the fenced state, close at `observed + 2`. This is
the shared skeleton of the insert branches (map.rs L911-L913, L946-L950),
remove (L999-L1003), apply_split (L1664-L1676), clear (L776-L799), and reset
(L716-L739; both inline their fence instead of calling `begin_mutation_at`,
with identical open/close semantics). Every fallible step on those paths
precedes the fence-open; the commit tail is total writes. -/
def runGuarded (body : MapState K V → MapState K V) (st : MapState K V) :
    CallResult K V :=
  match beginMutationAt st st.mutationEpoch with
  | .ok mid => .ok (guardFinish (body mid) (mid.mutationEpoch + 1))
  | .fail e s' => .fail e s'

/-! ## Guard behavior -/

theorem begin_mutation_at_fail_id {st : MapState K V} {observed : Nat}
    {e : MutErr} {s' : MapState K V} (h : beginMutationAt st observed = .fail e s') :
    s' = st := by
  unfold beginMutationAt at h
  split at h
  · split at h
    · split at h
      · exact absurd h (by simp)
      · exact (CallResult.fail.inj h).2.symm
    · exact (CallResult.fail.inj h).2.symm
  · exact (CallResult.fail.inj h).2.symm

/-- A successful fence-open pins the full protocol state: quiescent even epoch,
matched observation, u64 headroom, and a fenced store differing only in the
epoch. -/
theorem begin_mutation_at_ok {st : MapState K V} {observed : Nat}
    {mid : MapState K V} (h : beginMutationAt st observed = .ok mid) :
    st.mutationEpoch % 2 = 0 ∧ st.mutationEpoch = observed ∧
      st.mutationEpoch + 2 ≤ U64Max ∧
      mid = { st with mutationEpoch := st.mutationEpoch + 1 } := by
  unfold beginMutationAt at h
  split at h
  · rename_i heven
    split at h
    · rename_i hobs
      split at h
      · exact ⟨heven, hobs, ‹_›, (CallResult.ok.inj h).symm⟩
      · exact absurd h (by simp)
    · exact absurd h (by simp)
  · exact absurd h (by simp)

theorem entry_gate_odd_fails {st : MapState K V} (hodd : st.mutationEpoch % 2 = 1) :
    entryGate st = .fail .inProgress st := by
  unfold entryGate
  rw [if_neg (by omega)]

theorem entry_gate_even_ok {st : MapState K V} (heven : st.mutationEpoch % 2 = 0) :
    entryGate st = .ok st := by
  unfold entryGate
  rw [if_pos heven]

/-- **Failure atomicity, guard-scoped (ADR 0067)**: when a guarded commit fails,
the reported store *is* the input — logical bytes, counters, incarnation, and
the even epoch included. In the Rust paths this holds because every fallible
step (`?` on `begin_mutation_at`) precedes the first write. -/
theorem run_guarded_fail_atomic {body : MapState K V → MapState K V}
    {st : MapState K V} {e : MutErr} {s' : MapState K V}
    (h : runGuarded body st = .fail e s') : s' = st := by
  unfold runGuarded at h
  cases hb : beginMutationAt st st.mutationEpoch with
  | ok mid => rw [hb] at h; exact absurd h (by simp)
  | fail e' s'' =>
      rw [hb] at h
      have hs := (CallResult.fail.inj h).2
      exact hs.symm.trans (begin_mutation_at_fail_id hb)

/-- Shape of a successful guarded commit: the fence was openable (even observed
epoch) and the result closes at `observed + 2` — written absolutely by the
closing `write_mutation_epoch`, clobbering whatever epoch the body carried. -/
theorem run_guarded_ok {body : MapState K V → MapState K V}
    {st s' : MapState K V} (h : runGuarded body st = .ok s') :
    st.mutationEpoch % 2 = 0 ∧
      s' = guardFinish (body { st with mutationEpoch := st.mutationEpoch + 1 })
        (st.mutationEpoch + 2) := by
  unfold runGuarded at h
  cases hb : beginMutationAt st st.mutationEpoch with
  | fail e' s'' => rw [hb] at h; exact absurd h (by simp)
  | ok mid =>
      rw [hb] at h
      obtain ⟨heven, _, _, hmid⟩ := begin_mutation_at_ok hb
      subst hmid
      have hseq := CallResult.ok.inj h
      subst hseq
      refine ⟨heven, ?_⟩
      rfl

/-- A committed mutation restores quiescence: the persisted epoch lands even,
advanced by exactly 2 from the observed one. -/
theorem run_guarded_ok_epoch {body : MapState K V → MapState K V}
    {st s' : MapState K V} (h : runGuarded body st = .ok s') :
    s'.mutationEpoch = st.mutationEpoch + 2 ∧ s'.mutationEpoch % 2 = 0 := by
  obtain ⟨heven, hshape⟩ := run_guarded_ok h
  have h1 : s'.mutationEpoch = st.mutationEpoch + 2 := by
    rw [hshape]; rfl
  exact ⟨h1, by rw [h1]; omega⟩

/-! ## Realized transforms (stages 3-4 recomposed under the fence)

Each committed transformer of `Ops.lean` / `Split.lean` bakes in the net "+2"
epoch convention. The guarded protocol reproduces them exactly: the fenced
intermediate state is invisible off the epoch (`begin_mutation_at` touches
nothing else), and the closing write sets the epoch absolutely. So stage 5 adds
ordering and atomicity without disturbing any stage-3/4 contract. -/

/-- Insert update branch (map.rs L907-L914) realizes stage-3 `setValue`. -/
theorem run_guarded_setValue_realizes (st : MapState K V) (b j : Nat) (k : K) (v : V)
    (h : runGuarded (fun s => setValue s b j k v) st = .ok s') :
    s' = setValue st b j k v := by
  obtain ⟨_, hshape⟩ := run_guarded_ok h
  subst hshape
  rfl

/-- Insert place branch (map.rs L931-L951) realizes stage-3 `placeAt`. The
checked counter arithmetic of map.rs L935-L945 is stage-3/A3 material and stays
outside the fence model; its position (before any write) matches
`run_guarded_fail_atomic`. -/
theorem run_guarded_placeAt_realizes (st : MapState K V) (k : K) (v : V) (b j : Nat)
    (h : runGuarded (fun s => placeAt s k v b j) st = .ok s') :
    s' = placeAt st k v b j := by
  obtain ⟨_, hshape⟩ := run_guarded_ok h
  subst hshape
  rfl

/-- Remove hit branch (map.rs L982-L1004) realizes stage-3 `clearSlot`; the
absent-key path re-affirms the epoch without opening a fence (`ensure_epoch`,
map.rs L979, L1245-L1249) and leaves the store untouched, as in `opRemove`. -/
theorem run_guarded_clearSlot_realizes (st : MapState K V) (b j : Nat)
    (h : runGuarded (fun s => clearSlot s b j) st = .ok s') :
    s' = clearSlot st b j := by
  obtain ⟨_, hshape⟩ := run_guarded_ok h
  subst hshape
  rfl

/-- `clear` (map.rs L751-L801) realizes stage-3 `clearedState`: gate and
headroom checks are prewrite (L757-L775), the inline fence opens at L776-L780,
the initial blocks zero at L781-L784, and the new control publishes at
L785-L799 carrying the completed epoch. -/
theorem run_guarded_clearedState_realizes (st : MapState K V)
    (h : runGuarded clearedState st = .ok s') : s' = clearedState st := by
  obtain ⟨_, hshape⟩ := run_guarded_ok h
  subst hshape
  rfl

/-- `reset` (map.rs L683-L741) realizes stage-3 `resetState`: parity gate,
incarnation fence (L692-L696), headroom and extent room (L697-L715) are all
prewrite; the inline fence opens at L716-L720. -/
theorem run_guarded_resetState_realizes (st : MapState K V)
    (h : runGuarded resetState st = .ok s') : s' = resetState st := by
  obtain ⟨_, hshape⟩ := run_guarded_ok h
  subst hshape
  rfl

/-! ## apply_split: prewrite growth error (map.rs L1662-L1678) -/

/-- Raw-capacity growth (`grow_to_bytes`, memory.rs L11-L26; called at
apply_split L1663 BEFORE the fence opens). Growth touches neither control bytes
nor block content, so success is the identity at this layer; failure surfaces
through `map_grow_error` (map.rs L1723-L1728). Modeled with an explicit room
bit so the failure's prewrite position stays visible. -/
def growTo (st : MapState K V) (room : Bool) : CallResult K V :=
  if room then .ok st else .fail .outOfMemory st

/-- Faithful transcription of `apply_split` (map.rs L1662-L1678): grow (L1663),
open the fence (L1664), write every planned block image completely
(L1665-L1667), publish the new geometry (L1668-L1675), close the fence (L1676).
All fallible steps precede the block writes. -/
def applySplitCall (st : MapState K V) (g : Geometry) (room : Bool) : CallResult K V :=
  match growTo st room with
  | .ok s1 => runGuarded (fun s => splitState s g) s1
  | .fail e s1 => .fail e s1

/-- Failure atomicity for the split pipeline covers both prewrite sources: the
grow error and the fence-open rejections. Either way the store comes back
exactly as it went in — the ADR 0067 claim for splits. -/
theorem apply_split_call_fail_atomic (st : MapState K V) (g : Geometry) (room : Bool)
    {e : MutErr} {s' : MapState K V} (h : applySplitCall st g room = .fail e s') :
    s' = st := by
  unfold applySplitCall at h
  cases hr : growTo st room with
  | ok s1 =>
      rw [hr] at h
      have hf := run_guarded_fail_atomic h
      unfold growTo at hr
      by_cases hroom : room
      · rw [if_pos hroom] at hr
        exact hf.trans (CallResult.ok.inj hr).symm
      · rw [if_neg hroom] at hr
        exact absurd hr (by simp)
  | fail e' s1 =>
      rw [hr] at h
      have hs := (CallResult.fail.inj h).2
      unfold growTo at hr
      by_cases hroom : room
      · rw [if_pos hroom] at hr
        exact absurd hr (by simp)
      · rw [if_neg hroom] at hr
        exact ((CallResult.fail.inj hr).2.trans hs).symm

/-- A successful split realizes the stage-4 published state exactly, with the
fence closed on top: `splitState` inherits the epoch (stage 4 models the
published control, leaving the net "+2" convention to stage 3), so the realized
state is `splitState` with the completed epoch written — precisely the order
apply_split publishes in (block images L1665-L1667, then control L1668-L1675,
then the fence closes at L1676). -/
theorem apply_split_call_ok_realizes (st : MapState K V) (g : Geometry) (room : Bool)
    {s' : MapState K V} (h : applySplitCall st g room = .ok s') :
    st.mutationEpoch % 2 = 0 ∧
      s' = { splitState st g with mutationEpoch := st.mutationEpoch + 2 } := by
  unfold applySplitCall at h
  cases hr : growTo st room with
  | fail e' s1 => rw [hr] at h; exact absurd h (by simp)
  | ok s1 =>
      rw [hr] at h
      have hs1 : s1 = st := by
        unfold growTo at hr
        by_cases hroom : room
        · rw [if_pos hroom] at hr; exact (CallResult.ok.inj hr).symm
        · rw [if_neg hroom] at hr; exact absurd hr (by simp)
      subst hs1
      obtain ⟨heven, hshape⟩ := run_guarded_ok h
      refine ⟨heven, ?_⟩
      rw [hshape]
      rfl

/-! ## Cross-message fencing (SCOPE.md assumption A2)

Odd epochs arise only between fence-open and fence-close inside one message.
If execution is interrupted there, the persisted epoch stays odd and the fence
then blocks every later entry point with `InProgress`
(`entry_gate_odd_fails`) until recovery, while reopen validation rejects the
record outright with `RecoveryRequired` (validate_control L1048-L1050, i.e.
`ValidControl` conjunct 8). Readers are gated identically
(`read_consistent_hot`, map.rs L1203-L1219), so no torn state is ever served;
recovery restarts from the last even epoch.

Committed operations restore quiescence by construction:
`run_guarded_ok_epoch` shows every successful call lands on an even epoch, so
an odd epoch can never be published by a completed message. -/

/-! ## Byte-layer surface: write-before-publish (REPORT.md finding 4)

Stages 3-4 model the logical surface only. This section names the two layers:
`RawStore` is the physical image abstracted to flattened slots with no extent
clamp — addresses past the published control still hold history — while
`publishedView` clamps reads to the published extent, exactly what
`scan_physical_window` (map.rs L1088-L1119) and `entries_from_image`
(map.rs L1787-L1806) enumerate. -/

/-- Physical stored bytes, indexed by `(bucket, flattened slot)`, unclamped.
An `abbrev` so applications stay first-class during rewriting. -/
abbrev RawStore (K V : Type) := Nat → Nat → Option (K × V)

/-- Logical surface a reader observes under a published control. -/
def publishedView (σ : RawStore K V) (pb : Nat) : Nat → Nat → Option (K × V) :=
  fun b j => if b < pb ∧ j < SlotsPerBucket then σ b j else none

theorem published_view_inside {σ : RawStore K V} {pb b j : Nat} (hb : b < pb)
    (hj : j < SlotsPerBucket) : publishedView σ pb b j = σ b j := by
  unfold publishedView
  exact if_pos ⟨hb, hj⟩

theorem published_view_outside {σ : RawStore K V} {pb b j : Nat} (hb : pb ≤ b) :
    publishedView σ pb b j = none := by
  unfold publishedView
  exact if_neg (fun hc => absurd hc.1 (by omega))

/-- clear/reset zero only the first INITIAL_BUCKETS blocks (map.rs L721-L724,
L781-L784): bytes beyond survive physically. -/
def zeroInitialBlocks (σ : RawStore K V) : RawStore K V :=
  fun b j => if b < 2 ^ InitialLevel ∧ j < SlotsPerBucket then none else σ b j

/-- Finding 4's premise: clearing leaves physical bytes untouched outside the
initial extent. -/
theorem zero_initial_blocks_keeps_stale (σ : RawStore K V) (b j : Nat)
    (hb : 2 ^ InitialLevel ≤ b) : zeroInitialBlocks σ b j = σ b j := by
  unfold zeroInitialBlocks
  exact if_neg (fun hc => absurd hc.1 (by omega))

/-- Finding 4, positive half: after clear, the published surface is empty
everywhere — precisely stage 3's `clearedState.buckets = fun _ _ => none`.
Stale bytes beyond the initial extent persist but cannot surface, because
reads stop at the published extent. -/
theorem cleared_published_empty (σ : RawStore K V) (b j : Nat) :
    publishedView (zeroInitialBlocks σ) (2 ^ InitialLevel) b j = none := by
  unfold publishedView zeroInitialBlocks
  by_cases h : b < 2 ^ InitialLevel ∧ j < SlotsPerBucket
  · rw [if_pos h, if_pos h]
  · rw [if_neg h]

/-- apply_split's commit (map.rs L1665-L1675): complete block images land first
(L1665-L1667); the grown geometry publishes afterwards (L1668-L1675). `images`
is the partial map of rewritten blocks, each a complete block image. The
published extent itself lives in the control region, not in `RawStore`, hence
the underscore. -/
def writeBlocksThenPublish (σ : RawStore K V)
    (images : Nat → Option (Nat → Option (K × V))) (_newPb : Nat) : RawStore K V :=
  fun b j =>
    match images b with
    | some img => if j < SlotsPerBucket then img j else σ b j
    | none => σ b j

theorem writeBlocksThenPublish_rewritten (σ : RawStore K V)
    (images : Nat → Option (Nat → Option (K × V))) (newPb b j : Nat)
    (img : Nat → Option (K × V)) (h : images b = some img) (hj : j < SlotsPerBucket) :
    writeBlocksThenPublish σ images newPb b j = img j := by
  unfold writeBlocksThenPublish
  rw [h]
  show (if j < SlotsPerBucket then img j else σ b j) = img j
  exact if_pos hj

theorem writeBlocksThenPublish_kept (σ : RawStore K V)
    (images : Nat → Option (Nat → Option (K × V))) (newPb b j : Nat)
    (h : images b = none) :
    writeBlocksThenPublish σ images newPb b j = σ b j := by
  unfold writeBlocksThenPublish
  rw [h]

/-- **Finding 4, encoded**: under the newly published extent, every readable
slot either lies in a rewritten block (its complete image) or coincides with
the old published surface. The coverage hypothesis is exactly
write-before-publish: the only extent growth `nextGeometry` permits is +1
(map.rs L1693-L1709), and `apply_split` rewrites the newly exposed bucket — the
split's new destination — before publishing it (plan_split builds both images
at L1465-L1491, apply_split writes them at L1665-L1667 ahead of L1668-L1675).
Without the rewrite, slots in `newPb \ oldPb` would surface stale history. -/
theorem write_before_publish (σ : RawStore K V)
    (images : Nat → Option (Nat → Option (K × V))) (oldPb newPb b j : Nat)
    (hbn : b < newPb) (hjn : j < SlotsPerBucket)
    (hcover : ∀ c, oldPb ≤ c → c < newPb → images c ≠ none) :
    publishedView (writeBlocksThenPublish σ images newPb) newPb b j =
      match images b with
      | some img => img j
      | none => publishedView σ oldPb b j := by
  refine Eq.trans (published_view_inside (σ := writeBlocksThenPublish σ images newPb)
    (pb := newPb) (b := b) (j := j) hbn hjn) ?_
  cases himg : images b with
  | some img =>
      exact writeBlocksThenPublish_rewritten σ images newPb b j img himg hjn
  | none =>
      have hb_old : b < oldPb := by
        rcases Nat.lt_or_ge b oldPb with hlt | hge
        · exact hlt
        · exact absurd himg (hcover b hge hbn)
      exact Eq.trans (writeBlocksThenPublish_kept σ images newPb b j himg)
        (published_view_inside (σ := σ) (pb := oldPb) (b := b) (j := j) hb_old hjn).symm

/-! ## Insert retry-loop progress measure (REPORT.md finding 5) -/

theorem two_pow_lt_two_pow {a b : Nat} (hlt : a < b) : 2 ^ a < 2 ^ b := by
  induction b with
  | zero => exact absurd hlt (by omega)
  | succ n ih =>
      rcases Nat.lt_or_ge a n with hlt2 | hge2
      · have hstep : 2 ^ n < 2 ^ (n + 1) := by
          rw [Nat.pow_succ, Nat.mul_two]
          have hpos := two_pow_pos n
          omega
        exact Nat.lt_trans (ih hlt2) hstep
      · have haeq : a = n := by omega
        rw [haeq, Nat.pow_succ, Nat.mul_two]
        have hpos := two_pow_pos n
        omega

theorem two_pow_le_two_pow {a b : Nat} (hle : a ≤ b) : 2 ^ a ≤ 2 ^ b := by
  rcases Nat.lt_or_ge a b with hlt | hge
  · exact Nat.le_of_lt (two_pow_lt_two_pow hlt)
  · have haeq : a = b := Nat.le_antisymm hle hge
    rw [haeq]
    exact Nat.le_refl _

private theorem pow62_add_pow62 : 2 ^ 62 + 2 ^ 62 = 2 ^ 63 := by
  have h63 : (63 : Nat) = 62 + 1 := rfl
  rw [h63, Nat.pow_succ, Nat.mul_two]

/-- Remaining successful `nextGeometry` steps before fail-closed exhaustion,
from geometry `(level, cursor)` with `cursor < 2^level`. Closed form
`2^63 − (2^level + cursor)`; under the control equation this is
`2^63 − physical_buckets` — the monotone bucket-count measure consumed by the
insert retry loop (map.rs L886-L964: each maintenance split publishes
`buckets + 1`). -/
def remSplits (level cursor : Nat) : Nat := 2 ^ 63 - (2 ^ level + cursor)

theorem rem_splits_step_adv {L cur : Nat} (hstep : cur + 1 < 2 ^ L) (hL : L < 63) :
    remSplits L (cur + 1) + 1 = remSplits L cur := by
  have hA : 2 ^ L ≤ 2 ^ 62 := two_pow_le_two_pow (by omega)
  have h2 : 2 ^ 62 + 2 ^ 62 = 2 ^ 63 := pow62_add_pow62
  unfold remSplits
  omega

theorem rem_splits_step_up {L cur : Nat} (hfront : cur + 1 = 2 ^ L) (hl : L + 1 < 63) :
    remSplits (L + 1) 0 + 1 = remSplits L cur := by
  have hsucc : 2 ^ (L + 1) = 2 ^ L + 2 ^ L := by
    rw [Nat.pow_succ, Nat.mul_two]
  have hQ : 2 ^ (L + 1) ≤ 2 ^ 62 := two_pow_le_two_pow (by omega)
  have h2 : 2 ^ 62 + 2 ^ 62 = 2 ^ 63 := pow62_add_pow62
  unfold remSplits
  omega

/-- One successful geometry step consumes exactly one unit of budget. -/
theorem next_geometry_rem_splits {L cur pb : Nat} {g : Geometry}
    (hcur : cur < 2 ^ L) (hg : nextGeometry L cur pb = some g) :
    remSplits g.level g.cursor + 1 = remSplits L cur := by
  rw [nextGeometry] at hg
  split at hg
  · rename_i hL
    split at hg
    · rename_i hfront
      split at hg
      · cases hg
        rename_i hl
        show remSplits (L + 1) 0 + 1 = remSplits L cur
        exact rem_splits_step_up hfront hl
      · simp at hg
    · cases hg
      rename_i hnfront
      show remSplits L (cur + 1) + 1 = remSplits L cur
      exact rem_splits_step_adv (by omega) hL
  · simp at hg

/-- One retry-loop iteration's geometry transition: `insert` plans a split from
the freshly read control, `apply_split` publishes `buckets + 1`, and the loop
re-reads (map.rs L886-L964, L953-L963). The pair is
`(level, split_cursor)`; the bucket count is derived by the control equation. -/
def stepGeom (p : Nat × Nat) : Option (Nat × Nat) :=
  match nextGeometry p.1 p.2 (2 ^ p.1 + p.2) with
  | some g => some (g.level, g.cursor)
  | none => none

/-- Iterated retry-loop geometry chains. -/
inductive GeomChain : Nat → Nat → Nat → Prop where
  | done (L cur : Nat) : GeomChain 0 L cur
  | more (N L cur l' c' : Nat) (hstep : stepGeom (L, cur) = some (l', c'))
      (hrest : GeomChain N l' c') : GeomChain (N + 1) L cur

/-- Finding 5, as a lemma: any chain of successful maintenance splits from
`(level, cursor)` has length at most `remSplits level cursor` — the measure
strictly decreases along the chain and cannot pass zero. -/
theorem geom_chain_bounded : ∀ (N L cur : Nat), cur < 2 ^ L →
    GeomChain N L cur → N ≤ remSplits L cur := by
  intro N
  induction N with
  | zero => intro L cur _ _; exact Nat.zero_le _
  | succ N ih =>
      intro L cur hcur hchain
      cases hchain with
      | more _ _ _ l' c' hstep hrest =>
          unfold stepGeom at hstep
          cases hx : nextGeometry L cur (2 ^ L + cur) with
          | none =>
              rw [hx] at hstep
              exact absurd hstep (by simp)
          | some g =>
              rw [hx] at hstep
              have hpair := Option.some.inj hstep
              rw [Prod.mk.injEq] at hpair
              obtain ⟨hgl, hgc⟩ := hpair
              subst hgl
              subst hgc
              have hshape := next_geometry_shape L cur (2 ^ L + cur) hcur g hx
              have hmeas := next_geometry_rem_splits hcur hx
              have ihn := ih g.level g.cursor hshape.1 hrest
              omega

/-- Beyond the budget there is no chain: the loop must exit through the
fail-closed branch instead. -/
theorem retry_loop_terminates (L cur : Nat) (hcur : cur < 2 ^ L) :
    ¬ GeomChain (remSplits L cur + 1) L cur := by
  intro hchain
  have h := geom_chain_bounded _ L cur hcur hchain
  omega

/-- At exhaustion the geometry step fails closed, ending the loop with an
error return rather than another iteration. -/
theorem rem_splits_zero_fails {L cur pb : Nat} (hcur : cur < 2 ^ L)
    (h0 : remSplits L cur = 0) : nextGeometry L cur pb = none := by
  have hbig : 2 ^ 63 ≤ 2 ^ L + cur := by unfold remSplits at h0; omega
  by_cases hL : L < 63
  · exfalso
    have h1 : 2 ^ L < 2 ^ 63 := two_pow_lt_two_pow hL
    have h2 : 2 ^ L ≤ 2 ^ 62 := two_pow_le_two_pow (by omega)
    have h3 : 2 ^ 62 + 2 ^ 62 = 2 ^ 63 := pow62_add_pow62
    have h4 : cur < 2 ^ L := hcur
    omega
  · rw [nextGeometry, if_neg (by omega)]

/-- Bucket-count form of the measure: under the control equation the remaining
budget is exactly `2^63 − physical_buckets`. -/
theorem rem_splits_by_buckets (L cur pb : Nat) (hp : pb = 2 ^ L + cur) :
    remSplits L cur = 2 ^ 63 - pb := by
  unfold remSplits
  rw [hp]

end Lhm.Abs
