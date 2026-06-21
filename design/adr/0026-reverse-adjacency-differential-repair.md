# 0026. Reverse-adjacency differential repair

Date: 2026-06-21
Status: implemented
Last revised: 2026-06-21
Anchor timestamp: 2026-06-21 00:11:30 UTC +0000

## Context

The canonical adjacency source of truth is the **forward** directed edges in `GRAPH`.
The reverse orientation (LARA `REV_*` regions 15, 16, 19, 20, 21, 25, 28, 29) is a
derived mirror, co-updated inside the same commit on every edge insert/delete. The
membership invariant is `reverse == projection of forward over local directed edges`.

`check_reverse_adjacency` (`facade/derived_state/reverse_adjacency.rs`) already detects
divergence as per-`DirectedEdgeKey` count gaps (`missing_in_reverse`, `extra_in_reverse`),
but ADR 0025's commit left the repair side deferred ("future ADR"). The sibling derived
store `EDGE_ALIASES` has had a check **and** rebuild (`rebuild_edge_aliases`) since
ADR 0009; reverse adjacency was the only `Derived` region with a check but no rebuild
entry point, so its stable-layout classification was `SyncCoUpdate` rather than `Named`.

A naive `rebuild_reverse_adjacency` that clears all `REV_*` regions and replays forward
edges is unsound in this layout:

- `EDGE_ALIASES` directed keys are `edge_alias_slot_key(reverse_slot, true)` — derived
  from reverse **slot indices** (`facade/store/helpers.rs`). Rebuilding the whole reverse
  orientation reassigns every reverse slot, cascade-invalidating every directed alias row.
- The reverse payload slab/log/blobs (regions 25, 28, 29) would be rewritten wholesale.

## Decision

Provide a **differential** `rebuild_reverse_adjacency(store)` that reconciles **only the
diverged keys** reported by `check_reverse_adjacency`, against the forward source of truth.

For each diverged `(src, tgt, label)` key:

1. Remove all of the key's reverse in-edge halves at `tgt` (predicate match on neighbor
   `src`) via `remove_reverse_edge_matching`, and drop the key's directed alias rows via
   `EDGE_ALIASES::remove_all_for_canonical` over the key's forward slots.
2. Re-insert one reverse half per forward out-edge of the key — copying the forward edge's
   payload bytes from the slab (`for_each_directed_out_edges_for_label_with_payload_slices_reusing`)
   and re-creating the directed reverse-IN alias with the exact
   `find_reverse_alias_for_canonical` + alias-insert sequence the live insert path uses in
   `commit_directed_edge_insert`.

This makes `reverse[key] == forward[key]` exactly, handles multigraph count gaps and edge
payloads, and reassigns slots **only for diverged keys** (recreating their aliases in the
same step). Non-diverged keys keep their reverse slots and alias rows; edge properties
(`EDGE_PROPERTIES`, keyed by canonical forward identity) are unaffected by reverse-slot
changes. The expensive part — the full forward+reverse scan — already lives in the oracle;
the repair set is normally empty.

The repair is `pub(crate)` defense-in-depth with **no canister/admin endpoint**: IC
co-updates trap-and-roll-back atomically, so divergence is near-unreachable in practice.
The `REV_*` derived regions are reclassified from `RebuildPath::SyncCoUpdate` to
`RebuildPath::Named("rebuild_reverse_adjacency")`, matching the `EDGE_ALIASES` precedent
(co-updated live **and** `Named`).

### Multigraph alias precision

For parallel directed edges sharing one `(src, tgt, label)` key, `find_reverse_alias_for_canonical`
returns the first matching reverse half, so alias precision after repair matches the live
insert path's existing first-match behavior — not more, not less. The membership invariant
checked by `check_reverse_adjacency` is always restored regardless.

### Alternatives considered

- **Full clear-and-rebuild** (like `rebuild_edge_aliases` clears `EDGE_ALIASES`).
  Rejected: clearing/replaying all `REV_*` regions reassigns every reverse slot index,
  cascade-invalidating all `EDGE_ALIASES` directed keys and rewriting the reverse payload
  slab wholesale — a larger, multi-store operation with no benefit over the differential
  repair, whose cost is already bounded by the (normally empty) divergence set.
- **No repair (check-only).** The status quo; leaves the only `Derived` region without a
  rebuild path and offers no recovery if divergence ever occurs.
- **Admin endpoint.** Unnecessary: there is no reachable production path that produces
  divergence (atomic co-update), so the repair stays internal defense-in-depth.

## Consequences

- Reverse adjacency now has a check **and** a rebuild, closing the ADR 0025 follow-up and
  making `Named("rebuild_reverse_adjacency")` the honest stable-layout classification for
  the 8 derived `REV_*` regions.
- The repair touches only diverged keys, so it preserves unrelated reverse slots, their
  aliases, and all edge properties; no cascade invalidation.
- No new stable region, timer, or endpoint is added; the GC/repair surface stays internal.

## Tests

`crates/graph/src/facade/derived_state/reverse_adjacency.rs`:
`repairs_forward_only_edge_into_reverse`,
`repairs_reverse_orphan_by_removal`,
`rebuild_preserves_edge_payload`,
`rebuild_is_noop_when_consistent`,
`rebuild_leaves_unrelated_reverse_slots_untouched`.

`crates/graph-kernel/src/stable_layout.rs`:
`derived_regions_declare_a_rebuild_path` (asserts `REV_VERTICES` is
`Named("rebuild_reverse_adjacency")`).
