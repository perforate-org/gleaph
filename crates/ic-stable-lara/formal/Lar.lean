/-
Permanent Lean verification project for `ic-stable-lara` (Labeled LARA).

Stage 1 (record/slot arithmetic): LogHead codec, 36-bit slot index space,
locator words, vertex tail28, BucketLabelKey, bucket word packing, and
LabelBucket validation. See SCOPE.md for the staged contract and REPORT.md
for transcription findings.
-/
import Lar.Basic
import Lar.LogHead
import Lar.SlotIndex
import Lar.BucketLabelKey
import Lar.BucketWord
import Lar.LabelBucket

namespace Lar

#print axioms Lar.pack2_inj
#print axioms Lar.decode_of_tryEncode
#print axioms Lar.checkedAddSlotIndex_spec
#print axioms Lar.locator_decode_of_tryEncode
#print axioms Lar.unpack_pack_canonical
#print axioms Lar.bucketWord_decode_of_tryEncode
#print axioms Lar.encodeBucketWord_reserved_zero
#print axioms Lar.tryReadFromFields_ok_of_wireValid
#print axioms Lar.wireValid_of_tryReadFromFields_ok

end Lar
