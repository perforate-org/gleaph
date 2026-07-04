---
name: adr-review
description: Evaluate major architectural changes against encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.
---

# ADR Review

## Purpose

This skill evaluates whether a proposed architectural change justifies modifying the current design.

Use this skill before:

- Storage layout changes
- Persistence format changes
- Sharding changes
- Query execution changes
- Index architecture changes
- Public API changes
- Architectural boundary changes
- New subsystems
- New data or execution domains
- Major refactors

---

## Principle

The burden of proof is on the change.

Existing architecture should be assumed correct until demonstrated otherwise.

A proposal must justify why the current architecture is insufficient.

Do not start from a preferred solution.

Start from a demonstrated problem.

Every ADR review must explicitly evaluate encapsulation, separation of concerns, invariants, consistency, and fitness for purpose. A proposed design should not be accepted only because it works; it must fit the existing boundaries and preserve the invariants that make the system understandable.

---

## Architecture Preservation Bias

Existing architectural concepts should be preserved whenever possible.

Before introducing:

- A new subsystem
- A new data or execution domain
- A new storage layer
- A new index type
- A new protocol
- A new abstraction
- A new persistence mechanism
- A new execution layer

demonstrate why an existing concept cannot reasonably own the required state, invariant, API surface, or execution flow.

Prefer extending an existing concept over creating a new one.

Complexity is a cost, even when functionality improves.

The burden of proof is on the new concept.

Examples:

Prefer:

- Extending Graph over creating a new storage subsystem
- Extending Router over introducing a new orchestration layer
- Extending Property Index over creating another indexing mechanism
- Extending Vector Index over creating another vector subsystem

unless the existing domain would become conceptually incorrect.

---

## Review Procedure

### Step 1: Describe the Problem

State:

- Current behavior
- Current limitation
- Observed issue
- Measured evidence, if available

Do not start with a solution.

Clearly separate facts from assumptions.

---

### Step 2: Evaluate Existing Architecture

Determine whether the problem can be solved by:

- Existing abstractions
- Existing modules
- Existing data, invariant, API, and execution boundaries
- Existing extension points

If the problem can be solved without introducing new concepts, prefer that approach.

Document why existing solutions are insufficient before proposing new ones.

### Step 2a: Prove state representability

For every proposed status, transition, pointer, receipt, or ownership claim, inspect the current
wire/stable types and owning catalogs. State explicitly whether the proposal is representable as-is,
requires a new table/field/variant, or requires a breaking migration. Do not describe a projected
state on an existing record when the record cannot exist yet or its enum cannot represent that
state.

For cross-canister flows, mark every irreversible effect and every following `await`/callback. Add
durable `*Pending`/ack/reconcile states where a remote success can precede local registration or
notification. A remote side effect followed by a failed callback is not a terminal failure and must
not be retried as a fresh create/delete.

---

### Step 3: Generate Alternatives

Identify at least:

- Minimum-change approach
- Moderate-change approach
- Large-scale redesign

Additional alternatives should be listed when appropriate.

For each alternative, explain:

- Benefits
- Drawbacks
- Complexity impact
- Boundary impact

---

### Step 4: Evaluate Costs

Consider:

- Migration complexity
- Compatibility impact
- Documentation updates
- Design document updates
- Testing impact
- Benchmark impact
- Operational impact
- Maintenance burden
- Future extension cost

Complexity must be treated as an explicit cost.

Verify platform-dependent claims against current authoritative documentation. Prefer existing
platform primitives over reimplementing them, and record their preconditions (for example,
controller, subnet, or chunk-store constraints). Avoid remembered numeric limits or lifecycle
semantics.

Audit accounting decisions algebraically: distinguish total balance, reserved balance, available
balance, and actual spend so transitions cannot double-subtract or release active work. Time-based
reservation expiry requires a lease/fence or proof that no external operation remains in flight.

When multiple independently versioned artifacts must interoperate, decide whether activation is per
artifact or an atomic compatible release set. Independent active pointers are unsafe if they can
assemble an unsupported mixed deployment.

---

### Step 5: Evaluate Long-Term Effects

Determine whether the proposal:

- Simplifies the architecture
- Clarifies boundaries
- Improves encapsulation
- Improves separation of concerns
- Strengthens invariant enforcement
- Preserves consistency between canonical and derived state
- Fits the concrete problem without over-generalizing
- Strengthens SSOT
- Reduces duplication
- Improves maintainability
- Improves extensibility

Or instead:

- Introduces new concepts
- Creates additional data or execution domains
- Increases coupling
- Weakens boundaries
- Exposes internal state across APIs
- Spreads one invariant across multiple modules
- Creates additional consistency surfaces
- Uses an abstraction broader than the demonstrated need
- Increases migration burden
- Makes future reasoning harder

---

## ADR Template

### Context

Why does the problem exist?

### Problem

What limitation is being addressed?

### Existing Architecture Assessment

Why can the current architecture not reasonably solve the problem?

### Alternatives

List all viable alternatives.

### Decision

Chosen approach.

### Consequences

Positive effects.

### Trade-offs

Negative effects and accepted costs.

### Migration

Required migration steps.

### Design Documentation Impact

Which design documents must be updated?

---

## Rejection Criteria

Reject if:

- Existing architecture already solves the problem
- Existing concepts can absorb the required state, invariant, API surface, or execution flow
- Boundaries become less clear
- Encapsulation is weakened without a strong justification
- Concerns become mixed across parsing, planning, routing, execution, storage, indexing, or persistence
- Invariant enforcement moves away from the state owner
- Consistency depends on duplicated update logic
- The abstraction is not fit for the demonstrated problem
- SSOT is weakened
- Duplication increases
- Coupling increases
- Complexity increases without sufficient benefit
- Documentation burden outweighs value
- Migration cost outweighs benefit

---

## Expected Output

### Context

### Problem

### Existing Architecture Assessment

### Alternatives

### Recommendation

APPROVE / APPROVE WITH CHANGES / REJECT

### Rationale

### Required Axes Impact

Encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

### Required Design Updates

### Migration Considerations
