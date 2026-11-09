/-
Permanent Lean verification project for `ic-stable-lara` (Labeled LARA).

Stage 1 (record/slot arithmetic): 36-bit slot index space, locator words,
vertex tail28, and BucketLabelKey semantics. See SCOPE.md for the staged
contract and REPORT.md for transcription findings.
-/
import Lar.Basic
import Lar.SlotIndex
import Lar.BucketLabelKey

namespace Lar

#print axioms Lar.pack2_inj
#print axioms Lar.checkedAddSlotIndex_spec
#print axioms Lar.locator_decode_of_tryEncode
#print axioms Lar.unpack_pack_canonical

end Lar
