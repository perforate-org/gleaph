# 2026-11-07: the inline value schema family, measured

ADR 0100's last remaining family is inline value schema: "width declaration, 0-to-w migration, materialisation
scheduling". This measures it before a cut, in the form the previous two investigation records established.

## The three questions, and where each currently lives

| Question | Entry points | Callers |
| --- | --- | --- |
| Declare a bucket's width | `ensure_label_bucket_inline_property_byte_width` (+ `ensure_bucket_inline_property_byte_width_on_slot`) | 132 |
| Migrate a bucket from width 0 to a width | `materialize_inline_property_stream` (whole) and `materialize_inline_property_stream_step` (incremental, cursor-driven) | 15 / 20 |
| Schedule the migration | the bidirectional maintenance queue's owner/worker, driven by the step form | - |
| Write a width into a descriptor (no migration) | `with_inline_property_byte_width` | 9 |

## What is already shared

The migration is not two implementations of a copy; it is one migration with two drivers:

- `materialize_source` is the single admission (vertex, bucket, mode, width, accounting) and both drivers call it;
- `MaterializeCursor` carries the reservation state, and both drivers use it;
- `materialize_compaction_before_reserve` and `prepare_materialize_publication` are the shared steps, invoked from
  both.

The difference between the two drivers is *who owns the loop*: the whole form runs it inline under a byte budget
(`INLINE_MATERIALIZE_BYTE_BUDGET`), the step form runs one step per maintenance call and persists the cursor so a
reopen resumes. That is the same shape the tree deepen and the batch cursors had - one operation, two drivers - and
it is the reason this family is medium rather than large.

## Where the duplication actually is

Nine sites write `with_inline_property_byte_width` directly, while declaration has its own entry with 132 callers.
Those nine are the width writers that bypass the declaration's admission: some are legitimate (the promotion
rewrites a descriptor it has just built, ADR 0101's split, a fixture), some may be a declaration written by hand.

That is the cut worth making, and it is small:

1. `BucketValueSchema` view: `declare(src, label, width)` is the current declaration entry, and
   `migrate(src, bucket_slot, width, budget)` drives the same cursor either inline or one step at a time - the
   caller says which, the view keeps the loop.
2. Every direct `with_inline_property_byte_width` is either routed through `declare` or carries a comment saying why
   it is a descriptor rewrite rather than a declaration (the ADR 0101 value-root split is the real case).
3. The maintenance queue keeps its role as the driver, and asks the view for a step rather than the graph.

The number to beat: **9 direct width writes -> each either routed through the declaration or justified in place.**

## Why this is its own session

The width is persisted schema and the cursor is persisted progress; the reopen tests in `bidirectional/deferred.rs`
already cover the retry and cancellation cases (`materialize_owner_*`). A cut that renames the driver has to keep
those green, and the 132 declaration callers must keep compiling while the view appears - the same constraint the
Visit and bucket lookup families had.
