# Edge Identity and Physical Key Design

## Status

- Draft
- Motivation: recover read-path performance after `edge_id`-centric unification while preserving correct multi-edge support

## Problem

Recent changes unified more edge-related state around `edge_id`. That improved identity consistency and multi-edge correctness, but it also introduced broad benchmark regressions.

The core issue is not that `edge_id` exists. The issue is that too much of the hot read path became effectively `edge_id`-centric.

That caused three classes of regressions:

1. query-time revalidation of `src/dst/label` after looking up edge ids
2. repeated tombstone / label / metadata lookups from hot traversal loops
3. heavier write and rebuild paths due to larger canonical edge state and more index maintenance

We already mitigated the first two in some targeted places:

- cached tombstone state on adjacency/reverse entries
- `by_src` / `by_dst` edge property index projections
- executor changes that avoid per-edge tombstone re-lookups

Those fixes helped, but the underlying design tension remains.

## Goal

Support all of the following at once:

- correct multi-edge semantics
- stable edge identity for mutation and upgrade logic
- fast forward and reverse adjacency traversal
- fast shape-specific property lookup
- minimal hot-path dependence on secondary lookups

## Main Principle

Separate **logical identity** from **physical access order**.

- `edge_id` should be the canonical logical identity
- adjacency storage should be ordered for traversal locality

This is a standard database tradeoff:

- logical key for uniqueness and updates
- clustered/physical key for scan locality

## VCSR Ground Rule

At the storage-kernel level, Gleaph should stay aligned with the VCSR-style
model described in the repository references:

- adjacency is vertex-centric
- physical iteration is endpoint-major
- neighborhood scans are the primitive operation
- update structures may be auxiliary, but they must not become the primary
  read-path representation

That means `edge_id` may exist as a semantic identifier, but it should not sit
in the middle of ordinary traversal in a way that forces:

- `edge_id -> overlay -> src/dst/label` recovery
- mixed-bucket scans followed by late identification
- query-time rediscovery of facts already implied by adjacency order

In other words:

- VCSR principle: "start from the vertex, walk the neighborhood"
- anti-pattern: "start from an auxiliary edge identity, then reconstruct the neighborhood"

The recent regressions strongly suggest that Gleaph drifted toward the second
shape in some generic paths, especially reverse traversal and edge-property
lookup.

## DGAP Ground Rule

DGAP reinforces the same conclusion from the update side.

From the reference implementation:

- the primary graph shape remains PMA-CSR
- the log exists as an auxiliary mutation structure
- analysis still runs through vertex-centric neighborhood iteration
- update machinery does not replace adjacency as the core read abstraction

In other words, DGAP adds:

- per-segment overflow logs
- recovery support
- persistent-memory-friendly update handling

but it still preserves the same low-level discipline:

- start from a vertex
- iterate a neighborhood
- merge in buffered updates as part of that neighborhood view

It does **not** make a logical edge identifier the primary traversal axis.

That matters for Gleaph because `edge_id` is useful for semantics and mutation,
but DGAP suggests it should behave more like:

- update metadata
- mutation indirection
- recovery / persistence handle

and less like:

- the intermediate representation used by generic query execution

So the combined VCSR + DGAP reading is:

- VCSR tells us what the read path should look like
- DGAP tells us how to add dynamic-update machinery without breaking that read path

## Consequence for `edge_id`

`edge_id` should therefore be treated as:

- semantic identity
- mutation target
- overlay/log key
- stable upgrade handle

but **not** as the primary traversal axis.

If a design choice makes generic graph reads depend on `edge_id` as an
intermediate representation, that choice should be considered suspect unless it
clearly matches or beats the simpler VCSR-style adjacency walk in benchmarks.

## Proposed Model

## 1. Canonical identity stays `edge_id`

`edge_id` remains the unique handle for:

- mutation targeting
- stable serialization / upgrade semantics
- overlays and logs
- external references to specific edges

This keeps multi-edge support straightforward because even identical `(src, dst, label_id)` edges remain distinct.

## 2. Physical forward order becomes `(src, dst, label_id, edge_id)`

Forward adjacency should be laid out in source-major order, with `edge_id` only as the final tie-breaker.

That gives:

- fast `src -> neighbors` iteration
- stable support for multi-edges on identical `(src, dst, label_id)`
- deterministic traversal order

This restores the main performance property we used to rely on: neighborhood access is physically aligned with the query pattern.

## 3. Physical reverse order becomes `(dst, src, label_id, edge_id)`

Reverse traversal should mirror the same idea for incoming edges.

That gives:

- fast incoming-neighbor scans
- no need to rediscover `src`/`label` by chasing `edge_id`
- symmetric support for multi-edge traversal

## 4. Hot metadata stays inline in physical entries

Physical forward and reverse entries should include the hot-path fields needed during traversal:

- packed `label_and_flags`
- `edge_id`
- endpoint id (`target` for forward, `src` for reverse)
- optionally `timestamp` or other already-hot fields

Hot loops should not need to look up another structure just to answer:

- is this edge tombstoned?
- what label is this edge?
- which specific edge instance is this?
`edge_id` is still present, but it should not be required for every branch in the loop.

### Forward path

This is already the right direction. `EdgeEntry` already carries packed label/flag information, so forward traversal does not need an additional duplicate copy of tombstone or label state.

### Reverse path

`RevEntry` should follow the same pattern. Instead of storing `label_id` and tombstone state as separate fields, reverse entries should pack them into the same `label_and_flags` representation used by `EdgeEntry`.

That gives:

- the same `label_id()` / `is_tombstoned()` access pattern in both directions
- denser storage than separate `u32 + bool` fields in practice
- less synchronization complexity when toggling tombstones or changing labels

## 5. Canonical metadata stays keyed by `edge_id`

Overlay state, logs, upgrade state, and any sparse mutable metadata should remain keyed by `edge_id`.

Examples:

- edge property overlay
- mutation log entries
- edge-specific stable upgrade metadata

This keeps update semantics simple and keeps the hot adjacency entries relatively compact.

## 6. Add an `edge_id -> locator` helper if needed

If update-heavy paths need direct access from `edge_id` to the stored edge entry, add a helper structure such as:

```rust
edge_locator_by_id: edge_id -> EdgeLocator
```

Where `EdgeLocator` is enough to find and mutate the physical entry quickly.

This avoids needing the physical layout itself to be `edge_id`-ordered.

## Why This Works

## Multi-edge correctness

With `(src, dst, label_id, edge_id)` as the forward key:

- identical `(src, dst, label_id)` edges are distinct
- updates can still target a specific edge via `edge_id`
- traversal sees all edge instances without ambiguity

## Read-path performance

Most graph queries are fundamentally adjacency queries, not arbitrary edge-id lookups. Their hot loops want:

- all outgoing edges for `src`
- all incoming edges for `dst`
- label/tombstone decisions inline

The proposed structure restores that locality.

## Update-path correctness

`edge_id` still provides a stable identity for:

- precise delete / revive
- property mutation on one edge among many
- stable-memory roundtrips

## Relation to Recent Fixes

The recent targeted fixes are still valid under this design.

### `by_src` / `by_dst` edge property indexes

These are still useful. Even with better physical adjacency order, property equality queries such as:

- `edge_index_targets_for_src`
- `edge_index_sources_for_dst`

benefit from query-shape-specific projected indexes.

### Cached tombstone state on physical entries

This becomes part of the core design rather than a patch. Traversal should always prefer metadata already stored in the physical forward/reverse entry over re-looking up canonical state.

### Specialized aggregate fast paths

These continue to compose well with the design because they rely on cheap adjacency scans.

## Data Structure Sketch

## Forward entry

```rust
struct EdgeEntry {
    target: u32,
    timestamp: u64,
    weight: f32,
    label_and_flags: u32,
    edge_id: u32,
}
```

The physical sort key is effectively:

```text
(src, target, label_id, edge_id)
```

## Reverse entry

```rust
struct RevEntry {
    src: u32,
    timestamp: u64,
    weight: f32,
    label_and_flags: u32,
    edge_id: u32,
}
```

The physical or indexed access pattern is:

```text
(dst, src, label_id, edge_id)
```

The recommended API mirrors `EdgeEntry`:

```rust
impl RevEntry {
    fn label_id(&self) -> u32 { ... }
    fn is_tombstoned(&self) -> bool { ... }
}
```

## Canonical overlay state

```rust
edge_props: HashMap<edge_id, EdgePropsOverlay>
edge_locator_by_id: HashMap<edge_id, EdgeLocator>   // optional
```

## Tradeoffs

## Advantages

- preserves multi-edge support cleanly
- restores adjacency-local scan performance
- avoids per-edge re-lookups in hot loops
- keeps mutation semantics stable
- fits current targeted fixes instead of fighting them

## Costs

- duplicated metadata between physical entries and canonical edge state
- additional maintenance to keep tombstone/label state synchronized
- possible extra memory if `edge_locator_by_id` is introduced

These costs are acceptable because they buy back the main benchmark regressions on the read path.

## Non-Goals

- making all query shapes fast through physical layout alone
- eliminating query-shape-specific secondary indexes
- replacing future factorized / top-k / semijoin optimizations

This design is storage- and adjacency-level groundwork, not a complete query optimizer.

## Migration Strategy

## Phase 1: document and preserve the layering

Keep the current principle explicit:

- canonical identity is `edge_id`
- traversal must use adjacency-local metadata whenever possible

## Phase 2: restore/ensure physical ordering assumptions

Audit forward and reverse traversal code to ensure it depends on:

- adjacency-local tombstone state
- adjacency-local label state
- adjacency-major iteration order

and not on repeated canonical lookups.

## Phase 3: add `edge_id -> locator` only if mutation paths need it

Do not add it preemptively unless benchmarks or code complexity justify it.

## Phase 4: re-evaluate write-heavy benchmarks

After the physical/read-path layering is settled, re-measure:

- bulk insert
- rebalance
- resize
- upgrade roundtrip

That is the right time to decide whether write-path structures need further compaction.

## Expected Benchmark Impact

This design should improve or protect:

- neighbor enumeration
- reverse traversal
- grouped 1-hop aggregates
- top-k endpoint-grouped aggregates
- edge property lookup by `src`/`dst`

It will not by itself solve:

- cyclic join explosion
- multi-hop acyclic pruning
- top-k early stop for arbitrary ranking expressions
- repeated analytics that really want maintained summaries

## Recommendation

Adopt the layered model explicitly:

- logical identity: `edge_id`
- forward physical key: `(src, dst, label_id, edge_id)`
- reverse physical key: `(dst, src, label_id, edge_id)`
- hot traversal metadata duplicated in adjacency entries
- sparse mutable metadata keyed by `edge_id`

This is the most practical way to preserve the benefits of `edge_id` while recovering the performance characteristics that graph workloads actually need.

## Partial Rollback Strategy

If restoring read-path locality requires undoing part of the `edge_id`-centric
design, prefer a **partial rollback** over a full semantic rollback.

The goal is:

- keep the unified and semantically clear parts of the model
- roll back only the physical organization that made generic queries slower
- prefer the simpler design when two options have comparable performance

### What should stay

These parts are still worth keeping unless measurement proves otherwise:

- `edge_id` as the canonical logical identity
- `edge_id`-keyed overlay / log / upgrade metadata
- multi-edge semantics based on distinct logical edge instances
- inline hot metadata on `EdgeEntry` / `RevEntry`

These give cleaner semantics for:

- precise mutation targeting
- stable replay / restore behavior
- distinguishing multiple edges with the same endpoints and label

### What is a valid rollback target

These parts are good candidates to move back toward the pre-`edge_id` design:

- reverse physical organization that forces mixed-label scans before filtering
- forward/reverse traversal APIs that rediscover label or endpoint facts too late
- query-shape indexes that lost `(src,dst,label)` locality by collapsing to `edge_id`
- any generic traversal path whose hot loop became “scan first, identify later”

In practice, that means:

- physical access paths may become endpoint-major again
- reverse access may become explicitly label-aware again
- projected indexes may be keyed by endpoint tuples even if canonical state stays keyed by `edge_id`

### Preferred rollback form

The preferred rollback is:

- keep `edge_id` in the entry
- restore endpoint-major physical ordering
- restore label-aware reverse access
- keep mutable canonical metadata keyed by `edge_id`

This is better than a full revert because it preserves the clearer semantic
model while moving hot query execution back to the simpler, faster access path.

### When full rollback becomes reasonable

A fuller rollback should be considered only if all of the following happen:

- reverse and forward physical locality are restored
- generic query regressions still remain materially above pre-`edge_id` levels
- the remaining cost is caused by canonical `edge_id` coupling itself rather than surrounding indexing/layout choices

Until then, the burden of proof is on the more complex `edge_id`-centric
storage shape, not on the simpler endpoint-major one.

### Decision rule

For each affected subsystem, choose the simplest design that satisfies:

- multi-edge correctness
- stable mutation semantics
- no hot-path secondary lookup requirement
- benchmark parity with the simpler pre-`edge_id` access pattern

If two designs are performance-equivalent, prefer the one with:

- fewer synchronized structures
- more direct traversal semantics
- less query-shape-specific patching in the executor

## Implementation Checklist Against Current Code

This section tracks how the current implementation compares to the target model.

### Already aligned

- `EdgeEntry` already stores packed `label_and_flags`
- `RevEntry` now also stores packed `label_and_flags`
- forward traversal hot paths can use `edge.label_id()` and `edge.is_tombstoned()` inline
- reverse traversal hot paths can use `rev.label_id()` and `rev.is_tombstoned()` inline
- query-shape-specific edge property indexes exist:
  - `edge_prop_eq_by_src`
  - `edge_prop_eq_by_dst`
- specialized endpoint-grouped aggregate paths already benefit from adjacency-local scans

### Read-path issues mostly fixed

The following regression sources were largely addressed already:

- per-edge tombstone re-lookups in executor hot loops
- reverse traversal depending on repeated canonical tombstone lookups
- edge property equality lookups scanning `edge_id` sets and then filtering by `src/dst`

These fixes should be preserved as invariants, not treated as incidental optimizations.

### Remaining read-path issues

Some code still derives label or tombstone state through endpoint-based re-discovery instead of consuming the already-available physical entry metadata.

Current watchlist:

- endpoint-based helpers such as `edge_label(src, dst)` and `is_edge_tombstoned(src, dst, label)`
- executor paths that still compose `edge_label(...)` + `is_edge_tombstoned(...)` instead of using a carried edge payload
- any path that finds an edge by `(src, dst, label_id)` and then immediately re-checks state already present on `EdgeEntry` or `RevEntry`

These are not all equally hot, but they are the main places where the implementation can still drift back toward `edge_id`-centric or endpoint-relookup behavior.

### Remaining write-path issues

Write-heavy paths still perform linear searches inside reverse-entry vectors when synchronizing updates:

- revive paths update `rev_index` by searching for matching `(src, edge_id)`
- delete paths do the same
- payload update helpers scan reverse entries by `(src, label_id())`
- bulk insert / bulk revive paths patch recently inserted reverse entries by searching the destination bucket

This is acceptable for now, but it means update cost still grows with per-destination reverse degree.

If write-heavy benchmarks remain a problem, the next optional step is:

- add `edge_id -> locator`
- and possibly `edge_id -> reverse bucket position`

Only do that if benchmarks justify the additional maintenance complexity.

### Overlay / canonical-state issues

Canonical edge metadata remains keyed by `edge_id`, which is correct. However, some maintenance and stats paths still iterate `edge_props` and then re-check liveness via endpoint helpers:

- stable snapshot / restore
- property selectivity maintenance
- index rebuild / backfill helpers

These are colder than query execution, so they are lower priority than traversal fixes, but they are still part of the overall `edge_id` integration cost.

### Practical next steps

1. Audit executor call sites that still use `edge_label(...)` + `is_edge_tombstoned(...)`
2. Replace those with carried `EdgeEntry` / `RevEntry` metadata where possible
3. Re-measure graph traversal and aggregate benchmarks
4. Only if write-heavy regressions remain important, prototype `edge_id -> locator`

### Success criteria

We should consider the layering successful when:

- traversal-heavy queries no longer need endpoint rediscovery in hot loops
- reverse traversals never re-read canonical edge state just to test tombstone/label
- shape-specific property lookups avoid `edge_id` full-bucket filtering
- remaining `edge_id` costs are mostly in mutation, upgrade, and cold maintenance paths

## Concrete Rollback Target: `rev_index`

The current `rev_index` is:

```rust
rev_index: HashMap<dst, Vec<RevEntry>>
```

This is the clearest remaining example of the wrong physical shape:

- all incoming labels for a destination are mixed together
- generic reverse traversal often scans the full bucket before rejecting by label
- exact-label reverse hops are only fast when the executor manually pushes a label filter down

That shape is semantically fine, but physically wrong for graph queries.

### Why `rev_index` should be first

The current investigation already shows:

- `thread_depth` continuation work was dominated by reverse label mismatch
- exact-label reverse prefiltering cut scanned reverse edges dramatically
- the remaining issue is not canonical `edge_id` lookup, but reverse iteration shape

So `rev_index` is the highest-value rollback target.

### Preferred redesign

Move from:

```text
dst -> [mixed RevEntry labels]
```

to one of these label-aware shapes:

```text
(dst, label_id) -> [RevEntry]
```

or

```text
dst -> { label_id -> [RevEntry] }
```

The second form is often simpler to update incrementally:

- one destination lookup
- then one label bucket lookup
- then iterate only matching reverse edges

### Keep these fields

The redesigned reverse entry should still keep:

- `src`
- `weight`
- `timestamp`
- `label_and_flags`
- `edge_id`

So this is not a semantic rollback of `RevEntry`, only a rollback of the
bucket organization.

### API consequences

After the redesign, the intended API shape is:

- exact-label reverse traversal:
  `for_each_reverse_neighbor(dst, Some(label_id), ...)`
- all-label reverse traversal:
  `for_each_reverse_neighbor(dst, None, ...)`

The important distinction is that `Some(label_id)` should no longer mean
"scan a mixed vector and filter cheaply"; it should mean "enter the matching
label bucket directly".

### Expected simplifications

This redesign should let us remove or reduce:

- executor-side exact-label reverse scan patches
- repeated generic `resolved_label.matches(...)` checks on obviously exact-label reverse hops
- some reverse-side dedup work that currently happens after scanning irrelevant labels

### Update-path tradeoff

Today, updates are simple because each destination has a single vector.
Moving to label-aware buckets introduces slightly more bookkeeping on:

- insert
- revive
- delete
- label change
- bulk insert / revive

But this is a good trade if it removes generic reverse-query regressions.

### Minimal rollout plan

1. Keep `RevEntry` exactly as-is
2. Change only the bucket shape of `rev_index`
3. Reimplement:
   - `build_reverse_index`
   - `for_each_reverse_neighbor`
   - `reverse_neighbors_rich`
   - reverse insert/revive/delete maintenance
4. Re-measure:
   - `bench_social_thread_depth`
   - `bench_social_feed`
   - `bench_social_fof_recommend`
   - `bench_social_hashtag_cooccurrence`
5. Only after measurement, decide whether forward adjacency needs the same kind of rollback

### Decision rule for `rev_index`

If a label-aware `rev_index` restores generic reverse-query performance close to
pre-`edge_id` levels, keep:

- `edge_id` semantics
- current `RevEntry`
- current overlay/log design

and stop there.

If not, then the next rollback step should be the broader physical key shape,
not immediate removal of `edge_id` semantics.

## `rev_index` Redesign Proposal

This is the concrete proposal that best matches the combined VCSR + DGAP
guidance.

### Target shape

Replace:

```rust
rev_index: HashMap<u32, Vec<RevEntry>>
```

with:

```rust
rev_index: HashMap<u32, HashMap<u32, Vec<RevEntry>>>
```

meaning:

- outer key: `dst`
- inner key: `label_id`
- value: reverse neighbors for exactly that `(dst, label_id)` bucket

This keeps reverse access vertex-centric while restoring label-locality.

### Why this shape first

It is the smallest structural rollback that:

- fixes the confirmed mixed-label scan problem
- preserves `RevEntry` unchanged
- preserves `edge_id` unchanged
- keeps update semantics simple enough to implement incrementally

It is also closer to how a VCSR/DGAP-style reader wants to consume data:

- find vertex bucket
- optionally narrow by label
- iterate matching neighbors

### Access semantics

The intended behavior becomes:

- `for_each_reverse_neighbor(dst, Some(label_id), ...)`
  - go directly to the `(dst, label_id)` bucket
- `for_each_reverse_neighbor(dst, None, ...)`
  - iterate all label buckets for `dst`
- `reverse_neighbors_rich(dst)`
  - concatenate or iterate all buckets for `dst`, then apply existing dedup rules

This means exact-label reverse traversal becomes structurally fast rather than
executor-accidentally-fast.

### Update semantics

The redesign should preserve the current semantic model:

- insert:
  - append to `rev_index[dst][label_id]`
- delete / revive:
  - find by `(src, edge_id)` inside the specific label bucket if label known
- relabel:
  - remove from old label bucket and insert into new label bucket
- payload update:
  - update weight/timestamp in the specific label bucket

This remains compatible with DGAP's principle that update-side auxiliary
structures may be more complex, as long as read-side iteration stays simple.

### Expected hot-path gains

This should reduce:

- wrong-label reverse scans
- reverse-side label reject counts
- executor-side need to special-case exact-label reverse traversal
- reverse continuation cost in generic var-len queries

The main expected winners are:

- `bench_social_thread_depth`
- `bench_social_feed`
- `bench_social_fof_recommend`
- `bench_social_hashtag_cooccurrence`

### Known tradeoffs

Costs:

- one extra hash lookup on reverse updates
- relabel needs bucket migration instead of in-place label change only
- `reverse_neighbors_rich(None)` must iterate nested buckets

Benefits:

- read-path work scales with matching labels instead of all incoming labels
- generic reverse queries become simpler to reason about
- less executor-local patching is needed

This is the right trade if reverse-heavy generic queries are still materially
slower than pre-`edge_id`.

### Fallback option if nested maps are too costly

If nested maps regress write-heavy paths too much, the next option is:

```rust
rev_index: HashMap<(u32, u32), Vec<RevEntry>>
```

keyed by `(dst, label_id)`.

This is less ergonomic for "all incoming labels" scans, but even more explicit
about the physical access key. The nested-map version should be tried first
because it preserves the natural vertex-first access pattern.

### Rollout plan

Phase 1:

- change the in-memory bucket shape only
- keep public reverse APIs stable

Phase 2:

- update build/restore/insert/revive/delete/relabel paths
- re-run reverse-heavy social benches

Phase 3:

- if successful, simplify executor call sites that manually compensate for
  mixed-label reverse scans

Phase 4:

- only then decide whether forward-side physical rollback is also necessary

### Follow-up after reverse rollback

That decision is now made: forward-side physical rollback is necessary.

Observed after the reverse-bucket redesign:

- reverse-heavy generic queries recovered materially without adding new query
  families
- but `thread_depth` still overran the IC query limit
- split counters in the generic matcher showed the remaining continuation cost
  was dominated by outgoing candidates and outgoing label rejects, not reverse
  ones

Implication:

- the reverse side is no longer the dominant structural bottleneck
- the remaining low-level problem is outgoing adjacency that still lacks strong
  label locality for exact-label traversal

So the next physical experiment should mirror the reverse rollback on the
forward side:

- keep `edge_id` as semantic identity / overlay-log key
- restore a more label-aware outgoing access path
- prefer the simplest design that preserves vertex-first traversal

Candidate forms:

- `src -> { label_id -> Vec<EdgeEntry> }`
- `(src, label_id) -> Vec<EdgeEntry>`
- endpoint-major adjacency plus compact per-label offsets

Decision rule:

- if one of these restores generic outgoing exact-label traversal close to
  pre-`edge_id` behavior, prefer it over more executor fast paths
- if two designs perform similarly, choose the simpler one
