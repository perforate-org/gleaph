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
- Architectural responsibility changes
- New subsystems
- New ownership domains
- Major refactors

---

## Principle

The burden of proof is on the change.

Existing architecture should be assumed correct until demonstrated otherwise.

A proposal must justify why the current architecture is insufficient.

Do not start from a preferred solution.

Start from a demonstrated problem.

---

## Architecture Preservation Bias

Existing architectural concepts should be preserved whenever possible.

Before introducing:

- A new subsystem
- A new ownership domain
- A new storage layer
- A new index type
- A new protocol
- A new abstraction
- A new persistence mechanism
- A new execution layer

demonstrate why an existing concept cannot reasonably absorb the responsibility.

Prefer extending an existing concept over creating a new one.

Complexity is a cost, even when functionality improves.

The burden of proof is on the new concept.

Examples:

Prefer:

- Extending Graph over creating a new storage subsystem
- Extending Router over introducing a new orchestration layer
- Extending Property Index over creating another indexing mechanism
- Extending Vector Index over creating another vector subsystem

unless the existing owner would become conceptually incorrect.

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
- Existing ownership boundaries
- Existing extension points

If the problem can be solved without introducing new concepts, prefer that approach.

Document why existing solutions are insufficient before proposing new ones.

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
- Ownership impact

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

---

### Step 5: Evaluate Long-Term Effects

Determine whether the proposal:

- Simplifies the architecture
- Clarifies ownership
- Strengthens SSOT
- Reduces duplication
- Improves maintainability
- Improves extensibility

Or instead:

- Introduces new concepts
- Creates additional ownership domains
- Increases coupling
- Weakens boundaries
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
- Existing concepts can absorb the responsibility
- Ownership becomes less clear
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

### Required Design Updates

### Migration Considerations
