/-
Stage 4-note resolution: the insert-carrying split variant.

`insert` reaches `plan_split(control, Some((k, v)))` (map.rs L953) only after the
key was found absent from both candidate blocks and no candidate slot is free
(map.rs L907-L930). `plan_split` with `insert = Some` (map.rs L1453-L1557)
re-packs the source block exactly as the maintenance split, then routes the new
key under the stepped geometry and calls `choose_image_location` (map.rs
L1623-L1660): among the two new-geometry candidates — deduplicated, loads taken
from the re-packed images for the source/new destinations and from disk for an
untouched third bucket — every candidate at full capacity is dropped and the
least-loaded one wins, ties to the first. The entry is appended to that image
(`append_entry_to_image`, map.rs L1489-L1490) and `finish_split_plan` runs with
`inserted = true` (len + 1, map.rs L1575-L1578).

Model. Because a re-packed image occupies a prefix of its block
(`packImg_isSome`) and appending lands at the first free slot, the composed
result is literally the plain split followed by a fresh placement:

    splitInsertState st g k v b j = placeAt (splitState st g) k v b j,

where `(b, j)` is chosen by `pickImage` — the least-loaded-admissible chooser
shared with stage 3's insert path (`pickPair`, mirroring map.rs L916-L930 and
L1631-L1652). `loadOf`/`firstFreeIdx` of the post-split state agree with the
re-packed image counts (`loadOf_splitState_eq`, `firstFreeIdx_splitState_eq`),
so `chooseFreeSlot` on the post-split state computes the same `(b, j)` — the
formal bridge between `choose_image_location` and the plain-insert path
(`splitInsert_eq_opInsert`).

Modeling decision (split_debt): as recorded in the stage-4 notes, `Inv` does
not constrain `splitDebt`; the modeled debt therefore follows the stage-3
insert convention (`debt_after_insert`, map.rs L1680-L1691) inherited from
`placeAt` rather than `finish_split_plan`'s saturating-subtract detail
(map.rs L1596-L1602), which remains outside the invariant.
-/

import Lhm.Abs.Split
import Lhm.Abs.Epoch

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## First-match helpers -/

theorem firstMatchAux_eq_some {p : Nat → Bool} :
    ∀ n s i, s ≤ i → i < s + n → (∀ m, s ≤ m → m < i → p m = false) → p i = true →
      firstMatchAux p s n = some i := by
  intro n
  induction n with
  | zero => intro s i _ hi _ _; exact absurd hi (by omega)
  | succ k ih =>
      intro s i hs hi hbefore hat
      rw [firstMatchAux]
      by_cases hp0 : p s = true
      · rw [if_pos hp0]
        have hlt : ¬(s < i) := by
          intro hc
          rw [hbefore s (Nat.le_refl s) hc] at hp0
          exact Bool.noConfusion hp0
        have heq : s = i := Nat.le_antisymm hs (Nat.not_lt.1 hlt)
        rw [heq]
      · rw [if_neg hp0]
        have hslt : s < i := by
          rcases Nat.lt_or_ge s i with hlt' | hge'
          · exact hlt'
          · have heq : s = i := Nat.le_antisymm hs hge'
            rw [← heq] at hat
            exact absurd hat hp0
        have ha : s + 1 ≤ i := by omega
        have hb2 : i < s + 1 + k := by omega
        refine ih (s + 1) i ha hb2 ?_ hat
        intro m hm1 hm2
        rcases Nat.lt_or_ge m (s + 1) with hlt | hge
        · have hmeq : m = s := by omega
          rw [hmeq]
          cases hb : p s with
          | false => rfl
          | true => exact absurd hb hp0
        · exact hbefore m (Nat.le_trans (Nat.le_succ s) hge) hm2

theorem firstMatch_eq_some {p : Nat → Bool} {n i : Nat} (hi : i < n)
    (hbefore : ∀ m, m < i → p m = false) (hat : p i = true) :
    firstMatch p n = some i :=
  firstMatchAux_eq_some n 0 i (Nat.zero_le i) (by omega) (fun m _ hm => hbefore m hm) hat

theorem firstMatch_none_of_all_false {p : Nat → Bool} {n : Nat}
    (h : ∀ i, i < n → p i = false) : firstMatch p n = none := by
  cases hm : firstMatch p n with
  | none => rfl
  | some j =>
      obtain ⟨hj, hpj⟩ := firstMatch_found hm
      rw [h j hj] at hpj
      exact Bool.noConfusion hpj

theorem firstMatch_congr {p q : Nat → Bool} (n : Nat) (h : ∀ i, i < n → p i = q i) :
    firstMatch p n = firstMatch q n := by
  have aux : ∀ m s, (∀ i, s ≤ i → i < s + m → p i = q i) →
      firstMatchAux p s m = firstMatchAux q s m := by
    intro m
    induction m with
    | zero => intro s _; rfl
    | succ k ih =>
        intro s hag
        rw [firstMatchAux, firstMatchAux, hag s (Nat.le_refl s) (by omega)]
        cases hs : q s with
        | true => rfl
        | false => exact ih (s + 1) (fun i h1 h2 => hag i (by omega) (by omega))
  simpa [firstMatch] using aux n 0 (fun i h1 _ => h i (by omega))

theorem findIn_congr [DecidableEq K] {st s2 : MapState K V} {b : Nat} {k : K}
    (h : ∀ j, s2.buckets b j = st.buckets b j) : findIn s2 b k = findIn st b k := by
  unfold findIn
  rw [firstMatch_congr SlotsPerBucket (fun j _ => by
    show keyPred s2 b k j = keyPred st b k j
    simp only [keyPred, h])]
  simp only [h]

theorem firstFreeIdx_congr {st s2 : MapState K V} {b : Nat}
    (h : ∀ j, s2.buckets b j = st.buckets b j) : firstFreeIdx s2 b = firstFreeIdx st b := by
  unfold firstFreeIdx
  refine firstMatch_congr SlotsPerBucket (fun j _ => ?_)
  show (!(occPred s2 b j)) = (!(occPred st b j))
  simp only [occPred, h]

/-- A counted range containing one uncounted position falls short of the range. -/
theorem countMatch_lt_of_exists_false {p : Nat → Bool} :
    ∀ n i, i < n → p i = false → countMatch p n < n := by
  intro n
  induction n with
  | zero => intro i hi _; exact absurd hi (Nat.not_lt_zero i)
  | succ k ih =>
      intro i hi hf
      rcases Nat.lt_or_ge i k with hlt | hge
      · have hcnt := ih i hlt hf
        cases hb : p k with
        | false =>
            rw [countMatch_succ_of_false p k hb]
            have hle := countMatch_le p k
            omega
        | true =>
            rw [countMatch_succ_of_true p k hb]
            omega
      · have hik : i = k := by omega
        rw [hik] at hf
        rw [countMatch_succ_of_false p k hf]
        have hle := countMatch_le p k
        omega

theorem firstFreeIdx_of_prefix {st : MapState K V} {b c : Nat}
    (hocc : ∀ j, occPred st b j = decide (j < c)) (hlt : c < SlotsPerBucket) :
    firstFreeIdx st b = c := by
  unfold firstFreeIdx
  refine firstMatch_eq_some hlt (fun m hm => ?_) ?_
  · rw [hocc m]; simp; omega
  · rw [hocc c]; simp

theorem firstFreeIdx_of_full {st : MapState K V} {b : Nat}
    (hocc : ∀ j, occPred st b j = decide (j < SlotsPerBucket)) : firstFreeIdx st b = none :=
  firstMatch_none_of_all_false (fun i hi => by rw [hocc i]; simp [hi])

/-! ## Pointwise facts about the post-split arrangement -/

theorem splitBuckets_at_src (st : MapState K V) (g : Geometry) (j : Nat) :
    splitBuckets st g st.splitCursor j = splitSrcFun st g j := by
  unfold splitBuckets
  rw [if_pos rfl]

theorem splitBuckets_at_new (st : MapState K V) (g : Geometry) (j : Nat) :
    splitBuckets st g (st.splitCursor + 2 ^ st.level) j = splitNewFun st g j := by
  unfold splitBuckets
  have hne : ¬(st.splitCursor + 2 ^ st.level = st.splitCursor) := by
    have hp := two_pow_pos st.level
    omega
  rw [if_neg hne, if_pos rfl]

theorem splitBuckets_at_else (st : MapState K V) (g : Geometry) {b : Nat}
    (hb1 : b ≠ st.splitCursor) (hb2 : b ≠ st.splitCursor + 2 ^ st.level) (j : Nat) :
    splitBuckets st g b j = st.buckets b j := by
  unfold splitBuckets
  rw [if_neg hb1, if_neg hb2]

/-- Bucket content of the post-split state at the source destination. -/
theorem splitState_buckets_at_src (st : MapState K V) (g : Geometry) (j : Nat) :
    (splitState st g).buckets st.splitCursor j = splitSrcFun st g j := by
  show splitBuckets st g st.splitCursor j = splitSrcFun st g j
  rw [splitBuckets_at_src]

/-- Bucket content of the post-split state at the new destination. -/
theorem splitState_buckets_at_new (st : MapState K V) (g : Geometry) (j : Nat) :
    (splitState st g).buckets (st.splitCursor + 2 ^ st.level) j = splitNewFun st g j := by
  show splitBuckets st g (st.splitCursor + 2 ^ st.level) j = splitNewFun st g j
  rw [splitBuckets_at_new]

/-- Occupancy of a re-packed destination block is the packed prefix. -/
theorem occPred_splitState_src (st : MapState K V) (g : Geometry) (j : Nat) :
    occPred (splitState st g) st.splitCursor j
      = decide (j < countMatch (blockPred (srcImageFun st) (srcPred st g)) SlotsPerBucket) := by
  show (splitBuckets st g st.splitCursor j).isSome = _
  rw [splitBuckets_at_src]
  exact packImg_isSome _ _ _ _

theorem occPred_splitState_new (st : MapState K V) (g : Geometry) (j : Nat) :
    occPred (splitState st g) (st.splitCursor + 2 ^ st.level) j
      = decide (j < countMatch (blockPred (srcImageFun st) (newPred st g)) SlotsPerBucket) := by
  show (splitBuckets st g (st.splitCursor + 2 ^ st.level) j).isSome = _
  rw [splitBuckets_at_new]
  exact packImg_isSome _ _ _ _

/-! ## The picker (choose_image_location, map.rs L1623-L1660) -/

/-- Load of a re-packed source image = number of selected originals. -/
def srcCount (st : MapState K V) (g : Geometry) : Nat :=
  countMatch (blockPred (srcImageFun st) (srcPred st g)) SlotsPerBucket

/-- Load of a re-packed new-bucket image. -/
def newCount (st : MapState K V) (g : Geometry) : Nat :=
  countMatch (blockPred (srcImageFun st) (newPred st g)) SlotsPerBucket

/-- Load `choose_image_location` sees for bucket `b` under the stepped geometry:
the re-packed image count for the two destinations, the untouched disk load
otherwise (map.rs L1639-L1645). -/
def imgLoad (st : MapState K V) (g : Geometry) (b : Nat) : Nat :=
  if b = st.splitCursor then srcCount st g
  else if b = st.splitCursor + 2 ^ st.level then newCount st g
  else loadOf st b

theorem loadOf_splitState_src (st : MapState K V) (g : Geometry) :
    loadOf (splitState st g) st.splitCursor = srcCount st g := by
  unfold loadOf srcCount
  rw [countMatch_congr _ _ SlotsPerBucket (fun j _ => occPred_splitState_src st g j),
    countMatch_decide_lt _ _ (countMatch_le _ _)]

theorem loadOf_splitState_new (st : MapState K V) (g : Geometry) :
    loadOf (splitState st g) (st.splitCursor + 2 ^ st.level) = newCount st g := by
  unfold loadOf newCount
  rw [countMatch_congr _ _ SlotsPerBucket (fun j _ => occPred_splitState_new st g j),
    countMatch_decide_lt _ _ (countMatch_le _ _)]

theorem loadOf_splitState_eq (st : MapState K V) (g : Geometry) (x : Nat) :
    loadOf (splitState st g) x = imgLoad st g x := by
  unfold imgLoad
  by_cases h1 : x = st.splitCursor
  · subst h1
    rw [if_pos rfl]
    exact loadOf_splitState_src st g
  · by_cases h2 : x = st.splitCursor + 2 ^ st.level
    · subst h2
      have hne : ¬(st.splitCursor + 2 ^ st.level = st.splitCursor) := by
        have hp := two_pow_pos st.level
        omega
      rw [if_neg hne, if_pos rfl]
      exact loadOf_splitState_new st g
    · rw [if_neg h1, if_neg h2]
      exact loadOf_congr_buckets (fun j => splitBuckets_at_else st g h1 h2 j)

/-- Append slot of the carried entry in destination `b`: the end of a re-packed
image, or the first free slot of an untouched bucket (map.rs L1489-L1490,
L1513-L1515). Admission gate mirrors `load < SLOTS_PER_BUCKET`. -/
def imageSlot (st : MapState K V) (g : Geometry) (b : Nat) : Option Nat :=
  if imgLoad st g b < SlotsPerBucket then
    if b = st.splitCursor then some (srcCount st g)
    else if b = st.splitCursor + 2 ^ st.level then some (newCount st g)
    else firstFreeIdx st b
  else none

theorem firstFreeIdx_splitState_eq (st : MapState K V) (g : Geometry) (x : Nat) :
    firstFreeIdx (splitState st g) x = imageSlot st g x := by
  unfold imageSlot
  by_cases h1 : x = st.splitCursor
  · have hload : imgLoad st g x = srcCount st g := by
      unfold imgLoad; rw [if_pos h1]
    by_cases hlt : srcCount st g < SlotsPerBucket
    · rw [if_pos h1, if_pos (by rw [hload]; exact hlt)]
      refine firstFreeIdx_of_prefix (b := x) (c := srcCount st g) (fun j => ?_) hlt
      rw [h1]
      exact occPred_splitState_src st g j
    · rw [if_pos h1, if_neg (by rw [hload]; exact hlt)]
      refine firstMatch_none_of_all_false (fun i hi => ?_)
      have hle : SlotsPerBucket ≤ srcCount st g := Nat.not_lt.1 hlt
      have hic : i < srcCount st g := by omega
      rw [h1, occPred_splitState_src st g i]
      show (!(decide (i < srcCount st g))) = false
      simp [hic]
  · by_cases h2 : x = st.splitCursor + 2 ^ st.level
    · have hload : imgLoad st g x = newCount st g := by
        unfold imgLoad; rw [if_neg h1, if_pos h2]
      by_cases hlt : newCount st g < SlotsPerBucket
      · rw [if_neg h1, if_pos h2, if_pos (by rw [hload]; exact hlt)]
        refine firstFreeIdx_of_prefix (b := x) (c := newCount st g) (fun j => ?_) hlt
        rw [h2]
        exact occPred_splitState_new st g j
      · rw [if_neg h1, if_pos h2, if_neg (by rw [hload]; exact hlt)]
        refine firstMatch_none_of_all_false (fun i hi => ?_)
        have hle : SlotsPerBucket ≤ newCount st g := Nat.not_lt.1 hlt
        have hic : i < newCount st g := by omega
        rw [h2, occPred_splitState_new st g i]
        show (!(decide (i < newCount st g))) = false
        simp [hic]
    · have hload : imgLoad st g x = loadOf st x := by
        unfold imgLoad; rw [if_neg h1, if_neg h2]
      by_cases hlt : loadOf st x < SlotsPerBucket
      · rw [if_neg h1, if_neg h2, if_pos (by rw [hload]; exact hlt)]
        exact firstFreeIdx_congr (fun j => splitBuckets_at_else st g h1 h2 j)
      · rw [if_neg h1, if_neg h2, if_neg (by rw [hload]; exact hlt)]
        refine firstMatch_none_of_all_false (fun i hi => ?_)
        have hall : ∀ i₂, i₂ < SlotsPerBucket → occPred st x i₂ = true := by
          intro i₂ hi₂
          by_cases hp : occPred st x i₂ = true
          · exact hp
          · exfalso
            have hf : occPred st x i₂ = false := by
              cases hp' : occPred st x i₂ with
              | false => rfl
              | true => exact absurd hp' hp
            exact absurd
              (countMatch_lt_of_exists_false SlotsPerBucket i₂ hi₂ hf) hlt
        show (!(occPred (splitState st g) x i)) = false
        have hp1 : occPred (splitState st g) x i = occPred st x i := by
          show ((splitState st g).buckets x i).isSome = (st.buckets x i).isSome
          show (splitBuckets st g x i).isSome = (st.buckets x i).isSome
          rw [splitBuckets_at_else st g h1 h2]
        rw [hp1, hall i hi]
        rfl

/-- Destination of the carried insert under the stepped geometry: stage 3's
least-loaded-admissible chooser applied to the post-split state's candidate
blocks. By `loadOf_splitState_eq` and `firstFreeIdx_splitState_eq` this is
exactly what `choose_image_location` computes (map.rs L1623-L1660): the same
admission gate, the same re-packed image counts for source/new, the same disk
load for an untouched third bucket, ties to the first candidate. -/
def pickImage (st : MapState K V) (g : Geometry) (k : K) : Option (Nat × Nat) :=
  chooseFreeSlot (splitState st g) (candNew1 st g k) (candNew2 st g k)

/-! ## Picker soundness -/

/-- A picked destination lies among the new-geometry candidates of the carried
key, at a bounded slot genuinely free in the post-split state. -/
theorem pickImage_spec [DecidableEq K] {st : MapState K V} {g : Geometry} {k : K}
    {b j : Nat} (h : pickImage st g k = some (b, j)) :
    InCands (splitState st g) k b ∧ j < SlotsPerBucket ∧
      (splitState st g).buckets b j = none :=
  chooseFreeSlot_spec h

/-! ## Key absence survives the split -/

/-- A key absent from both pre-split candidate blocks stays absent everywhere in
the post-split state: re-packed entries all originate in the old source block,
whose resident keys lie in their own pre-candidate pairs (`Inv.placed`), and
untouched buckets are searched verbatim. -/
theorem findIn_splitState_absent [DecidableEq K] {st : MapState K V} (inv : Inv st)
    {g : Geometry} {k : K}
    (habsent : ∀ c, InCands st k c → findIn st c k = none) {c : Nat}
    (_hcand : InCands (splitState st g) k c) :
    findIn (splitState st g) c k = none := by
  by_cases hcs : c = st.splitCursor
  · cases hf : findIn (splitState st g) c k with
    | none => rfl
    | some w =>
        exfalso
        obtain ⟨j, e⟩ := w
        obtain ⟨_, hloc, hkey⟩ := findIn_some_spec hf
        rw [hcs] at hloc
        obtain ⟨k₀, hk₀, hfe, _, _⟩ :=
          packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j e
            ((splitState_buckets_at_src st g j).symm.trans hloc)
        have hcandOld : InCands st k st.splitCursor := by
          obtain ⟨_, _, hc⟩ := inv.placed st.splitCursor k₀ e hfe
          rw [← hkey]
          exact hc
        have hfnd := findIn_some_of_present (st := st) (b := st.splitCursor) (k := k)
          ⟨k₀, e, hk₀, hfe, hkey⟩
        rw [habsent _ hcandOld] at hfnd
        exact absurd hfnd (by simp)
  · by_cases hcn : c = st.splitCursor + 2 ^ st.level
    · cases hf : findIn (splitState st g) c k with
      | none => rfl
      | some w =>
          exfalso
          obtain ⟨j, e⟩ := w
          obtain ⟨_, hloc, hkey⟩ := findIn_some_spec hf
          rw [hcn] at hloc
          obtain ⟨k₀, hk₀, hfe, _, _⟩ :=
            packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j e
              ((splitState_buckets_at_new st g j).symm.trans hloc)
          -- every entry of the new image originates in the old source block
          have hcandOld : InCands st k st.splitCursor := by
            obtain ⟨_, _, hc⟩ := inv.placed st.splitCursor k₀ e hfe
            rw [← hkey]
            exact hc
          have hfnd := findIn_some_of_present (st := st) (b := st.splitCursor) (k := k)
            ⟨k₀, e, hk₀, hfe, hkey⟩
          rw [habsent _ hcandOld] at hfnd
          exact absurd hfnd (by simp)
    · rw [findIn_congr (fun j => splitBuckets_at_else st g hcs hcn j)]
      cases hf : findIn st c k with
      | none => rfl
      | some w =>
          exfalso
          obtain ⟨j, e⟩ := w
          obtain ⟨_, hloc, hkey⟩ := findIn_some_spec hf
          have hcandOld : InCands st k c := by
            obtain ⟨_, _, hc⟩ := inv.placed c j e hloc
            rw [← hkey]
            exact hc
          exact absurd hf (by rw [habsent c hcandOld]; simp)

/-! ## The composed transition and its preservation -/

/-- Final logical effect of one insert-carrying split (map.rs L1453-L1557 with
`insert = Some`, committed by L1662-L1678): the plain split plus the fresh
placement at the picked `(b, j)` — len + 1 and inline-overflow accounting per
`finish_split_plan` with `inserted = true` (map.rs L1575-L1578), epoch advanced
by exactly 2 through the fence. -/
def splitInsertState (st : MapState K V) (g : Geometry) (k : K) (v : V) (b j : Nat) :
    MapState K V :=
  placeAt (splitState st g) k v b j

/-- **Stage-4-note headline**: one split carrying a fresh insert preserves
`Inv` — stage 4's transfer followed by stage 3's placement lemma. -/
theorem inv_splitInsert_preserves [DecidableEq K] {st : MapState K V} (inv : Inv st)
    {g : Geometry} (hg : nextGeometry st.level st.splitCursor st.physicalBuckets = some g)
    {k : K} {v : V}
    (habsent : ∀ c, InCands st k c → findIn st c k = none)
    {b j : Nat} (hpick : pickImage st g k = some (b, j)) :
    Inv (splitInsertState st g k v b j) := by
  have hsplit := inv_split_transfer inv hg
  obtain ⟨hcand, hj, hfree⟩ := pickImage_spec hpick
  have hab' : ∀ c, InCands (splitState st g) k c → findIn (splitState st g) c k = none :=
    fun c hc => findIn_splitState_absent inv habsent hc
  exact inv_place hsplit hcand hj hfree hab'

/-! ## Equivalence with plain split then insert -/

/-- **Equivalence corollary**: a split carrying an insert realizes exactly the
plain split followed by `opInsert` under the stepped geometry — outcome
`placed` and identical final state. This is what lets stages 3–5 treat the
carried variant as derived syntax. -/
theorem splitInsert_eq_opInsert [DecidableEq K] {st : MapState K V} (inv : Inv st)
    {g : Geometry} (_hg : nextGeometry st.level st.splitCursor st.physicalBuckets = some g)
    {k : K} {v : V}
    (habsent : ∀ c, InCands st k c → findIn st c k = none)
    {b j : Nat} (hpick : pickImage st g k = some (b, j)) :
    opInsert (splitState st g) k v = (InsertOutcome.placed, splitInsertState st g k v b j) := by
  have hab' : ∀ c, InCands (splitState st g) k c → findIn (splitState st g) c k = none :=
    fun c hc => findIn_splitState_absent inv habsent hc
  have hscrut2 : (if cand1 (splitState st g) k = cand2 (splitState st g) k then none
      else findIn (splitState st g) (cand2 (splitState st g) k) k) = none := by
    by_cases hc12 : cand1 (splitState st g) k = cand2 (splitState st g) k
    · rw [if_pos hc12]
    · rw [if_neg hc12]
      exact hab' _ (Or.inr rfl)
  have hpair : chooseFreeSlot (splitState st g) (cand1 (splitState st g) k)
      (cand2 (splitState st g) k) = some (b, j) := hpick
  unfold opInsert
  rw [hab' _ (Or.inl rfl), hscrut2, hpair]
  rfl

/-! ## Guarded call (stage-5 composition) -/

/-- `apply_split` for a plan carrying the insert (map.rs L953-L956 feeding
L1662-L1678): grow, open the fence, commit the split-plus-entry state, close.
Every fallible step precedes the writes. -/
def applySplitInsertCall (st : MapState K V) (g : Geometry) (room : Bool) (k : K) (v : V)
    (b j : Nat) : CallResult K V :=
  match growTo st room with
  | .ok s1 => runGuarded (fun s => splitInsertState s g k v b j) s1
  | .fail e s1 => .fail e s1

theorem apply_split_insert_call_fail_atomic (st : MapState K V) (g : Geometry) (room : Bool)
    (k : K) (v : V) (b j : Nat) {e : MutErr} {s' : MapState K V}
    (h : applySplitInsertCall st g room k v b j = .fail e s') : s' = st := by
  unfold applySplitInsertCall at h
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

/-- A successful guarded call realizes the composed transition exactly, under
an even observed epoch. -/
theorem apply_split_insert_call_ok_realizes (st : MapState K V) (g : Geometry) (room : Bool)
    (k : K) (v : V) (b j : Nat) {s' : MapState K V}
    (h : applySplitInsertCall st g room k v b j = .ok s') :
    st.mutationEpoch % 2 = 0 ∧ s' = splitInsertState st g k v b j := by
  unfold applySplitInsertCall at h
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

end Lhm.Abs
