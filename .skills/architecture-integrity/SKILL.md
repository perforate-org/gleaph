# Architecture Integrity Review

## Purpose

This skill reviews repository structure, module boundaries, and architectural consistency.

Its primary goal is not code quality, performance, or style. Its goal is to preserve conceptual integrity over time.

Use this skill whenever:

- A new module is introduced
- A new dependency is added
- A responsibility is moved between modules
- A significant refactor is proposed
- An architectural decision is under discussion

---

## Core Principles

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

### Explicit Ownership

Every responsibility must have a clear owner.
Avoid shared ownership whenever possible.

Questions:

- Which module owns this concern?
- Which module is responsible for correctness?
- Which module should be modified when requirements change?

If the answer is "multiple modules", ownership is unclear.

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
- Is a module performing responsibilities that belong elsewhere?
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
- Whether responsibilities remain isolated

### Step 4: Verify SSOT

Determine:

- Whether a new source of truth is being created
- Whether existing sources remain authoritative

### Step 5: Verify Long-Term Consistency

Ask:

- Will future developers know where this belongs?
- Will future changes require modifying multiple locations?
- Does this simplify or complicate the architecture?

---

## Output Format

For each review, provide:

### Summary

Short description of the change.

### Ownership Analysis

Which module owns the responsibility.

### Boundary Analysis

Potential boundary violations.

### SSOT Analysis

Potential duplication of knowledge or ownership.

### Architectural Risks

Long-term maintenance concerns.

### Recommendation

One of:

- APPROVE
- APPROVE WITH CHANGES
- REJECT

Include rationale.
