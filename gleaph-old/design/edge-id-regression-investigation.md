# Edge ID Regression Investigation

This note summarizes the most plausible benchmark regressions introduced by the
`edge_id` unification work and the next verification steps.

Primary commits:

- `de342b0` `Inline edge labels and tombstones into EdgeEntry`
- `f21dd2f` `Remove EdgeMetaTable and add top-k count fast path`

## Working hypothesis

The likely fix is not a full semantic revert of `edge_id`, but a **partial
rollback of the physical/query-time consequences** of the integration.

Current working rule:

- keep `edge_id` where it gives semantic clarity:
  - logical identity
  - mutation targeting
  - upgrade / log / overlay state
- roll back or redesign the parts that made generic reads slower:
  - mixed-label reverse scans
  - endpoint-late identification
  - query-time recovery of `src/dst/label`
  - endpoint-locality loss in projected indexes

This means the investigation is no longer asking “should `edge_id` exist at
all?”, but rather:

- which remaining regressions are caused by `edge_id` as a semantic identity
- and which are caused by using `edge_id` too deeply in physical read paths

Low-level design rule for this investigation:

- use the VCSR-style adjacency walk as the baseline mental model
- use DGAP's log-structured update path as the baseline mutation mental model
- treat any path that reconstructs adjacency from `edge_id`-centric state as a
  potential regression source

So benchmark regressions should be interpreted first as violations of:

- vertex-centric access
- endpoint-major physical locality
- direct neighborhood iteration
- update buffering as an auxiliary structure rather than the read-path center

and only secondarily as missing query-specific executor optimizations.

## Candidate regressions

### 1. Edge property lookups lost physical locality

`de342b0` changed `edge_props` from `(src, dst, label) -> PropertyMap` to
`edge_id -> EdgePropsOverlay`, and changed the equality index from
`BTreeSet<(src, dst)>` to `BTreeSet<u32>`.

Current structures:

- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L85)
- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L433)
- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L435)

Why this can regress:

- query-time lookup now often means `edge_id -> overlay -> src/dst/label`
- old index shape could answer `targets_for_src` / `sources_for_dst` by range
- generic edge-property paths lost the clustered `(src,dst,label)` access pattern

This already explained the earlier edge-index regressions, and may still affect
generic aggregate paths that need edge payload/property access.

### 2. Reverse traversal started doing per-edge tombstone relookups

`de342b0` changed reverse traversal from checking a tombstone set to calling
`is_edge_tombstoned(entry.src, target, label)` inside reverse loops.

Current reverse APIs:

- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L4160)
- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L4240)

`is_edge_tombstoned()` now resolves a locator and reads the PMA slot again.
That is much more expensive than the pre-`edge_id` `tombstoned_edges` set check.

`f21dd2f` partially fixed this by carrying tombstone state in `RevEntry`, but
the commit history strongly suggests reverse paths were one major source of the
initial regression.

### 3. `edge_label()` and `edge_record()` became much more expensive

Current implementations:

- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L4141)
- [pma.rs](/Users/yota/dev/gleaph/crates/pma/src/pma.rs#L4301)

Why this matters:

- `edge_label()` now derives the label from PMA payload lookup for the pair
- `edge_record()` now combines:
  - tombstone lookup by locator
  - overlay lookup by `edge_id`
  - `collect_neighbors(src)` followed by `find(dst)`

Pre-`edge_id`, `edge_record()` could use `(src,dst,label)` keyed overlay and
the tombstone set directly. Any generic executor path that still calls
`edge_label()` / `edge_record()` in hot loops is a likely remaining regression.

### 4. Per-edge storage got larger and more duplicated

Relevant current types:

- [lib.rs](/Users/yota/dev/gleaph/crates/types/src/lib.rs#L191)
- [lib.rs](/Users/yota/dev/gleaph/crates/types/src/lib.rs#L253)

Effects:

- `EdgeEntry` now always carries `edge_id`
- `LogEntry` also carries `edge_id`
- overlay keeps `src`, `dst`, `label`, and props alongside PMA storage

This explains write-heavy regressions (`rebalance`, `resize`, `upgrade`) and
can also hurt cache density for generic traversals.

### 5. Generic reverse/var-len aggregate paths still pay the old abstraction cost

`thread_depth` is currently still on the generic aggregate path. The latest
profile showed:

- `execute_plan_with_limits_and_hasher`: ~`83.5M`
- `scanned_e`: `69249`
- `rows_after_match`: `5000`
- `groups`: `5000`

This suggests the remaining cost is not only traversal count, but also generic
row/group materialization over many intermediate rows. The attempted dedicated
fast path regressed, which implies the root issue is deeper than simple shape
recognition.

## Verification plan

### A. Cross-commit measurement

Compare these commits on the same targeted benchmarks:

1. `c504a36` pre-`edge_id`
2. `de342b0` first `edge_id` integration
3. `f21dd2f` after reverse tombstone and index fixes
4. current working tree

Target benches:

- `bench_social_thread_depth`
- `bench_social_content_virality`
- `bench_social_feed`
- `bench_edge_index_sources_for_dst_5000`
- `bench_edge_index_targets_for_src_500`
- `bench_upgrade_roundtrip`

Expected outcome:

- if `de342b0` is the sharp drop and `f21dd2f` only partially recovers it,
  the remaining root cause is structural and not just a missing fast path

Prepared worktrees:

- `/tmp/gleaph-c504a36`
- `/tmp/gleaph-de342b0`
- `/tmp/gleaph-f21dd2f`

Concrete commands for each worktree:

```bash
cd /tmp/gleaph-<commit>
cargo build --release -p gleaph-graph --target wasm32-unknown-unknown --features bench-social
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  cargo test -p gleaph-tests -- --ignored gen_social_stable_snapshot --test-threads=1 --nocapture
cd bench/social
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  canbench bench_social_thread_depth
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  canbench bench_social_feed
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  canbench bench_social_content_virality
```

If `thread_depth` is too slow on older commits, fall back to:

```bash
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  canbench bench_social_follower_activity
POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic \
  canbench bench_social_hashtag_cooccurrence
```

### Current cross-commit results

`feed`:

- `c504a36`: total `1.04M`, execute `443.38K`, parse `274.04K`, plan `115.93K`
- `de342b0`: total `1.11M`, execute `513.01K`, parse `275.16K`, plan `115.09K`
- `f21dd2f`: total `1.04M`, execute `445.32K`, parse `275.49K`, plan `114.87K`

`content_virality`:

- `c504a36`: total `788.82K`, execute `446.37K`, parse `152.58K`, plan `71.97K`
- `de342b0`: total `1.00M`, execute `657.12K`, parse `154.84K`, plan `71.84K`
- `f21dd2f`: total `807.67K`, execute `460.99K`, parse `154.72K`, plan `71.50K`

`thread_depth`:

- `c504a36`: profiled probe succeeds
  - `parse_statement`: `247,767`
  - `validate_statement`: `14,734`
  - `build_plan_with_stats`: `91,641`
  - `execute_plan_with_limits_and_hasher`: `71,306,323`
  - `scanned_e`: `44,766`
  - `rows_after_match`: `1,413`
  - `groups`: `795`
- `de342b0`: profiled probe exceeds the `40B` single-message instruction limit
- `f21dd2f`: profiled probe exceeds the `40B` single-message instruction limit
- current main, scaled shape (`WITH u LIMIT 50`):
  - `execute_plan_with_limits_and_hasher`: `38,408,871`
  - `scanned_e`: `8,354`
  - `rows_after_match`: `2,500`
  - `compiled_fast`: `true`
  - `edge_label_calls`: `0`
  - `edge_record_calls`: `0`
  - `is_edge_tombstoned_calls`: `0`
  - `reverse_callbacks`: `0`
  - `var_len_dfs_calls`: `71`
  - `compiled_match_records`: `0`
  - `var_len_binding_clones`: `0`
  - `var_len_path_contains_checks`: `0`
  - `var_len_node_match_checks`: `0`
  - `reverse_row_clones`: `0`
  - `reverse_node_match_checks`: `0`
  - `compiled_group_key_evals`: `0`
  - `compiled_group_bucket_probes`: `0`
  - `compiled_agg_updates`: `0`
  - `compiled_projection_fast_calls`: `1`
  - `compiled_projection_input_rows`: `0`
  - `compiled_projection_empty_returns`: `1`
  - `with_continuation_match_calls`: `1`
  - `with_continuation_match_input_rows`: `50`
  - `with_continuation_match_output_rows`: `0`
  - `joined_match_start_candidates`: `2,550`
  - `joined_match_local_rows_before_inline_where`: `2,500`
  - `joined_match_local_rows_after_inline_where`: `2,500`
  - `with_continuation_joined_match_start_candidates`: `50`
  - `with_continuation_joined_local_rows_before_inline_where`: `0`
  - `with_continuation_joined_local_rows_after_inline_where`: `0`
  - `with_continuation_scanned_edges`: `8,354`
  - `with_continuation_execution_steps`: `8,354`
  - `with_continuation_hop_label_rejects`: `8,183`
  - `with_continuation_hop_node_rejects`: `0`
  - `with_continuation_hop_edge_property_rejects`: `0`
  - `with_continuation_hop_where_pushdown_rejects`: `0`
  - `with_continuation_var_len_cycle_rejects`: `0`

Interpretation:

- `de342b0` is the sharp execute-path regression point for both simple social
  queries and reverse/var-len traversal.
- `f21dd2f` largely recovers `feed` / `content_virality`, but does **not**
  recover `thread_depth`.
- That means the reverse/var-len family still carries structural `edge_id`
  lookup costs even after tombstone carry and `by_src/by_dst` recovery.
- On current main, the hot path no longer goes through `edge_record()`,
  `edge_label()`, or `is_edge_tombstoned()` for the scaled `thread_depth`
  shape.
- The scaled probe also does **not** hit the currently instrumented generic
  reverse/var-len counters or the compiled aggregate grouping counters. That
  means the remaining cost is even deeper than the executor-layer helpers we
  first suspected.
- `compiled_fast=true` is currently a red herring for this shape: it comes from
  a row-based compiled projection that sees `0` input rows and immediately
  returns the empty-row aggregate result. It is not where the `~38M`
  instructions are spent.
- The expensive part of the scaled shape is the WITH continuation itself:
  after `WITH u LIMIT 50`, the follow-on MATCH is invoked once with `50` seed
  rows and still produces `0` output rows while spending `~38M` instructions.
- Call-site-scoped deltas confirm the broader joined-MATCH counters were indeed
  aggregating other uses. The WITH continuation itself contributes only:
  `start_candidates=50`, `local_rows_before_inline_where=0`,
  `local_rows_after_inline_where=0`.
- That means the expensive part of scaled `thread_depth` is not “rows are
  generated and later filtered out”. It is the cost of exploring 50 seeded
  reverse/var-len searches that each fail without producing any local rows.
- The rejection breakdown is now clear: almost all continuation work is lost on
  edge-label mismatch (`8,183` label rejects out of `8,354` scanned edges),
  while node/property/WHERE/cycle rejects are effectively zero.
- The next likely candidate is therefore generic reverse/var-len traversal over
  mixed-label neighborhoods after `edge_id` unification. The executor is
  scanning many irrelevant reverse neighbors before it can find the relevant
  labels, or conclude there are none.
- After changing exact-label reverse hops to use
  `for_each_reverse_neighbor(..., Some(label_id), ...)` instead of scanning
  `reverse_neighbors_rich()` and filtering later, the same scaled current-main
  probe drops to:
  - `execute_plan_with_limits_and_hasher`: `32,829,953`
  - `scanned_e`: `941`
  - `with_continuation_scanned_edges`: `941`
  - `with_continuation_execution_steps`: `941`
  - `with_continuation_hop_label_rejects`: `770`
- This confirms that the remaining reverse/var-len cost was not primarily
  overlay relookup, but the lack of reverse label prefiltering in the generic
  path.
- The broader `bench-social` run also improves in the same direction after this
  change:
  - `bench_social_feed`: total `694.47K`, execute `271.48K`
  - `bench_social_content_virality`: total `532.84K`, execute `282.94K`
  - `bench_social_fof_recommend`: total `940.78K`, execute `588.24K`
  - `bench_social_follower_activity`: total `446.40K`, execute `153.75K`
  - `bench_social_hashtag_cooccurrence`: total `14.20M`, execute `13.85M`
- Those improvements point to a more general rule: reverse traversal should be
  label-aware at iteration time, not after enumerating mixed-label incoming
  neighborhoods.

## Next rollback candidate: reverse bucket organization

The current evidence points to `rev_index` as the best partial-rollback target.

Current shape:

- `rev_index: HashMap<dst, Vec<RevEntry>>`

Likely problem:

- all incoming labels for a destination share one physical bucket
- exact-label reverse traversal is only fast if the executor manually pushes a label filter down
- generic reverse code is still vulnerable to “scan mixed bucket, reject later”

Most likely high-value redesign:

- move to label-aware reverse buckets
  - either `(dst, label_id) -> Vec<RevEntry>`
  - or `dst -> { label_id -> Vec<RevEntry> }`

Why this is the right next experiment:

- it is a physical rollback, not a semantic rollback
- it keeps `edge_id`, `RevEntry`, and overlay semantics intact
- it directly targets the confirmed `thread_depth` failure mode
- it should reduce the need for executor-local reverse-label fast paths

Success criterion:

- reverse-heavy generic benchmarks move closer to pre-`edge_id` behavior
- especially `bench_social_thread_depth`
- without reintroducing ambiguity for multi-edge updates

## First implementation experiment

The first concrete rollback experiment should be:

- change `rev_index` from
  - `HashMap<dst, Vec<RevEntry>>`
- to
  - `HashMap<dst, HashMap<label_id, Vec<RevEntry>>>`

Reason:

- this is the smallest structural change that directly attacks the confirmed
  wrong-label reverse-scan regression
- it keeps `edge_id` semantics intact
- it keeps `RevEntry` intact
- it matches the VCSR/DGAP principle that traversal should start from a vertex
  neighborhood, with update machinery remaining auxiliary

Measurement plan for that experiment:

1. `cargo test -p gleaph-pma --lib`
2. `cargo test -p gleaph-gql --lib`
3. `POCKET_IC_BIN=/Users/yota/dev/_packages/pocket-ic/v12/pocket-ic make bench-social`
4. focus on:
   - `bench_social_thread_depth`
   - `bench_social_feed`
   - `bench_social_fof_recommend`
   - `bench_social_hashtag_cooccurrence`
   - `bench_bulk_insert_10k`

Decision rule:

- if reverse-heavy reads recover materially and write regressions stay modest,
  keep the redesign
- if write-path cost grows too much, try `(dst, label_id)` flat-key buckets
  before considering broader rollback

### Result of the first `rev_index` rollback experiment

Implemented experiment:

- changed `rev_index` from
  - `HashMap<dst, Vec<RevEntry>>`
- to
  - `HashMap<dst, HashMap<label_id, Vec<RevEntry>>>`

Observed `bench-social` improvements after this change:

- `bench_social_content_virality`
  - total: `463.21K`
  - execute: `237.39K`
- `bench_social_feed`
  - total: `616.88K`
  - execute: `230.08K`
- `bench_social_fof_recommend`
  - total: `797.96K`
  - execute: `474.77K`
- `bench_social_follower_activity`
  - total: `411.90K`
  - execute: `143.76K`
- `bench_social_hashtag_cooccurrence`
  - total: `11.28M`
  - execute: `10.96M`

Interpretation:

- the `rev_index` bucket shape was indeed a structural bottleneck
- reverse-heavy generic queries improved without adding new query-specific fast paths
- this is strong evidence that the remaining regression is primarily physical
  organization, not lack of executor specialization

Working conclusion:

- partial rollback of reverse physical organization is the right direction
- `edge_id` semantics can remain intact while read-path shape moves back toward
  VCSR/DGAP principles
- the remaining `thread_depth` cost is no longer reverse-dominant after this
  rollback

### Follow-up probe after reverse-bucket rollback

After adding forward/reverse split counters to the generic matcher and rerunning
the scaled `thread_depth` probe on current main:

- `execute_plan_with_limits_and_hasher`: `33,521,708`
- `scanned_e`: `941`
- `with_continuation_scanned_edges`: `941`
- `with_continuation_execution_steps`: `941`
- `outgoing_hop_candidates`: `870`
- `incoming_hop_candidates`: `71`
- `hop_label_rejects`: `770`
- `outgoing_hop_label_rejects`: `770`
- `incoming_hop_label_rejects`: `0`
- `with_continuation_outgoing_hop_candidates`: `870`
- `with_continuation_incoming_hop_candidates`: `71`
- `with_continuation_outgoing_hop_label_rejects`: `770`
- `with_continuation_incoming_hop_label_rejects`: `0`

This materially changes the diagnosis:

- the reverse-side structural regression has mostly been removed by the
  `rev_index` rollback
- the remaining `thread_depth` waste is now almost entirely in outgoing
  traversal
- specifically, the continuation `MATCH` still enumerates many outgoing edges
  only to reject them by label later

Updated next rollback candidate:

- forward adjacency / neighbor iteration should become more label-aware, in the
  same way reverse adjacency was made label-aware
- if the current PMA layout cannot cheaply support exact-label outgoing
  traversal, the next physical rollback target should be a forward label-aware
  access path rather than additional query-specific fast paths

### Full social-suite confirmation after forward exact-label prefiltering

After adding `for_each_neighbor_filtered(..., label_filter, ...)` in PMA and
pushing exact-label outgoing traversal down from the generic executor:

- `bench_social_content_virality`
  - total: `463.63K`
  - execute: `237.83K`
- `bench_social_feed`
  - total: `618.86K`
  - execute: `231.45K`
- `bench_social_fof_recommend`
  - total: `799.45K`
  - execute: `476.74K`
- `bench_social_follower_activity`
  - total: `413.67K`
  - execute: `145.22K`
- `bench_social_hashtag_cooccurrence`
  - total: `11.25M`
  - execute: `10.93M`
- `bench_social_influencer`
  - total: `32.15M`
  - execute: `31.88M`

But `bench_social_thread_depth` still traps at:

- `8,370,124,301,988` instructions

This means:

- generic outgoing exact-label pushdown is directionally correct, but not
  sufficient for the remaining reverse+var-len family
- the unresolved bottleneck is now the physical cost of outgoing adjacency
  enumeration itself, not merely executor-side late label filtering
- the next rollback experiment should target forward physical organization, not
  additional query-shaped fast paths

### B. Instrument generic hot APIs

Add temporary counters around:

- `edge_label()`
- `edge_record()`
- `is_edge_tombstoned()`
- `collect_neighbors()` when called from generic aggregate paths

Goal:

- prove whether `thread_depth` and similar queries still hit pair-based
  relookups often enough to matter

Status:

- completed for current main scaled `thread_depth`
- result: the suspected pair-relookup APIs are not hot anymore

### C. Instrument compiled aggregate internals

Add temporary counters around:

- compiled aggregate matcher state transitions
- compiled row emission / carry transitions
- lower-level PMA neighbor iteration inside the compiled aggregate path

Goal:

- identify which compiled-aggregate internal loop still accounts for the
  `~38M` instructions in the scaled `thread_depth` shape

### D. A/B one structural rollback at a time

Most promising structural experiments:

1. restore a direct `(src,dst,label)` overlay lookup path for read-only queries
2. restore a direct pair-keyed equality index for read-paths only
3. add a locator cache `edge_id -> edge locator` for cold metadata lookup
4. reintroduce a read-optimized physical key `(src,dst,label,edge_id)` while
   keeping `edge_id` as logical identity

The safest order is `3 -> 1 -> 4`.

### E. Thread-depth specific validation

Use the profiled social probe after each experiment and capture:

- `execute_plan_with_limits_and_hasher`
- `scanned_e`
- `rows_after_match`
- `groups`
- `aggregate_fast_path_used`

Goal:

- separate “fewer traversed edges” from “same traversal count but lower generic
  aggregation cost”

## Working hypothesis

The most likely lasting regression from `edge_id` unification is not the inline
`EdgeEntry` packing itself. It is the shift from physically query-shaped keys to
`edge_id`-centric lookup, which forced generic read paths to reconstruct
`src/dst/label/tombstone` state through extra lookups.

The strongest candidates are:

1. pair lookup degradation in `edge_record()`
2. reverse tombstone / label relookup patterns introduced in `de342b0`
3. loss of pair-keyed edge-property locality

For `thread_depth`, the historical evidence still points most strongly at `2`
and `1`, but the current-main residual cost no longer manifests through the
executor-layer lookup helpers:

- pre-`edge_id` reverse + var-len execution fits in `~71M` instructions
- post-`edge_id` reverse + var-len execution blows past `40B`
- the family still does not recover after later fixes unless the query is
  moved onto a dedicated fast path
- but current main's scaled probe shows that the residual cost is no longer
  explained by direct calls to `edge_record()`, `edge_label()`,
  `is_edge_tombstoned()`, generic reverse-row cloning, generic var-len
  traversal counters, or the currently instrumented compiled-grouping updates

That is consistent with the history:

- edge-index benches regressed sharply and recovered once `by_src/by_dst` were added
- reverse/traversal benches recovered once tombstone state was carried inline
- `thread_depth` still looks like a generic path that has not yet regained the
  old physical locality, but the remaining hot loop likely sits deeper inside
  the compiled aggregate matcher than the counters added so far
