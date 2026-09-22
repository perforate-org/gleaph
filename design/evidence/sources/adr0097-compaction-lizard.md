# Lizard complexity review

Mode: working tree; baseline `d4c59ff8b`; Lizard 1.24.0. Six changed Rust files only.
Options: `lizard --csv -i -1`. No project settings found under `scripts/` or `.github/`.
Snapshots, raw CSVs and zero-context hunks are retained in the temporary evidence directory.

## Summary

- Directly touched functions: 15 → 20.
- Maximum touched CCN: 42 → 31.
- Aggregate touched CCN: 245 → 212.
- Assessment: focused deletion of the observer-free logical packing alternative, not a performance claim.

## Material changes

| Function | CCN | NLOC | Tokens | Parameters |
| --- | ---: | ---: | ---: | ---: |
| `deferred.rs::edge_slot_moved` | — → 1 | — → 22 | — → 115 | — → 4 |
| `deferred.rs::retained_compaction_observer_keeps_external_identities_through_retry_and_finish` | — → 9 | — → 126 | — → 793 | — → 0 |
| `bucket.rs::rebalance_cascade_after_labeled_mutation` | 31 → 31 | 95 → 95 | 593 → 579 | 2 → 2 |
| `compact.rs::commit_vertex_edge_span_layout` | 42 → 25 | 188 → 117 | 1147 → 681 | 15 → 12 |
| `compact.rs::compact_vertex_edge_span_one_step_with_span_policy` | 33 → 27 | 99 → 90 | 643 → 584 | 6 → 6 |
| `compact.rs::labeled_rewrite_within_pinned_leaf_does_not_release_vertex_footprint` | 2 → 2 | 46 → 46 | 255 → 253 | 0 → 0 |
| `compact.rs::maintain_vertex_edge_span_light` | 10 → 10 | 27 → 27 | 187 → 185 | 3 → 3 |
| `compact.rs::materialize_labeled_vertex_edge_plan` | 27 → 3 | 86 → 18 | 611 → 105 | 4 → 2 |
| `compact.rs::pack_live_edge_slots` | 4 → — | 11 → — | 84 → — | 1 → — |
| `compact.rs::pack_vertex_edge_span_layout` | — → 8 | — → 26 | — → 187 | — → 2 |
| `compact.rs::rebalance_labeled_leaf_weighted_slide_in_block` | 27 → 27 | 151 → 131 | 1002 → 943 | 7 → 7 |
| `compact.rs::rebalance_vertex_edge_span` | 4 → 4 | 25 → 24 | 112 → 110 | 5 → 5 |
| `compact.rs::retained_compaction_finished_matches_every_reported_move` | — → 9 | — → 133 | — → 733 | — → 0 |
| `compact.rs::rewrite_vertex_edge_span` | 2 → 2 | 22 → 20 | 77 → 71 | 7 → 6 |
| `compact.rs::rewrite_vertex_edge_span_read_and_plan` | 30 → 25 | 77 → 70 | 509 → 480 | 8 → 7 |
| `compact.rs::rewrite_vertex_edge_span_with_policy` | 17 → 17 | 79 → 71 | 407 → 367 | 8 → 7 |
| `compact.rs::vertex_edge_span_slot_moves` | 11 → — | 41 → — | 210 → — | 2 → — |
| `test_support.rs::retained_compaction_fixture` | — → 5 | — → 133 | — → 823 | — → 1 |
| `test_support.rs::retained_compaction_rows` | — → 1 | — → 22 | — → 134 | — → 5 |
| `test_support.rs::retained_compaction_value` | — → 1 | — → 5 | — → 62 | — → 2 |
| `values.rs::edge_inline_propertys_survive_rewrite_with_tombstones` | 3 → 3 | 52 → 52 | 353 → 351 | 0 → 0 |
| `values.rs::labeled_inline_property_bytes_edge_order_matches_edge_slab_order` | 2 → 2 | 30 → 30 | 195 → 193 | 0 → 0 |


File totals (all parsed functions, to detect complexity displacement):

| File | CCN | NLOC | Maximum CCN |
| --- | ---: | ---: | ---: |
| `crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs` | 730 → 740 | 6516 → 6664 | 36 → 36 |
| `crates/ic-stable-lara/src/labeled/graph.rs` | 24 → 24 | 161 → 161 | 3 → 3 |
| `crates/ic-stable-lara/src/labeled/graph/bucket.rs` | 242 → 242 | 1286 → 1286 | 54 → 54 |
| `crates/ic-stable-lara/src/labeled/graph/compact.rs` | 671 → 621 | 4387 → 4308 | 42 → 27 |
| `crates/ic-stable-lara/src/labeled/graph/test_support.rs` | 57 → 64 | 546 → 706 | 3 → 5 |
| `crates/ic-stable-lara/src/labeled/graph/values.rs` | 434 → 434 | 3814 → 3814 | 33 → 33 |


## Decisions

Keep: one physical publisher for completion, peer cascade and both leaf-relocation arms.
Remove: the compact/fold switches, typed whole-prefix encoding buffer, silent run retirement,
`pack_live_edge_slots` and unused `vertex_edge_span_slot_moves`. No cache, queue or dependency added.
The new shared packing entry shrinks physical slack inside existing backing; it cannot move logical ordinals.
Tests/fixture growth protects current-head reachability, independent values, external identities, retry and completion.

## Validation

`retained_compaction`: 2 passed; `compact`: 81 passed; `relocat`: 11 passed.
Strict library clippy and scoped formatting pass. No benchmark speedup inferred from these metrics.

## Limits

This is implementation self-review, not independent acceptance or general atomicity approval.
Lizard can count generic commas as parameters (e.g. the four-argument rows helper reports five)
and nested test functions contribute parser-level aggregate counts. Removed functions remain unmatched;
the new packing entry is not forced into a match with the deleted unused move collector.
Other high-complexity functions and broader branch complexity remain outside this slice.
