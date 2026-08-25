/-
Permanent Lean verification project for `ic-stable-linear-hash-map`.

Stage 1-2 (routing mathematics + control-region invariants) and the first slice of
Stage 3 (setValue preservation via the transfer principle) are verified here with no
`sorry`. See SCOPE.md for the contract and REPORT.md for audit findings.

The `#print axioms` lines below make every build print the axiom dependencies of each
headline theorem: a regression to `sorry` shows up as `sorryAx` in `lake build` output.
-/
import Lhm.Basic
import Lhm.Routing
import Lhm.Control
import Lhm.Abs.Transfer
import Lhm.Abs.Preserve
import Lhm.Abs.Place

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

end Lhm
