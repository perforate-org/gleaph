/-
Stage 4: split preservation — routing corollaries, the split state transformer,
and the placement/counter lemmas that discharge `inv_split_transfer`.

One split step (map.rs L1693-L1709 `next_geometry` driving map.rs L1453-L1557
`plan_split`) has exactly two successful shapes: cursor advance `(L, cur + 1)` and
level increment `(L + 1, 0)` at the frontier `cur + 1 = 2^L`. The lemmas below pin
down, for every old candidate value `x`, whether the new candidate stays `x`:

- entries whose old candidate is the source `cur` may move to `cur + 2^L` (P2);
- entries at any other bucket keep both candidates exactly — a candidate can only
  change when `lowBits hash L = cur`, which routes to the source itself, or (in the
  level-increment case) sits past the frontier, which is empty there.

This is what makes "relocate the source block, copy everything else verbatim"
preserve the placement invariant.
-/
import Lhm.Abs.OpPreserve

namespace Lhm.Abs

open Lhm

/-! ## Shape of a successful geometry step -/

/-- A successful `nextGeometry` is a cursor advance or a frontier level increment;
either way the bucket count grows by exactly one, and the surviving level bound
(`L < 63`, respectively `L + 1 < 63`) is exported. -/
theorem nextGeometry_cases {L cur pb : Nat} {g : Geometry}
    (hg : nextGeometry L cur pb = some g) :
    (g.level = L ∧ g.cursor = cur + 1 ∧ g.buckets = pb + 1 ∧ L < 63)
      ∨ (cur + 1 = 2 ^ L ∧ g.level = L + 1 ∧ g.cursor = 0 ∧ g.buckets = pb + 1
          ∧ L + 1 < 63) := by
  rw [nextGeometry] at hg
  split at hg
  · rename_i hl63
    by_cases hc : cur + 1 = 2 ^ L
    · rw [if_pos hc] at hg
      by_cases hl : L + 1 < 63
      · rw [if_pos hl] at hg
        cases hg
        exact Or.inr ⟨hc, rfl, rfl, rfl, hl⟩
      · rw [if_neg hl] at hg
        simp at hg
    · rw [if_neg hc] at hg
      cases hg
      exact Or.inl ⟨rfl, rfl, rfl, hl63⟩
  · simp at hg

/-! ## Candidate fixity off the source -/

/-- Cursor advance: an entry whose old candidate is `x ≠ cur` keeps it. -/
theorem route_fixed_adv (hash L cur x : Nat)
    (hold : linearBucket hash L cur = x) (hx : x ≠ cur) :
    linearBucket hash L (cur + 1) = x := by
  rcases Nat.lt_trichotomy (lowBits hash L) cur with hlt | heq | hgt
  · rw [linearBucket_eq_wide hash L cur hlt] at hold
    rw [linearBucket_eq_wide hash L (cur + 1) (by omega)]
    exact hold
  · exfalso
    rw [linearBucket_eq_low hash L cur (by omega)] at hold
    omega
  · rw [linearBucket_eq_low hash L cur (by omega)] at hold
    rw [linearBucket_eq_low hash L (cur + 1) (by omega)]
    exact hold

/-- Frontier level increment: an entry whose old candidate is `x ≠ cur` keeps it.
Only `lowBits = cur = 2^L - 1` could flip, and that is the source itself. -/
theorem route_fixed_up (hash L cur x : Nat)
    (hc : cur + 1 = 2 ^ L)
    (hold : linearBucket hash L cur = x) (hx : x ≠ cur) :
    linearBucket hash (L + 1) 0 = x := by
  have hnew : linearBucket hash (L + 1) 0 = wideBits hash L :=
    linearBucket_at_zero hash (L + 1)
  have hlow : lowBits hash L < 2 ^ L := lt_mod_two_pow hash L
  rcases Nat.lt_trichotomy (lowBits hash L) cur with hlt | heq | hgt
  · rw [linearBucket_eq_wide hash L cur hlt] at hold
    rw [hnew]
    exact hold
  · exfalso
    rw [linearBucket_eq_low hash L cur (by omega)] at hold
    have hlow2 : lowBits hash L < 2 ^ L := lt_mod_two_pow hash L
    omega
  · exfalso
    omega

/-- Cursor advance, post-base zone: a resident of a bucket `x ≥ 2^L` keeps it (its
old candidate is necessarily the wide residue, which does not move). -/
theorem route_fixed_high_adv (hash L cur x : Nat)
    (hx : 2 ^ L ≤ x) (hold : linearBucket hash L cur = x) :
    linearBucket hash L (cur + 1) = x := by
  have hlow : lowBits hash L < 2 ^ L := lt_mod_two_pow hash L
  rcases Nat.lt_or_ge (lowBits hash L) cur with hlt | hge
  · have hw : wideBits hash L = x := by
      rw [← hold]
      exact (linearBucket_eq_wide hash L cur hlt).symm
    rw [linearBucket_eq_wide hash L (cur + 1) (by omega)]
    exact hw
  · have hl : lowBits hash L = x := by
      rw [(linearBucket_eq_low hash L cur (by omega)).symm]
      exact hold
    rw [linearBucket_eq_low hash L (cur + 1) (by omega)]
    exact hl

/-- Frontier level increment, post-base zone: the new first candidate is the same
wide residue, so the resident keeps its bucket. -/
theorem route_fixed_high_up (hash L cur x : Nat)
    (hx : 2 ^ L ≤ x) (hold : linearBucket hash L cur = x) :
    linearBucket hash (L + 1) 0 = x := by
  have hlow : lowBits hash L < 2 ^ L := lt_mod_two_pow hash L
  have hnew : linearBucket hash (L + 1) 0 = wideBits hash L :=
    linearBucket_at_zero hash (L + 1)
  rcases Nat.lt_or_ge (lowBits hash L) cur with hlt | hge
  · have hw : wideBits hash L = x := by
      rw [← hold]
      exact (linearBucket_eq_wide hash L cur hlt).symm
    rw [hnew]
    exact hw
  · exfalso
    rw [linearBucket_eq_low hash L cur (by omega)] at hold
    have hlow2 : lowBits hash L < 2 ^ L := lt_mod_two_pow hash L
    omega

/-- Across a successful step every candidate either stays or gains the old base. -/
theorem route_moves_step (hash L cur pb x : Nat) {g : Geometry}
    (hg : nextGeometry L cur pb = some g)
    (hold : linearBucket hash L cur = x) :
    linearBucket hash g.level g.cursor = x ∨
      linearBucket hash g.level g.cursor = x + 2 ^ L := by
  rcases nextGeometry_cases hg with h | h
  · rw [h.1, h.2.1]
    rcases split_stability_cursor_adv hash L cur with he | hm
    · left; rw [he]; exact hold
    · right; rw [hm, hold]
  · obtain ⟨hc, hl2, hc0, _⟩ := h
    rw [hl2, hc0]
    rcases split_stability_level_up hash L cur with he | hm
    · left; rw [he]; exact hold
    · right; rw [hm, hold]

/-- Away from the source, both candidates of an entry are fixed across the step. -/
theorem route_fixed_step (hash L cur pb x : Nat) {g : Geometry}
    (hg : nextGeometry L cur pb = some g)
    (hold : linearBucket hash L cur = x) (hx : x ≠ cur) :
    linearBucket hash g.level g.cursor = x := by
  rcases nextGeometry_cases hg with h | h
  · rw [h.1, h.2.1]
    rcases Nat.lt_or_ge x (2 ^ L) with hbelow | hhigh
    · exact route_fixed_adv hash L cur x hold hx
    · exact route_fixed_high_adv hash L cur x hhigh hold
  · obtain ⟨hc, hl2, hc0, _⟩ := h
    rw [hl2, hc0]
    rcases Nat.lt_or_ge x (2 ^ L) with hbelow | hhigh
    · exact route_fixed_up hash L cur x hc hold hx
    · exact route_fixed_high_up hash L cur x hhigh hold

/-
## The split state transformer

`splitState st g` mirrors one successful maintenance split (map.rs L1453-L1557
`plan_split` with `insert = none`, published by L1668-L1675): the source block is
redistributed to `{source, source + 2^level}` under geometry `g`, every other
block is copied verbatim, and the control record advances to `g`. Entries whose
new first candidate misses both destination blocks model Rust's fail-closed
`TablePressure`; the preservation theorem shows that cannot happen.
-/

variable {K V : Type}

/-! ## Destination choice (map.rs L1476-L1483) -/

/-- First candidate of key `k` under the stepped geometry. -/
def candNew1 (st : MapState K V) (g : Geometry) (k : K) : Nat :=
  linearBucket (st.hash1 k) g.level g.cursor

/-- Second candidate of key `k` under the stepped geometry. -/
def candNew2 (st : MapState K V) (g : Geometry) (k : K) : Nat :=
  linearBucket (st.hash2 k) g.level g.cursor

/-- Destination of one source-block entry: the source bucket when either candidate
hits it, else the new bucket when either candidate hits it, else fail-closed
`none` (= Rust's `TablePressure`). -/
def splitDest (st : MapState K V) (g : Geometry) (k : K) : Option Nat :=
  if candNew1 st g k = st.splitCursor ∨ candNew2 st g k = st.splitCursor then
    some st.splitCursor
  else if candNew1 st g k = st.splitCursor + 2 ^ st.level ∨
      candNew2 st g k = st.splitCursor + 2 ^ st.level then
    some (st.splitCursor + 2 ^ st.level)
  else none

/-- A decided destination is witnessed by a candidate. -/
theorem splitDest_cases {st : MapState K V} {g : Geometry} {k : K} {b : Nat}
    (h : splitDest st g k = some b) :
    candNew1 st g k = b ∨ candNew2 st g k = b := by
  unfold splitDest at h
  by_cases hc : candNew1 st g k = st.splitCursor ∨
      candNew2 st g k = st.splitCursor
  · rw [if_pos hc] at h
    refine hc.elim ?_ ?_
    · intro hh; exact Or.inl (hh.trans (Option.some.inj h))
    · intro hh; exact Or.inr (hh.trans (Option.some.inj h))
  · by_cases hc2 : candNew1 st g k = st.splitCursor + 2 ^ st.level ∨
        candNew2 st g k = st.splitCursor + 2 ^ st.level
    · rw [if_neg hc, if_pos hc2] at h
      refine hc2.elim ?_ ?_
      · intro hh; exact Or.inl (hh.trans (Option.some.inj h))
      · intro hh; exact Or.inr (hh.trans (Option.some.inj h))
    · rw [if_neg hc, if_neg hc2] at h
      exact absurd h (by simp)

/-- The two destinations are distinct. -/
theorem splitDest_disjoint {st : MapState K V} {g : Geometry} {k : K}
    (h1 : splitDest st g k = some st.splitCursor)
    (h2 : splitDest st g k = some (st.splitCursor + 2 ^ st.level)) : False := by
  have hinj : st.splitCursor + 2 ^ st.level = st.splitCursor :=
    Option.some.inj (h2.symm.trans h1)
  have hp := Nat.two_pow_pos st.level
  omega

/-- Selection predicate for the re-packed source image. -/
def srcPred (st : MapState K V) (g : Geometry) (e : K × V) : Bool :=
  splitDest st g e.1 == some st.splitCursor

/-- Selection predicate for the re-packed new-bucket image. -/
def newPred (st : MapState K V) (g : Geometry) (e : K × V) : Bool :=
  splitDest st g e.1 == some (st.splitCursor + 2 ^ st.level)

theorem srcPred_true {st : MapState K V} {g : Geometry} {e : K × V}
    (hd : splitDest st g e.1 = some st.splitCursor) : srcPred st g e = true := by
  show (splitDest st g e.1 == some st.splitCursor) = true
  rw [hd]
  exact decide_eq_true rfl

theorem newPred_true {st : MapState K V} {g : Geometry} {e : K × V}
    (hd : splitDest st g e.1 = some (st.splitCursor + 2 ^ st.level)) :
    newPred st g e = true := by
  show (splitDest st g e.1 == some (st.splitCursor + 2 ^ st.level)) = true
  rw [hd]
  exact decide_eq_true rfl

theorem of_srcPred {st : MapState K V} {g : Geometry} {e : K × V}
    (h : srcPred st g e = true) :
    splitDest st g e.1 = some st.splitCursor := by
  simpa [srcPred] using h

theorem of_newPred {st : MapState K V} {g : Geometry} {e : K × V}
    (h : newPred st g e = true) :
    splitDest st g e.1 = some (st.splitCursor + 2 ^ st.level) := by
  simpa [newPred] using h

/-- Every entry of the old source block has a destination: no `TablePressure`.
This is P2 applied to whichever candidate placed the entry; `hcur` identifies
the old cursor with `cur`. -/
theorem splitDest_defined {st : MapState K V} (inv : Inv st) {L cur pb : Nat}
    {g : Geometry} (hg : nextGeometry L cur pb = some g)
    (hlev : st.level = L) (hcur : st.splitCursor = cur) {j : Nat} {e : K × V}
    (he : st.buckets st.splitCursor j = some e) :
    splitDest st g e.1 = some cur ∨
      splitDest st g e.1 = some (cur + 2 ^ L) := by
  obtain ⟨_, _, hcand⟩ := inv.placed st.splitCursor j e he
  unfold InCands at hcand
  simp only [cand1, cand2, hlev, hcur] at hcand
  by_cases hcA : linearBucket (st.hash1 e.1) g.level g.cursor = cur ∨
      linearBucket (st.hash2 e.1) g.level g.cursor = cur
  · refine Or.inl ?_
    simp only [splitDest, candNew1, candNew2, hcur]
    rw [if_pos hcA]
  · have hcB : linearBucket (st.hash1 e.1) g.level g.cursor = cur + 2 ^ L ∨
        linearBucket (st.hash2 e.1) g.level g.cursor = cur + 2 ^ L := by
      rcases hcand with hc | hc
      · rcases route_moves_step (st.hash1 e.1) L cur pb cur hg hc.symm with h | h
        · exact absurd (Or.inl h) hcA
        · exact Or.inl h
      · rcases route_moves_step (st.hash2 e.1) L cur pb cur hg hc.symm with h | h
        · exact absurd (Or.inr h) hcA
        · exact Or.inr h
    refine Or.inr ?_
    simp only [splitDest, candNew1, candNew2, hcur, hlev]
    rw [if_neg hcA, if_pos hcB]

/-! ## The transformer -/

/-- Old source-block content function. -/
def srcImageFun (st : MapState K V) : Nat → Option (K × V) :=
  fun j => st.buckets st.splitCursor j

/-- Re-packed destination image kept at the source bucket. -/
def splitSrcFun (st : MapState K V) (g : Geometry) (j : Nat) : Option (K × V) :=
  packImg (srcImageFun st) (srcPred st g) SlotsPerBucket j

/-- Re-packed destination image placed at the new bucket. -/
def splitNewFun (st : MapState K V) (g : Geometry) (j : Nat) : Option (K × V) :=
  packImg (srcImageFun st) (newPred st g) SlotsPerBucket j

/-- Post-split bucket content: two re-packed images plus verbatim copies. -/
def splitBuckets (st : MapState K V) (g : Geometry) :
    Nat → Nat → Option (K × V) :=
  fun b j =>
    if b = st.splitCursor then splitSrcFun st g j
    else if b = st.splitCursor + 2 ^ st.level then splitNewFun st g j
    else st.buckets b j

/-- Overflow-slot count of one image (map.rs image `overflow_entries`). -/
def ovfCountFun (f : Nat → Option (K × V)) : Nat :=
  countMatch
    (fun j => match f j with
      | some _ => PrimarySlots ≤ j
      | none => false)
    SlotsPerBucket

/-- Recomputed `overflow_entries` (finish_split_plan L1579-L1595 with
`inserted = false`: subtract the source share, add both re-packed images). -/
def splitOverflow (st : MapState K V) (g : Geometry) : Nat :=
  st.overflowEntries - ovfLoadOf st st.splitCursor
    + ovfCountFun (splitSrcFun st g) + ovfCountFun (splitNewFun st g)

/-- One maintenance split under the stepped geometry `g`. -/
def splitState (st : MapState K V) (g : Geometry) : MapState K V :=
  { st with
    buckets := splitBuckets st g
    physicalBuckets := g.buckets
    overflowEntries := splitOverflow st g
    level := g.level
    splitCursor := g.cursor }

/-! ## Invariant preservation under one split -/

theorem ovfCountFun_eq_ovfLoadOf {st : MapState K V} {b : Nat}
    {f : Nat → Option (K × V)} (hf : ∀ j, st.buckets b j = f j) :
    ovfLoadOf st b = ovfCountFun f := by
  unfold ovfLoadOf ovfCountFun ovfPred
  exact countMatch_congr _ _ SlotsPerBucket (fun j _ => by rw [hf j]; cases f j <;> simp)

theorem loadOf_congr_buckets {st s2 : MapState K V} {b : Nat}
    (hf : ∀ j, s2.buckets b j = st.buckets b j) : loadOf s2 b = loadOf st b := by
  unfold loadOf
  exact countMatch_congr _ _ SlotsPerBucket (fun j _ => by
    unfold occPred; rw [hf])

theorem ovfLoadOf_congr_buckets {st s2 : MapState K V} {b : Nat}
    (hf : ∀ j, s2.buckets b j = st.buckets b j) : ovfLoadOf s2 b = ovfLoadOf st b := by
  unfold ovfLoadOf
  exact countMatch_congr _ _ SlotsPerBucket (fun j _ => by
    unfold ovfPred; rw [hf])

/-- A selected source slot makes the prefix count strictly increase to the total,
so every packed slot index below the selection count is a genuine slot. -/
private theorem countMatch_true_lt {p : Nat → Bool} {k S : Nat} (hk : k < S)
    (hp : p k = true) : countMatch p k < countMatch p S := by
  have hsucc := countMatch_succ_of_true p k hp
  have hmono := countMatch_mono p S (k + 1) (by omega)
  have hle := countMatch_le p S
  omega

/-- A non-empty packed output slot is a real slot index of the image. -/
private theorem packImg_slot_lt {α : Type} {f : Nat → Option α} {p : α → Bool}
    {n j : Nat} {e : α} (h : packImg f p n j = some e) : j < n := by
  obtain ⟨k, hklt, hfe, hpsel, hcnt⟩ := packImg_spec f p n j e h
  have hbpk : blockPred f p k = true := by
    simp only [blockPred, hfe]
    exact hpsel
  have hj := countMatch_true_lt hklt hbpk
  have hle := countMatch_le (blockPred f p) n
  omega

private theorem inv_split_transfer_aux {st : MapState K V} (inv : Inv st)
    {L cur pb gl gc gb : Nat}
    (hlev : st.level = L) (hcur : st.splitCursor = cur)
    (hpbE : st.physicalBuckets = 2 ^ L + cur)
    (hg : nextGeometry L cur pb = some g)
    (glev : g.level = gl) (gcur : g.cursor = gc) (gbk : g.buckets = gb)
    (ihl : InitialLevel ≤ gl) (ihh : gl < 63) (icb : gc < 2 ^ gl)
    (ibe : gb = 2 ^ gl + gc) (hSplus : gb = st.physicalBuckets + 1) :
    Inv (splitState st g) := by
  have hpow : 0 < 2 ^ L := Nat.two_pow_pos L
  have hnb : st.splitCursor + 2 ^ L = st.physicalBuckets := by omega
  have hSL : (splitState st g).level = g.level := rfl
  have hSC : (splitState st g).splitCursor = g.cursor := rfl
  have hSB : (splitState st g).physicalBuckets = g.buckets := rfl
  have hsH1 : (splitState st g).hash1 = st.hash1 := rfl
  have hsH2 : (splitState st g).hash2 = st.hash2 := rfl
  -- bucket-function identities of the transformed state
  have heqSrc : ∀ j, (splitState st g).buckets st.splitCursor j
      = splitSrcFun st g j := by
    intro j
    show splitBuckets st g st.splitCursor j = splitSrcFun st g j
    unfold splitBuckets splitSrcFun
    rw [if_pos rfl]
  have heqNb : ∀ j, (splitState st g).buckets (st.splitCursor + 2 ^ L) j
      = splitNewFun st g j := by
    intro j
    show splitBuckets st g (st.splitCursor + 2 ^ L) j = splitNewFun st g j
    have hne : ¬(st.splitCursor + 2 ^ L = st.splitCursor) := by omega
    unfold splitBuckets splitNewFun
    rw [hlev, if_neg hne, if_pos rfl]
  have heqElse : ∀ b : Nat, b ≠ st.splitCursor →
      b ≠ st.splitCursor + 2 ^ L → ∀ j,
        (splitState st g).buckets b j = st.buckets b j := by
    intro b hb1 hb2 j
    show splitBuckets st g b j = st.buckets b j
    unfold splitBuckets
    rw [hlev, if_neg hb1, if_neg hb2]
  have agree : ∀ b : Nat, b < st.physicalBuckets → b ≠ st.splitCursor →
      ∀ j, (splitState st g).buckets b j = st.buckets b j := fun b hb hne j =>
        heqElse b hne (by omega) j
  -- selection counts of the two re-packed images
  have cntSrcLe := countMatch_le
    (blockPred (srcImageFun st) (srcPred st g)) SlotsPerBucket
  have cntNewLe := countMatch_le
    (blockPred (srcImageFun st) (newPred st g)) SlotsPerBucket
  -- the two selections partition the old source block
  have part : countMatch (blockPred (srcImageFun st) (srcPred st g)) SlotsPerBucket
      + countMatch (blockPred (srcImageFun st) (newPred st g)) SlotsPerBucket
      = loadOf st st.splitCursor := by
    have hocc : loadOf st st.splitCursor =
        countMatch (blockPred (srcImageFun st) (fun _ => true)) SlotsPerBucket := by
      unfold loadOf occPred
      apply countMatch_congr
      intro k _
      simp only [srcImageFun, blockPred]
      cases hs : st.buckets st.splitCursor k <;> simp
    rw [hocc]
    refine countMatch_split _ _ _ SlotsPerBucket ?_ ?_ ?_
    · intro k _ hq
      simp only [blockPred] at hq
      cases hs : srcImageFun st k with
      | none =>
          rw [hs] at hq
          simp at hq
      | some e =>
          rw [hs] at hq
          have hsel : srcPred st g e = true := hq
          have hd : splitDest st g e.1 = some st.splitCursor :=
            of_srcPred hsel
          refine ⟨?_, ?_⟩
          · simp only [blockPred, hs]
          · have hb' : newPred st g e = false := by
              cases hb : newPred st g e with
              | false => rfl
              | true =>
                  exact absurd
                    (splitDest_disjoint hd (of_newPred hb)) (by simp)
            simp only [blockPred, hs, hb']
    · intro k _ hr
      simp only [blockPred] at hr
      cases hs : srcImageFun st k with
      | none =>
          rw [hs] at hr
          simp at hr
      | some e =>
          rw [hs] at hr
          have hsel : newPred st g e = true := hr
          have hd : splitDest st g e.1 = some (st.splitCursor + 2 ^ st.level) :=
            of_newPred hsel
          refine ⟨?_, ?_⟩
          · simp only [blockPred, hs]
          · have hb' : srcPred st g e = false := by
              cases hb : srcPred st g e with
              | false => rfl
              | true =>
                  exact absurd
                    (splitDest_disjoint (of_srcPred hb) hd) (by simp)
            simp only [blockPred, hs, hb']
    · intro k _ hp
      simp only [blockPred] at hp
      cases hs : srcImageFun st k with
      | none =>
          rw [hs] at hp
          simp at hp
      | some e =>
          rw [hs] at hp
          rcases splitDest_defined inv hg hlev hcur hs with hd | hd
          · exact Or.inl (by
              rw [← hcur] at hd
              have hp' : srcPred st g e = true := srcPred_true hd
              simp only [blockPred, hs]
              exact hp')
          · exact Or.inr (by
              rw [← hcur, ← hlev] at hd
              have hp' : newPred st g e = true := newPred_true hd
              simp only [blockPred, hs]
              exact hp')
  -- loads of the transformed state at the two destination buckets
  have lsrc : loadOf (splitState st g) st.splitCursor
      = countMatch (blockPred (srcImageFun st) (srcPred st g)) SlotsPerBucket := by
    have hpt : ∀ j, occPred (splitState st g) st.splitCursor j =
        decide (j < countMatch (blockPred (srcImageFun st)
          (srcPred st g)) SlotsPerBucket) := by
      intro j
      show ((splitState st g).buckets st.splitCursor j).isSome = _
      rw [heqSrc j]
      exact packImg_isSome _ _ _ _
    unfold loadOf
    rw [countMatch_congr _ _ SlotsPerBucket (fun j _ => hpt j),
      countMatch_decide_lt _ _ cntSrcLe]
  have lnew : loadOf (splitState st g) (st.splitCursor + 2 ^ L)
      = countMatch (blockPred (srcImageFun st) (newPred st g)) SlotsPerBucket := by
    have hpt : ∀ j, occPred (splitState st g) (st.splitCursor + 2 ^ L) j =
        decide (j < countMatch (blockPred (srcImageFun st)
          (newPred st g)) SlotsPerBucket) := by
      intro j
      show ((splitState st g).buckets (st.splitCursor + 2 ^ L) j).isSome = _
      rw [heqNb j]
      exact packImg_isSome _ _ _ _
    unfold loadOf
    rw [countMatch_congr _ _ SlotsPerBucket (fun j _ => hpt j),
      countMatch_decide_lt _ _ cntNewLe]
  have partL : loadOf (splitState st g) st.splitCursor
      + loadOf (splitState st g) (st.splitCursor + 2 ^ L)
      = loadOf st st.splitCursor := by
    rw [lsrc, lnew]; exact part
  refine ⟨by rw [hSL, glev]; exact ihl, by rw [hSL, glev]; exact ihh,
    by rw [hSC, gcur, hSL, glev]; exact icb,
    by rw [hSB, gbk, hSL, glev, hSC, gcur]; exact ibe,
    inv.geomEpochEven, inv.geomIncarnationPos, ?_, ?_, ?_, ?_⟩
  · -- countersLen
    have cL : st.len = totalLoads (fun x => loadOf st x) (st.splitCursor + 2 ^ L) := by
      rw [inv.countersLen, totalLenOf_eq, hnb]
    have lsrcAt : totalLoads (fun x => loadOf (splitState st g) x)
          (st.splitCursor + 2 ^ L) + loadOf st st.splitCursor
        = totalLoads (fun x => loadOf st x) (st.splitCursor + 2 ^ L)
          + loadOf (splitState st g) st.splitCursor :=
      totalLoads_except (fun x => loadOf (splitState st g) x) (fun x => loadOf st x)
        (st.splitCursor + 2 ^ L) st.splitCursor (Nat.lt_add_of_pos_right hpow)
        (fun b hb hne => loadOf_congr_buckets (agree b (by omega) hne))
    have calcEq : st.len
        = totalLoads (fun x => loadOf (splitState st g) x) (st.splitCursor + 2 ^ L)
          + loadOf (splitState st g) (st.splitCursor + 2 ^ L) := by
      omega
    show st.len
        = totalLoads (fun x => loadOf (splitState st g) x)
            ((splitState st g).physicalBuckets)
    rw [hSB, gbk, hSplus, totalLoads_succ, ← hnb]
    exact calcEq
  · -- countersOvf
    have cO : st.overflowEntries
        = totalLoads (fun x => ovfLoadOf st x) (st.splitCursor + 2 ^ L) := by
      rw [inv.countersOvf, totalOvfOf_eq, hnb]
    have ovsr : ovfLoadOf (splitState st g) st.splitCursor
        = ovfCountFun (splitSrcFun st g) :=
      ovfCountFun_eq_ovfLoadOf heqSrc
    have ovnb : ovfLoadOf (splitState st g) (st.splitCursor + 2 ^ L)
        = ovfCountFun (splitNewFun st g) :=
      ovfCountFun_eq_ovfLoadOf heqNb
    have lsrcAt : totalLoads (fun x => ovfLoadOf (splitState st g) x)
          (st.splitCursor + 2 ^ L) + ovfLoadOf st st.splitCursor
        = totalLoads (fun x => ovfLoadOf st x) (st.splitCursor + 2 ^ L)
          + ovfLoadOf (splitState st g) st.splitCursor :=
      totalLoads_except (fun x => ovfLoadOf (splitState st g) x)
        (fun x => ovfLoadOf st x)
        (st.splitCursor + 2 ^ L) st.splitCursor (Nat.lt_add_of_pos_right hpow)
        (fun b hb hne => ovfLoadOf_congr_buckets (agree b (by omega) hne))
    have srcLe : ovfLoadOf st st.splitCursor
        ≤ totalLoads (fun x => ovfLoadOf st x) (st.splitCursor + 2 ^ L) :=
      totalLoads_ge_single (fun x => ovfLoadOf st x) (st.splitCursor + 2 ^ L)
        st.splitCursor (Nat.lt_add_of_pos_right hpow)
    have calcEq : st.overflowEntries - ovfLoadOf st st.splitCursor
          + ovfCountFun (splitSrcFun st g) + ovfCountFun (splitNewFun st g)
        = totalLoads (fun x => ovfLoadOf (splitState st g) x)
            (st.splitCursor + 2 ^ L)
          + ovfLoadOf (splitState st g) (st.splitCursor + 2 ^ L) := by
      omega
    show st.overflowEntries - ovfLoadOf st st.splitCursor
          + ovfCountFun (splitSrcFun st g) + ovfCountFun (splitNewFun st g)
        = totalLoads (fun x => ovfLoadOf (splitState st g) x)
            ((splitState st g).physicalBuckets)
    rw [hSB, gbk, hSplus, totalLoads_succ, ← hnb]
    exact calcEq
  · -- placed
    intro b j e he
    by_cases hb1 : b = st.splitCursor
    · rw [hb1] at he
      rw [heqSrc j] at he
      obtain ⟨k, hklt, hfe, hpsel, hcnt⟩ :=
        packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j e he
      have hd : splitDest st g e.1 = some st.splitCursor :=
        of_srcPred hpsel
      have hjS := packImg_slot_lt he
      refine ⟨?_, ?_, ?_⟩
      · rw [hSB, gbk, hSplus]
        have hc := inv.geomCursorBound
        omega
      · exact hjS
      · rcases splitDest_cases hd with hh | hh
        · unfold InCands
          simp only [cand1, cand2, hsH1, hsH2, hSL, hSC, hb1]
          exact Or.inl hh.symm
        · unfold InCands
          simp only [cand1, cand2, hsH1, hsH2, hSL, hSC, hb1]
          exact Or.inr hh.symm
    · by_cases hb2 : b = st.splitCursor + 2 ^ L
      · rw [hb2] at he
        rw [heqNb j] at he
        obtain ⟨k, hklt, hfe, hpsel, hcnt⟩ :=
          packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j e he
        have hd : splitDest st g e.1 = some (st.splitCursor + 2 ^ st.level) :=
          of_newPred hpsel
        rw [hlev] at hd
        have hjS := packImg_slot_lt he
        refine ⟨?_, ?_, ?_⟩
        · rw [hSB, gbk, hSplus]
          omega
        · exact hjS
        · rcases splitDest_cases hd with hh | hh
          · unfold InCands
            simp only [cand1, cand2, hsH1, hsH2, hSL, hSC, hb2]
            exact Or.inl hh.symm
          · unfold InCands
            simp only [cand1, cand2, hsH1, hsH2, hSL, hSC, hb2]
            exact Or.inr hh.symm
      · have hold := inv.placed b j e (by
          exact (heqElse b hb1 hb2 j).symm.trans he)
        obtain ⟨hbLt, hjLt, hcandOld⟩ := hold
        unfold InCands at hcandOld
        simp only [cand1, cand2] at hcandOld
        rw [hlev, hcur] at hcandOld
        rw [hcur] at hb1
        refine ⟨?_, hjLt, ?_⟩
        · rw [hSB, gbk, hSplus]
          omega
        · unfold InCands
          simp only [cand1, cand2, hsH1, hsH2, hSL, hSC]
          rcases hcandOld with hh | hh
          · exact Or.inl (route_fixed_step (st.hash1 e.1) L cur pb b hg hh.symm hb1).symm
          · exact Or.inr (route_fixed_step (st.hash2 e.1) L cur pb b hg hh.symm hb1).symm
  · -- unique
    intro b1 j1 e1 b2 j2 e2 hj1 hj2 he1 he2 hkeys
    by_cases h1s : b1 = st.splitCursor
    · rw [h1s] at he1
      rw [heqSrc j1] at he1
      obtain ⟨k1, hklt1, hfe1, hps1, hc1⟩ :=
        packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j1 e1 he1
      by_cases h2s : b2 = st.splitCursor
      · rw [h2s] at he2
        rw [heqSrc j2] at he2
        obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
          packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j2 e2 he2
        have hu := inv.unique st.splitCursor k1 e1 st.splitCursor k2 e2 hklt1 hklt2
          hfe1 hfe2 hkeys
        exact ⟨by rw [h1s, h2s], by
          obtain ⟨_, hkeq⟩ := hu
          rw [← hc1, ← hc2, hkeq]⟩
      · by_cases h2n : b2 = st.splitCursor + 2 ^ L
        · rw [h2n] at he2
          rw [heqNb j2] at he2
          obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
            packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j2 e2 he2
          exfalso
          have hd1 : splitDest st g e1.1 = some st.splitCursor :=
            of_srcPred hps1
          have hd2 : splitDest st g e1.1 = some (st.splitCursor + 2 ^ st.level) := by
            rw [hkeys]; exact of_newPred hps2
          exact splitDest_disjoint hd1 hd2
        · exfalso
          have hv : st.buckets b2 j2 = some e2 :=
            (heqElse b2 h2s h2n j2).symm.trans he2
          have hu := inv.unique st.splitCursor k1 e1 b2 j2 e2 hklt1 hj2 hfe1 hv hkeys
          obtain ⟨heqb, _⟩ := hu
          exact h2s heqb.symm
    · by_cases h1n : b1 = st.splitCursor + 2 ^ L
      · rw [h1n] at he1
        rw [heqNb j1] at he1
        obtain ⟨k1, hklt1, hfe1, hps1, hc1⟩ :=
          packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j1 e1 he1
        by_cases h2s : b2 = st.splitCursor
        · rw [h2s] at he2
          rw [heqSrc j2] at he2
          obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
            packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j2 e2 he2
          exfalso
          have hd1 : splitDest st g e2.1 = some (st.splitCursor + 2 ^ st.level) := by
            rw [← hkeys]; exact of_newPred hps1
          have hd2 : splitDest st g e2.1 = some st.splitCursor :=
            of_srcPred hps2
          exact splitDest_disjoint hd2 hd1
        · by_cases h2n : b2 = st.splitCursor + 2 ^ L
          · rw [h2n] at he2
            rw [heqNb j2] at he2
            obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
              packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j2 e2 he2
            have hu := inv.unique st.splitCursor k1 e1 st.splitCursor k2 e2
              hklt1 hklt2 hfe1 hfe2 hkeys
            exact ⟨by rw [h1n, h2n], by
              obtain ⟨_, hkeq⟩ := hu
              rw [← hc1, ← hc2, hkeq]⟩
          · exfalso
            have hv : st.buckets b2 j2 = some e2 :=
              (heqElse b2 h2s h2n j2).symm.trans he2
            have hu := inv.unique st.splitCursor k1 e1 b2 j2 e2 hklt1 hj2 hfe1 hv hkeys
            obtain ⟨heqb, _⟩ := hu
            exact h2s heqb.symm
      · by_cases h2s : b2 = st.splitCursor
        · rw [h2s] at he2
          rw [heqSrc j2] at he2
          obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
            packImg_spec (srcImageFun st) (srcPred st g) SlotsPerBucket j2 e2 he2
          exfalso
          have hv : st.buckets b1 j1 = some e1 :=
            (heqElse b1 h1s h1n j1).symm.trans he1
          have hu := inv.unique b1 j1 e1 st.splitCursor k2 e2 hj1 hklt2 hv hfe2 hkeys
          obtain ⟨heqb, _⟩ := hu
          exact h1s heqb
        · by_cases h2n : b2 = st.splitCursor + 2 ^ L
          · rw [h2n] at he2
            rw [heqNb j2] at he2
            obtain ⟨k2, hklt2, hfe2, hps2, hc2⟩ :=
              packImg_spec (srcImageFun st) (newPred st g) SlotsPerBucket j2 e2 he2
            exfalso
            have hv : st.buckets b1 j1 = some e1 :=
              (heqElse b1 h1s h1n j1).symm.trans he1
            have hu := inv.unique b1 j1 e1 st.splitCursor k2 e2 hj1 hklt2 hv hfe2 hkeys
            obtain ⟨heqb, _⟩ := hu
            exact h1s heqb
          · have hv1 : st.buckets b1 j1 = some e1 :=
              (heqElse b1 h1s h1n j1).symm.trans he1
            have hv2 : st.buckets b2 j2 = some e2 :=
              (heqElse b2 h2s h2n j2).symm.trans he2
            exact inv.unique b1 j1 e1 b2 j2 e2 hj1 hj2 hv1 hv2 hkeys

/-- **Stage-4 headline**: one successful maintenance split preserves `Inv`.
Mirrors map.rs `plan_split` (L1453-L1557, `insert = none`) with the counter
recomputation of `finish_split_plan` (L1575-L1620). -/
theorem inv_split_transfer {st : MapState K V} (inv : Inv st) {g : Geometry}
    (hg : nextGeometry st.level st.splitCursor st.physicalBuckets = some g) :
    Inv (splitState st g) := by
  rcases nextGeometry_cases hg with
    ⟨glev, gcur, gbk, h63⟩ | ⟨hceq, glev, gcur, gbk, h63⟩
  · refine inv_split_transfer_aux inv rfl rfl inv.geomBucketsEq hg glev gcur gbk
      inv.geomLevelLow h63 ?_ ?_ rfl
    · -- P3: a successful step keeps the new cursor inside the (unchanged) level bound
      have hshape := (next_geometry_shape st.level st.splitCursor st.physicalBuckets
        inv.geomCursorBound g hg).1
      rw [glev, gcur] at hshape
      exact hshape
    · have hb := inv.geomBucketsEq
      omega
  · refine inv_split_transfer_aux inv rfl rfl inv.geomBucketsEq hg glev gcur gbk
      (by have hb := inv.geomLevelLow; omega) h63 (two_pow_pos _) ?_ rfl
    · have hp1 : 2 ^ (st.level + 1) = 2 * 2 ^ st.level := two_pow_succ_eq _
      have hb := inv.geomBucketsEq
      omega

end Lhm.Abs
