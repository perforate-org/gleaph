/-
Permanent Lean verification project for `ic-stable-vec-deque` (block-ring
layout, ADR 0086).

Stage 1' (block-routing arithmetic), Stage 2' (V1 header validity) and Stage 4'
(zero-movement growth) are verified here. The Stage 3' scaffold (`Svd/Abs/*`:
state, operation definitions, transfer lemmas) is present but its observable
operation contracts and `Inv` preservation are still being written. See
SCOPE.md for the verified-vs-open boundary and REPORT.md for the record.
-/
import Svd.Basic
import Svd.BlockRing
import Svd.BlockHeader
import Svd.Abs.State
import Svd.Abs.Transfer
import Svd.Abs.Ops
import Svd.Abs.Grow
import Svd.Abs.Preserve

namespace Svd

#print axioms Svd.virtualIndex_lt
#print axioms Svd.virtualIndex_injective
#print axioms Svd.virtual_pos_decomp
#print axioms Svd.blockOf_lt
#print axioms Svd.slotAddr_lt_memBytes
#print axioms Svd.rotateIndex_injective
#print axioms Svd.initialHeader_valid

end Svd

namespace Svd.Abs

#print axioms Svd.Abs.grow_preserves_contentOf

-- Stage 3' operation/preservation axiom guards are restored once their
-- contracts are written; none of those are claimed yet.

end Svd.Abs
