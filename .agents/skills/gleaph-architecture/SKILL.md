---
name: gleaph-architecture
description: Review Gleaph-specific boundaries with emphasis on encapsulation, invariant ownership, derived-state consistency, and crate fitness for purpose.
---

# Gleaph Architecture

## Purpose

This skill protects the architectural boundaries and design principles of Gleaph.

Use this skill whenever a change affects:

- Router
- Graph storage
- Property Index
- Vector Index
- GQL integration
- ICP integration
- Edge storage
- Property storage
- Query execution
- Sharding
- Persistence

This skill assumes the current architecture is correct unless explicitly superseded by an ADR.

---

## Core Principle

Gleaph is composed of distinct data and execution domains.

State, invariants, and execution concerns should not leak across boundaries.

A new feature should usually extend the existing domain that owns the relevant state, invariant, API surface, or execution flow.

Every Gleaph-specific architecture review must preserve:

- **Encapsulation:** Router, Graph, indexes, storage, and GQL crates expose capabilities through intentional APIs, not internal state shortcuts.
- **Separation of concerns:** portable GQL crates stay language-oriented; Router coordinates; Graph executes and stores; indexes answer index reads.
- **Invariants:** placement, postings, edge identity, labels, payloads, tombstones, and stable layout rules are enforced by the domain that owns the corresponding state.
- **Consistency:** derived state such as postings, telemetry, reverse adjacency, and cached plans has one update path from the canonical source.
- **Fitness for purpose:** Gleaph-specific extensions solve a concrete product or storage need without polluting general-purpose crates.

---

## Ownership Model

### Router

Owns:

- External API entry points
- Authentication
- Authorization
- GQL parsing and planning
- Label and property name resolution
- Shard management
- Query orchestration
- Result aggregation

Must not own:

- Graph storage
- Property storage
- Vector storage

---

### Graph

Owns:

- Vertex storage
- Edge storage
- Adjacency structures
- Traversal execution
- Local graph operations

Must not own:

- Global orchestration
- Property indexes
- Vector indexes

---

### Property Index

Owns:

- Property lookup
- Property search
- Property indexing

Must not own:

- Graph traversal
- Graph storage

---

### Vector Index

Owns:

- ANN search
- Vector indexing
- Quantization
- HNSW
- IVF
- Vector ranking

Must not own:

- Graph traversal
- Property indexing

---

## Edge Payload vs Property Store

Edge Payload and Property Store serve different purposes.

### Edge Payload

Use for:

- Frequently accessed values
- Compact fixed-size payloads
- Traversal-critical information
- Ranking weights

### Property Store

Use for:

- Rich metadata
- Variable-size data
- Infrequently accessed data
- Searchable attributes

Do not duplicate the same information in both systems without justification.

---

## GQL Boundaries

gleaph-gql and gleaph-gql-planner are generic GQL crates.

They must remain independent from:

- Gleaph storage
- Internet Computer
- Canisters
- Principals
- Vendor-specific execution assumptions

Gleaph-specific behavior belongs in integration layers.

---

## Design Review Questions

For every change:

1. Which domain owns the state, invariant, API surface, or execution flow?
2. Does the boundary become ambiguous?
3. Does the change introduce duplicate concepts?
4. Does it violate an existing boundary?
5. Can an existing domain absorb the change without weakening encapsulation or separation of concerns?
6. Does the change preserve invariant enforcement and consistency between canonical and derived state?
7. Is the abstraction fit for the concrete Gleaph need, without becoming a generic bucket for unrelated behavior?

Prefer extending existing concepts over introducing new ones.

---


## Bootstrap and authority

### Durable authority

Any principal or binding that authorizes later writes must be persisted in stable
memory, not in a thread-local cell. A heap-only `RefCell` or `Cell` that survives only
the current execution is not a canonical authority root; after an upgrade clears the heap,
the canonical state is lost and the authorized path becomes unusable.

Bootstrap bindings are seeded via init args. The `init` handler accepts a typed
`bootstrap_bindings: Vec<DeploymentBinding>` and writes each binding to the durable trust
store directly. The init-time seed is durable (init runs on every install / upgrade) and
the bindings become the source of truth for later authorization.

A separate durable bootstrap authority region is its own prerequisite slice. If a slice
needs a durable bootstrap authority stored in a separate stable-memory region, that
requires a stable-layout decision (ADR 0007 + inventory), a dedicated plan, and a separate
review. Do not smuggle a heap-only authority into a slice and document it as deferred.

### Pure-reject public API

A public ingress method whose entire reachable behavior is rejection (no path to success)
is a code smell. Either remove/defer the method from the public surface or implement a
meaningful operation authorized by a canonical durable authority. A pure-reject API is
not a foundation; it is a placeholder that lies about the surface.

Add an adversarial test that walks the public ingress surface and asserts at least one
path to success for each handler (or records that the method is explicitly deferred and
absent from the public surface).

## Stable collection layout

A single `ic_stable_structures` collection owns exactly one `MemoryId`. Two
collections (a `StableCell` and a `StableBTreeMap`, two `StableBTreeMap`s, etc.)
placed on the same `MemoryId` write at the same offset 0 and silently corrupt
each other at runtime. Per-MemoryId allocation is mandatory, not stylistic.

Use a true `StableCell<Option<T>>` (or the existing `StableGraphMetadata` pattern
at `crates/graph/src/facade/stable/metadata.rs:14-46`) for per-canister
singletons such as durable bootstrap authority. Do not substitute a
`StableBTreeMap<(), T>` with a single key as a singleton — that is a workaround
that loses StableCell's first-write semantics and explicit init/get separation.

For every `StableBTreeMap<_, V>` in a plan, specify the `V` type, the `Storable`
implementation (typically Candid-encoded with `StorableBound::Unbounded`), and
whether the value is a per-key scalar or a wrapper. A `Vec<T>` value requires an
explicit persisted wrapper with `Storable` (e.g. `BootstrapAuthHistory(Vec<T>)`).
State whether the design is "read-modify-write the wrapper" (acceptable for
bounded/small histories) or append-oriented keys (`(principal, seq)`) before
implementation, not after.

When authority is derived from initialization ordering (for example, "the first
bootstrap binding's governance principal is the durable authority"), persist
the selected authority as a first-class durable datum. Do not reconstruct it
from append-only audit logs, map iteration, or incidental collection order.

## Append-heavy stable logs

A `StableBTreeMap<Principal, History<Entry>>` log that grows under multi-principal
ingress pressure is a different shape from a per-key scalar or a small bounded
collection. It needs a bounded append behavior, a per-principal history limit or
compaction strategy, and an upgrade-stable entry encoding **before** the log
becomes production-critical. The default "read history, append entry, rewrite
history" pattern is acceptable for bounded/small histories and unacceptable for
unbounded ones.

Define in the plan, before the first handler write:

1. **Entry size bound**: a maximum byte width per entry; reject or compact when
   exceeded.
2. **Per-principal cap**: an upper bound on the number of entries per principal
   key, plus the eviction policy (LRU, FIFO, hash-bucketed summary).
3. **Compaction or summary policy**: how the log sheds entries (rotation, summary
   rows, archival) and how the summary is itself stored.
4. **Append path ownership**: which handler writes the entry, and whether the
   write precedes or follows the durable state mutation it records.
5. **Upgrade-stable encoding**: the entry's Candid or byte encoding must remain
   stable across canister upgrades; an entry added today must decode correctly
   after a future schema revision.

Use append-oriented keys (`(principal, sequence)`) when the log is truly
unbounded and compaction is not yet designed. Use a wrapper read-modify-write
(`History(Vec<Entry>)` with `Storable`) when the per-principal count is known
to stay small (for example, a governance audit log with a small principal set).

Adopt this rule for any audit log, telemetry log, or per-key event stream in
stable memory. Flag the absence of a bounded-append specification as a P2
finding when reviewing a slice that introduces a new log-shaped collection.

## Expected Output

Report:

- Affected data and execution domains
- Boundary violations
- Encapsulation, separation-of-concerns, invariant, consistency, and fitness-for-purpose impacts
- New concepts introduced
- Architectural risks
- Recommendation
