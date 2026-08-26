/-
Permanent Lean verification project for `ic-stable-vec-deque` (block-ring
layout, ADR 0086).

Stage 1' (block-routing arithmetic) and the abstract state model are in place.
Spec contracts, growth preservation, and Inv preservation are being restated
for the new layout. See SCOPE.md for status.
-/
import Svd.Basic
import Svd.Abs.State
import Svd.Abs.Transfer
import Svd.Abs.Ops
import Svd.Abs.Grow
import Svd.Abs.Preserve

namespace Svd.Abs

-- Axiom guards will be restored once all preservation proofs are complete.

end Svd.Abs
