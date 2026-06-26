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

## Expected Output

Report:

- Affected data and execution domains
- Boundary violations
- Encapsulation, separation-of-concerns, invariant, consistency, and fitness-for-purpose impacts
- New concepts introduced
- Architectural risks
- Recommendation
