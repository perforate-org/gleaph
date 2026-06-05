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

Gleaph is composed of distinct ownership domains.

Responsibilities should not leak across boundaries.

A new feature should usually extend an existing owner rather than creating a new owner.

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

1. Which owner is responsible?
2. Does ownership become ambiguous?
3. Does the change introduce duplicate concepts?
4. Does it violate an existing boundary?
5. Can an existing owner absorb the change?

Prefer extending existing concepts over introducing new ones.

---

## Expected Output

Report:

- Affected ownership domains
- Boundary violations
- New concepts introduced
- Architectural risks
- Recommendation
