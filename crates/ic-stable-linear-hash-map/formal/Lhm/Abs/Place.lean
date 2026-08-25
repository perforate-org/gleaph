/-
Stage 3 preservation, part 2: fresh placement (`placeAt`) preserves `Inv`.
-/
import Lhm.Abs.Deltas
import Lhm.Abs.Preserve

namespace Lhm.Abs

open Lhm

variable {K V : Type}

/-! ## Pointwise facts about placeAt -/

theorem placeAt_buckets {st : MapState K V} {k : K} {v : V} {b j : Nat} :
    (placeAt st k v b j).buckets = setBucketEntry st.buckets b j (some (k, v)) := rfl

theorem placeAt_buckets_ne {st : MapState K V} {k : K} {v : V} {b j y : Nat} (hy : y ≠ j) :
    (placeAt st k v b j).buckets b y = st.buckets b y := by
  rw [placeAt_buckets]
  simp [setBucketEntry, hy]

theorem placeAt_buckets_at {st : MapState K V} {k : K} {v : V} {b j : Nat} :
    (placeAt st k v b j).buckets b j = some (k, v) := by
  rw [placeAt_buckets]
  simp [setBucketEntry]

theorem placeAt_buckets_xne {st : MapState K V} {k : K} {v : V} {b x : Nat} (hx : x ≠ b)
    (j y : Nat) :
    (placeAt st k v b j).buckets x y = st.buckets x y := by
  rw [placeAt_buckets]
  simp [setBucketEntry, hx]

/-! ## Counter deltas for placement -/

end Lhm.Abs
