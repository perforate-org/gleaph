# graph-pma Rewrite Status

_Last updated: 2026-03-29_

## Purpose

This document records the current implementation state of the `graph-pma`
rewrite, with emphasis on the property subsystem and the rewrite-side
observability surfaces.

Use this document for:

- understanding what is already implemented
- identifying what is still transitional
- choosing the next implementation steps without re-deriving context

Design intent still lives in:

- [`graph-pma-low-level-spec.md`](/Users/yota/dev/gleaph-project/docs/graph-pma-low-level-spec.md)
- [`graph-pma-property-store-spec.md`](/Users/yota/dev/gleaph-project/docs/graph-pma-property-store-spec.md)

## High-level Summary

The rewrite is now in a materially different place than the earlier
overlay-heavy stage.

- adjacency rewrite:
  - already substantial
  - stable-memory-first
  - local rebalance and locator sidecar paths exist
- property store:
  - stable-memory-backed append-log exists
  - facade/integration read-write paths already treat it as source of truth
- property index:
  - paged node-store exists
  - hydrate/write/read paths are now node-store-primary
  - logical snapshot is increasingly a compatibility and reconstruction layer
- observability:
  - facade and overlay both expose shared event projections
  - formatted diagnostics and debug reports exist
  - executor tests already use these surfaces

The center of gravity has moved from:

- logical snapshot / in-memory overlay first

to:

- paged node-store / stable-memory-first, with logical views rebuilt as needed

## What Is Implemented

## Adjacency Rewrite

The rewrite-side adjacency subsystem is already implemented well beyond the
original “shape only” stage.

- forward and reverse adjacency surfaces
- overflow entries and merged neighborhood reads
- stable-memory hydration and writeback
- locator sidecar
- local rebalance planning and application
- edge insert / replace / tombstone write helpers
- facade/store/service boundaries
- integration overlay and harnesses

This part is no longer blocked on design uncertainty.

## Property Store

The property store is implemented as a stable-memory-backed append-log style
subsystem.

- variable-length key/value records exist
- property record headers and storage encoding exist
- node/edge property stores round-trip through bucket-backed regions
- facade owns node and edge property stores
- integration reads/writes go through facade property-store helpers
- overlay record `PropertyMap` is now cache/materialized-view behavior, not the
  intended long-term source of truth

Current authority model:

- canonical persisted state:
  - stable-memory-backed property store
- logical/integration cache:
  - overlay `PropertyMap`

## Property Index

The property index is implemented as a separate persisted subsystem, not as an
incidental in-memory map.

Implemented pieces:

- `PropertyIndexKey`, `PropertyIndexEntry`, and stable encoding
- logical `PropertyIndex`
- `PropertyIndexNodeStore`
- fixed-size node pages
- overflow page support for oversized node payloads
- paged-area encoding and decoding
- direct stable-memory section readers
- direct node-record readers from stable memory
- direct property/equality scans from persisted node stores
- multi-level build from logical index into persisted shape
- multi-level hydrate back into logical index

The persisted node-store is now the primary representation for property-index
state.

## Property Index Write Path

The write path has moved well past “always rebuild”.

Leaf-level local mutation paths exist for:

- local update without structural change
- insert redistribution across adjacent leaves
- local leaf split
- remove redistribution before merge
- empty leaf collapse
- underfull leaf merge

Internal-level local mutation paths exist for:

- single-root reuse with capacity-aware updates
- parent-local attach after leaf split
- ancestor split propagation on insert
- ancestor compaction on remove
- underfull internal borrow
- underfull internal merge
- ancestor repair propagation

Fallback rebuild still exists, but it is no longer the default or dominant path.

## Property Index Persistence Model

The writeback path now prefers compact persisted images.

Current writeback behavior:

- compact property-index image is written
- logical snapshot is written as empty when node stores are present
- persisted node stores are treated as the primary durable form
- hydrate rebuilds logical indices from node stores when needed

Current hydrate behavior:

- section-aware region path first
- paged node-store sections are required
- logical snapshot is reconstructed from node stores when needed

In all modern paths, the image is normalized back toward the node-store-primary
shape before use.

## Observability and Diagnostics

Observability is implemented at three levels.

### Facade

- unified write history
- shared write-event projections
- formatted history / last-event helpers
- diagnostics trait support for facade/store/service boundaries

### Overlay

- unified overlay write history
- property write summaries
- edge write summaries
- node delete summaries
- bootstrap aggregate summaries
- diagnostics trait support for overlay-facing callers

### Executor

- rewrite overlay tests use debug/diagnostics helpers
- persistent backend tests use representative graph snapshots
- failure helpers produce more informative panic reports

This means we now have both:

- machine-checkable observability contracts
- human-readable debug surfaces

## Transitional Pieces

The following are still transitional:

- logical `PropertyIndexSnapshot` as reconstruction payload
- some rebuild fallback branches in the property-index write path

These are acceptable today, but they are no longer the intended steady state.

## What Changed Recently

The most important recent shifts are:

1. property store became the effective source of truth in integration paths
2. property index gained paged node-store persistence and direct stable-memory
   read paths
3. writeback moved to compact node-store-primary property-index images
4. hydrate paths were normalized so sectioned/paged forms are the main path
5. observability became rich enough to explain property/index shape changes,
   including redistribution, split, merge, collapse, and rebuild

## Recommended Next Steps

## Priority 1: Reduce rebuild fallback further in property-index writes

Recommended work:

- keep expanding local repair for structural updates
- prefer leaf/internal local repair over whole-subtree rebuild
- continue surfacing shape-change kinds through summaries and diagnostics

Goal:

- make rebuild fallback truly exceptional

## Priority 2: Separate reconstruction payload from steady-state payload more clearly

The logical snapshot still exists for in-process logical reconstruction.

Recommended work:

- keep compact writeback as the default
- avoid writing non-empty logical snapshot when paged node stores are present
- make reconstruction-only paths explicit in API naming and docs

Goal:

- remove ambiguity about what is primary and what is only logical rebuild support

## Priority 3: Keep observability aligned with real write paths

Observability is now useful enough that it should stay coupled to structural
changes.

Recommended work:

- whenever a new local mutation path is added, update:
  - summaries
  - shared projections
  - formatted diagnostics
- continue validating exact report strings in a few representative tests

Goal:

- preserve trust in diagnostics as the implementation evolves

## De-prioritized for Now

The following are valuable, but not the current bottleneck.

- further executor test-helper cleanup
- richer persistent-backend debug snapshots
- cosmetic refactors around test helper layout

These should stay behind the property-index and persistence work.

## Practical Guidance for the Next Work Session

If resuming implementation from scratch, the best next target is:

1. open [`crates/graph-pma/src/facade.rs`](/Users/yota/dev/gleaph-project/crates/graph-pma/src/facade.rs)
2. inspect `load_property_index_image_from_stable_memory(...)`
3. keep section-aware and paged-node-store paths visually primary
4. re-run:
   - `cargo test -p gleaph-graph-pma`
   - `cargo test -p gleaph-gql-executor`
   - `cargo check`

If that is already done, the next natural target is local-repair coverage in
`PropertyIndexNodeStore` inside
[`crates/graph-pma/src/property_index.rs`](/Users/yota/dev/gleaph-project/crates/graph-pma/src/property_index.rs).
