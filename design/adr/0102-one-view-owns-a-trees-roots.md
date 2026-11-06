# 0102. One view owns a tree bucket's roots

Date: 2026-11-06
Status: Accepted
Anchor timestamp: 2026-11-06 01:05:00 UTC +0000

Related: [ADR 0100](0100-one-path-per-operation-with-a-view.md) owns the view rule;
[ADR 0101](0101-tree-addresses-live-in-their-own-sides-region.md) owns where the addresses live;
[ADR 0097](0097-per-bucket-spill-runs-replace-the-shared-overflow-log.md) owns the ordinal runs.

## Context

A tree-mode bucket has two roots - one over edges, one over inline property values - and both are written
today by six independent functions:

| Path | Function |
| --- | --- |
| promotion | `promote_bucket_to_tree_mode_impl` |
| append with tail room | `tree_mode_tail_append` |
| append at a block boundary | `tree_mode_tail_append_depth1` |
| append at depth >= 2 | `tree_mode_tail_append_depth_ge2` |
| value-tree deepen | `property_deepen` |
| edge-tree deepen | `tree_mode_deepen` |

Each builds a `[edge root | property root]` image and writes it with `write_slots_contiguous_bytes`, each
decides its own ordering of "write roots" versus "publish descriptor", and each carries its own rollback.
ADR 0101 makes this untenable: the value root moves to the value store, so every one of those sites would
have to learn a second store and a second address space.

That is exactly the shape ADR 0100 rejects: an operation (publish a tree bucket's roots) implemented by six
paths. A fix applied to one path leaves five.

## Decision

**`BucketTreePlane` owns writing a tree bucket's roots**, in a new module `labeled/graph/tree_plane.rs`. It
is the fifth of the graph's views, alongside `BucketRowPlane`, `BucketValuePlane`, `BucketLayout` and
`VertexLayout`.

- Its constructor is the only way in (ADR 0100 rule 2): `graph.tree_plane(bucket_slot, bucket)`.
- Its one write entry publishes **both** roots and then the descriptor, in that order, once. A caller does
  not write root bytes, does not order the publication, and does not know which store a root belongs to.
- The store-level write helpers move **inside** that module and become private there. That is the barrier: a
  free function in `tree_write.rs` would not be one, because the five sibling functions in the same module
  could keep writing directly and nothing would say so.
- The read side joins the same view: descending a root and reading its children is the same ownership
  question, and ADR 0101's "which store" must be answered in one place for reads as well. `tree_read.rs`
  keeps the walking code and asks the view for addresses.

The invariant the view enforces:

1. The two roots are written **together**: there is no reachable state where the edge root names a new block
   and the value root still names the old one.
2. A value-root region is allocated **and** the one it replaces is released in the same operation.
3. The descriptor is published **after** both writes, once, and a failure before it leaves the old descriptor
   naming the old roots.
4. After a deepen (which changes the physical depth and the edge base) the descriptor is re-read before it is
   published: the stale-descriptor hazard is handled here, not by every caller.

## Consequences

- The six call paths become callers of one entry; `grep write_slots_contiguous_bytes` over root images should
  find only `tree_plane.rs`.
- The combined-length arithmetic (`old_edge_root_len + old_property_root_len`) leaves the callers entirely;
  the view derives the geometry it needs from the descriptor.
- The ADR 0101 switch becomes a change inside this module instead of a change across five files.
- Per-operation cut plan: (S1) the view and its entry, (S2) `tree_mode_tail_append_depth1`, (S3) the depth-2
  append and `tree_mode_deepen`, (S4) promotion and `property_deepen`, (S5) verify with `grep` that no root
  image is written outside the module, (S6) apply ADR 0101 inside the view.

## Status

Accepted and implemented through S6. Every path that writes a tree bucket's roots calls
`BucketTreePlane::publish`, which also publishes the descriptor; `grep write_slots_contiguous_bytes` over the
production graph code finds root writes only in `tree_plane.rs` and the two rollback restores. The read side
(`value_child`, `value_children`) is the view's too, so "which store holds which root" is answered in one
module - which is what let ADR 0101's split land as a change inside it.

Decided, not implemented. The write paths named above are the ones to convert, in the order given.
