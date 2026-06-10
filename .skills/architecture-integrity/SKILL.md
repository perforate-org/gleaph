# Architecture Integrity Review

## Purpose

This skill reviews repository structure, module boundaries, encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

Its primary goal is not code quality, performance, or style. Its goal is to preserve conceptual integrity over time.

Use this skill whenever:

- A new module is introduced
- A new dependency is added
- Data ownership, invariants, API surfaces, or execution flow move between modules
- A significant refactor is proposed
- An architectural decision is under discussion

---

## Core Principles

### Required Evaluation Axes

Every review must explicitly weigh the change against these axes:

- **Encapsulation:** internal state and storage details remain behind the module API that owns them.
- **Separation of concerns:** parsing, planning, routing, execution, storage, indexing, and persistence concerns do not bleed into each other.
- **Invariants:** the module that owns an invariant enforces it at write boundaries and exposes read APIs that rely on that invariant honestly.
- **Consistency:** duplicated facts, schemas, metadata, or derived indexes stay synchronized through one source of truth and one update path.
- **Fitness for purpose:** the chosen abstraction is no broader, narrower, or more general than the concrete problem requires.

A change that weakens one of these axes should be rejected unless the trade-off is explicit and justified by a stronger domain need.

---

### Conceptual Integrity

A concept should exist in exactly one place.

If multiple implementations, definitions, or ownership models exist for the same concept, the architecture is likely degrading.

Questions:

- Where is the authoritative definition?
- Is ownership clear?
- Can a developer identify the source of truth immediately?

---

### Single Source of Truth (SSOT)

Every piece of knowledge must have one authoritative representation.

Examples of violations:

- Multiple modules defining the same schema
- Duplicated business rules
- Configuration values maintained in several locations
- Parallel metadata systems

Questions:

- What is the canonical source?
- Can conflicting versions exist?
- Is synchronization required?

If synchronization is required, the design should be reconsidered.

---

### DRY (Don't Repeat Yourself)

Knowledge should not be duplicated.
Code duplication is sometimes acceptable.
Knowledge duplication is not.

Examples:

Bad:

- The same business rule implemented in three services

Good:

- Shared implementation
- Shared abstraction
- Shared ownership

Questions:

- Is this knowledge already represented elsewhere?
- Why is duplication necessary?
- Can ownership be centralized?

---

### Encapsulation and Boundary Ownership

Every mutable state, invariant, API surface, and execution flow must have a clear boundary.
Avoid shared mutation or shared authority whenever possible.

Questions:

- Which module owns the state?
- Which module enforces the invariant?
- Which interface exposes the capability?
- Which module should change when the domain rule changes?

If the answer is "multiple modules", the boundary is unclear.

---

### Dependency Direction

Dependencies should flow toward more stable abstractions.

Questions:

- Does a lower-level module depend on a higher-level module?
- Does infrastructure depend on domain logic?
- Does storage depend on query execution?
- Does a leaf module depend on a sibling module?

Architecture should resemble a directed graph with intentional dependency flow.

---

### Boundary Preservation

Module boundaries exist to prevent conceptual leakage.

Questions:

- Is a module accessing another module's internal state?
- Is a module enforcing invariants that belong to another boundary?
- Is a concept crossing boundaries without abstraction?

Boundary violations accumulate architectural debt rapidly.

---

### Minimal Concepts

Do not introduce a new concept unless an existing one is insufficient.

Before adding:

- New module
- New abstraction
- New service
- New data structure

Ask:

- Can an existing concept represent this?
- What limitation prevents reuse?
- What new capability does this concept provide?

Favor extending existing concepts over creating new ones.

---

## Review Procedure

For every architectural change:

### Step 1: Identify Ownership

Determine:

- Which module owns the change
- Which concepts are affected
- Which invariants and interfaces are affected

### Step 2: Search for Existing Concepts

Determine whether equivalent functionality already exists.

Look for:

- Similar modules
- Similar abstractions
- Similar data structures
- Similar workflows

### Step 3: Verify Boundaries

Determine:

- Whether new dependencies are introduced
- Whether dependency direction remains valid
- Whether concerns remain separated by clear interfaces

### Step 4: Verify SSOT

Determine:

- Whether a new source of truth is being created
- Whether existing sources remain authoritative

### Step 5: Verify Long-Term Consistency

Ask:

- Will future developers know where this belongs?
- Will future changes require modifying multiple locations?
- Does this simplify or complicate the architecture?

### Step 6: Verify Required Axes

Determine whether the change preserves or improves:

- Encapsulation of internal state and storage details
- Separation of parsing, planning, routing, execution, storage, indexing, and persistence concerns
- Enforcement of affected invariants at the owning boundary
- Consistency between source-of-truth data and derived state
- Fitness of the abstraction for the actual problem

---

## Output Format

For each review, provide:

### Summary

Short description of the change.

### Ownership Analysis

Which module owns the state, invariant, API surface, or execution flow.

### Boundary Analysis

Potential boundary violations.

### SSOT Analysis

Potential duplication of knowledge or ownership.

### Required Axes Analysis

How the change affects encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

### Architectural Risks

Long-term maintenance concerns.

### Recommendation

One of:

- APPROVE
- APPROVE WITH CHANGES
- REJECT

Include rationale.
