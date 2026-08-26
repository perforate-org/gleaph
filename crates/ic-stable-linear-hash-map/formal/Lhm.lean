/-
Permanent Lean verification project for `ic-stable-linear-hash-map`.

Stage 1-2 (routing mathematics + control-region invariants) and Stage 3 (logical map
specification: transfer principle, insert-update / insert-place / remove / clear /
reset preservation, and the top-level `opInsert` / `opRemove` contracts) are verified
here with no `sorry`. See SCOPE.md for the contract and REPORT.md for audit findings.

The `#print axioms` lines below make every build print the axiom dependencies of each
headline theorem: a regression to `sorry` shows up as `sorryAx` in `lake build` output.
-/
import Lhm.Basic
import Lhm.Routing
import Lhm.Control
import Lhm.Abs.Transfer
import Lhm.Abs.Preserve
import Lhm.Abs.Place
import Lhm.Abs.Cleared
import Lhm.Abs.OpPreserve
import Lhm.Abs.Split

namespace Lhm

#print axioms Lhm.route_lt_base_plus_cursor
#print axioms Lhm.split_stability_level_up
#print axioms Lhm.split_stability_cursor_adv
#print axioms Lhm.next_geometry_shape
#print axioms Lhm.split_threshold_mono
#print axioms Lhm.split_threshold_le_capacity
#print axioms Lhm.split_threshold_lt_capacity
#print axioms Lhm.initialControl_valid
#print axioms Lhm.route_in_extent
#print axioms Lhm.next_geometry_from_valid
#print axioms Lhm.Abs.inv_setValue
#print axioms Lhm.Abs.inv_transfer
#print axioms Lhm.Abs.inv_place
#print axioms Lhm.Abs.inv_clearSlot
#print axioms Lhm.Abs.inv_set_incarnation
#print axioms Lhm.Abs.inv_cleared
#print axioms Lhm.Abs.inv_reset
#print axioms Lhm.Abs.opInsert_preserves
#print axioms Lhm.Abs.opRemove_preserves
#print axioms Lhm.Abs.inv_split_transfer

end Lhm
