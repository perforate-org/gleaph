/-
Stage 3 transfer principle: an updated state inherits `Inv` from the original state
whenever the update preserves slot *occupancy* pointwise and supplies geometry,
counter, placement, and uniqueness facts relative to the original state. All
operation-preservation theorems factor through this lemma.
-/
import Lhm.Abs.Ops

namespace Lhm.Abs

open Lhm

variable {K V : Type}

theorem totalLenOf_eq (st : MapState K V) (n : Nat) :
    totalLenOf st n = totalLoads (fun x => loadOf st x) n := rfl

theorem totalOvfOf_eq (st : MapState K V) (n : Nat) :
    totalOvfOf st n = totalLoads (fun x => ovfLoadOf st x) n := rfl

/-- Bucket-load congruence under pointwise isSome-agreement. -/
theorem loadOf_congr_occ {st s2 : MapState K V} {x : Nat}
    (h : ∀ y, (s2.buckets x y).isSome = (st.buckets x y).isSome) :
    loadOf s2 x = loadOf st x :=
  countMatch_congr _ _ SlotsPerBucket (fun i _ => h i)

/-- Inline-overflow congruence under pointwise isSome-agreement. -/
theorem ovfLoadOf_congr_occ {st s2 : MapState K V} {x : Nat}
    (h : ∀ y, (s2.buckets x y).isSome = (st.buckets x y).isSome) :
    ovfLoadOf s2 x = ovfLoadOf st x :=
  countMatch_congr _ _ SlotsPerBucket (fun i _ => by
    simp only [ovfPred]
    rw [h i])

/-- Candidate membership transfers when the two states share routing hashes and
geometry fields. -/
theorem cand1_congr {st s2 : MapState K V} (hh1 : s2.hash1 = st.hash1)
    (hlev : s2.level = st.level) (hcur : s2.splitCursor = st.splitCursor) (k : K) :
    cand1 s2 k = cand1 st k := by
  unfold cand1
  rw [hh1, hlev, hcur]

theorem cand2_congr {st s2 : MapState K V} (hh2 : s2.hash2 = st.hash2)
    (hlev : s2.level = st.level) (hcur : s2.splitCursor = st.splitCursor) (k : K) :
    cand2 s2 k = cand2 st k := by
  unfold cand2
  rw [hh2, hlev, hcur]

theorem InCands_congr {st s2 : MapState K V} (hh1 : s2.hash1 = st.hash1)
    (hh2 : s2.hash2 = st.hash2) (hlev : s2.level = st.level)
    (hcur : s2.splitCursor = st.splitCursor) {k : K} {c : Nat}
    (hin : InCands st k c) :
    InCands s2 k c := by
  unfold InCands at hin ⊢
  rcases hin with h | h
  · left
    rw [h, cand1_congr hh1 hlev hcur]
  · right
    rw [h, cand2_congr hh2 hlev hcur]

/-- Transfer `Inv` across an occupancy-preserving state update whose placement and
uniqueness facts are supplied relative to the original state. -/
theorem inv_transfer {st s2 : MapState K V} (inv : Inv st)
    (hisSome : ∀ x y, (s2.buckets x y).isSome = (st.buckets x y).isSome)
    (hh1 : s2.hash1 = st.hash1) (hh2 : s2.hash2 = st.hash2)
    (hlev : s2.level = st.level) (hcur : s2.splitCursor = st.splitCursor)
    (hpb : s2.physicalBuckets = st.physicalBuckets)
    (hepEven : s2.mutationEpoch % 2 = 0)
    (hinc : s2.incarnation = st.incarnation)
    (hlen : s2.len = st.len)
    (hcLen : st.len = totalLenOf st st.physicalBuckets)
    (hovf : s2.overflowEntries = st.overflowEntries)
    (hcOvf : st.overflowEntries = totalOvfOf st st.physicalBuckets)
    (hplaced : ∀ b j e, s2.buckets b j = some e →
        b < s2.physicalBuckets ∧ j < SlotsPerBucket ∧ InCands st e.1 b)
    (huq : ∀ b1 j1 e1 b2 j2 e2, j1 < SlotsPerBucket → j2 < SlotsPerBucket →
        s2.buckets b1 j1 = some e1 → s2.buckets b2 j2 = some e2 →
        e1.1 = e2.1 → b1 = b2 ∧ j1 = j2) :
    Inv s2 := by
  have hbLoad : ∀ x, loadOf s2 x = loadOf st x :=
    fun x => countMatch_congr _ _ SlotsPerBucket (fun i _ => hisSome x i)
  have hbOvf : ∀ x, ovfLoadOf s2 x = ovfLoadOf st x :=
    fun x => countMatch_congr _ _ SlotsPerBucket (fun i _ => by
      simp only [ovfPred]
      rw [hisSome x i])
  constructor
  · rw [hlev]; exact inv.geomLevelLow
  · rw [hlev]; exact inv.geomLevelHigh
  · rw [hcur, hlev]; exact inv.geomCursorBound
  · rw [hpb, hlev, hcur]; exact inv.geomBucketsEq
  · exact hepEven
  · rw [hinc]; exact inv.geomIncarnationPos
  · show s2.len = totalLoads (fun x => loadOf s2 x) s2.physicalBuckets
    rw [hpb,
      totalLoads_congr (f := fun x => loadOf s2 x) (g := fun x => loadOf st x)
        st.physicalBuckets (fun x _ => hbLoad x),
      ← totalLenOf_eq]
    exact hlen.trans hcLen
  · show s2.overflowEntries = totalLoads (fun x => ovfLoadOf s2 x) s2.physicalBuckets
    rw [hpb,
      totalLoads_congr (f := fun x => ovfLoadOf s2 x) (g := fun x => ovfLoadOf st x)
        st.physicalBuckets (fun x _ => hbOvf x),
      ← totalOvfOf_eq]
    exact hovf.trans hcOvf
  · intro b j e he
    refine ⟨?_, ?_, ?_⟩
    · exact (hplaced b j e he).1
    · exact (hplaced b j e he).2.1
    · exact InCands_congr hh1 hh2 hlev hcur (hplaced b j e he).2.2
  · intro b1 j1 e1 b2 j2 e2 hj1 hj2 l1 l2 hkeys
    exact huq b1 j1 e1 b2 j2 e2 hj1 hj2 l1 l2 hkeys

end Lhm.Abs
