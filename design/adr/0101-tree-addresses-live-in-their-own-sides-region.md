# 0101. A tree's block addresses live in their side's region

Date: 2026-11-05
Status: Decided, not implemented
Anchor timestamp: 2026-11-05 20:55:00 UTC +0000

Related: [ADR 0096](0096-inline-tiny-mode-for-low-degree-label-buckets.md) owns the bucket modes;
[ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md) owns the per-bucket runs;
[ADR 0099](0099-inline-property-values-are-slot-addressed.md) owns the value side's slot addressing.

## Context

A tree-mode bucket has two independent trees: one over edges, one over inline property values. Both are
block-addressed: a node stores up to `R_MAX` (1024) child addresses of 4 bytes each (`PROPERTY_ID_BYTES`),
a block holds `B` of them, and the tree deepens to `MAX_DEPTH` (3) as the block count outgrows the fan-out.

The edge side keeps its addresses in the leaf's slot space, which is what `LabeledVertex::stored_slots`
(VertexEdgeSpan width) measures. The value side keeps its data in the inline property bytes store, addressed
in bytes.

The code currently puts **both** roots in the edge region: `TreeSpans::property_root` derives
`edge_start + edge_root_len`, and `combined_span_region_len` sums the two, so `bucket_content_region` reports
one contiguous edge-side span for a bucket whose value root is not edge data. Two consequences, both observed:

- The reported edge region grows when the value root grows, so it can overlap a sibling bucket's region after a
  placement that was correct when it happened. Compaction then cannot move the overlapping spans and fails
  with `CollectAllocationOverflow`.
- The value root's address is a function of the edge geometry, so the two cannot be laid out independently.

## Decision

**A tree's addresses are stored in the region that owns the tree.** Edge-tree addresses live in the leaf's
slot space; value-tree addresses live in the inline property bytes store.

- The two sides share the structure and the arithmetic: entries of 4 bytes, `B` per block, `R_MAX` fan-out,
  `MAX_DEPTH` deepening. The type that expresses this must not be named after the root: interior (branch)
  nodes hold the same array, so the abstraction is the **block address array** that any node stores.
- The shared unit is the **entry** (one 4-byte child address). Each side converts it to its own storage unit
  at the boundary: the edge side to leaf slots, the value side to bytes.
- `LabeledVertex::stored_slots` (the edge cover) measures the **edge** region only. A bucket's value root never
  contributes to it, and `bucket_content_region` reports edge geometry only.
- The value root's address is a value-store address, recorded where the value side's own geometry lives.

## Consequences

- `TreeSpans` stops being one span: the edge root and the value root are separate spans in separate address
  spaces. `combined_span_region_len`, `combined()`, and the derived `property_root()` offset are removed.
- The value side needs its root region allocated and accounted in its own store, with the same deepening rule
  as the edge side.
- Region-disjointness becomes checkable per side, which is what the compaction failure was telling us: the
  edge regions overlapped because the value root was folded into them.

## Status

Decided, not implemented. The steps are: ADR (this), the shared block-address-array type, the edge geometry
losing the value root, the value side gaining its own root region, then the read/write paths and the
disjointness tests.
