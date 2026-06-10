# Design Sync

## Purpose

This skill keeps the design/ directory consistent with the current implementation.

Design documents are active architectural contracts. They are not historical notes unless explicitly marked as archived.

Use this skill whenever a change may affect:

- Architecture
- Module boundaries and ownership of state, invariants, API surfaces, or execution flow
- Storage layout
- Query semantics
- GQL extensions
- Public APIs
- Canister boundaries
- Index behavior
- Benchmark assumptions
- Migration requirements
- Failure modes or invariants

## Core Rule

A code change and its design contract must not diverge.

If implementation behavior changes, update the relevant design document in the same patch.

If the design document is intentionally ahead of implementation, mark that section clearly as planned, partial, or not yet implemented.

Design documents must make architectural boundaries testable: name the canonical source of truth, the owner of each invariant, the API surface that preserves encapsulation, the separation between concerns, and why the chosen abstraction fits the stated problem.

## Document Status

Every major design document should declare its status near the top.

Allowed statuses:

- Implemented
- Partially Implemented
- Planned
- Experimental
- Deprecated
- Archived

Do not leave ambiguous future-facing text in active documents.

## Review Procedure

### Step 1: Identify Affected Design Areas

Check whether the change affects any document under design/.

Look especially for documents related to:

- Storage
- Query planning
- Parser behavior
- Router / Graph / Index boundaries
- Property model
- Vector index
- Canister execution
- Benchmarks

### Step 2: Compare Design and Implementation

Verify that the document still describes the implementation accurately.

Look for:

- Outdated data layouts
- Old module names
- Removed assumptions
- Incorrect ownership
- Unclear encapsulation or boundary language
- Invariants described without an enforcing module or write path
- Derived state described without a consistency mechanism
- Abstractions that no longer fit the implemented behavior
- Stale benchmark expectations
- Future plans written as current behavior

### Step 3: Update or Mark Status

If the design is still valid, no document change is required.

If the design is outdated, either:

- Update it to match the implementation
- Mark the relevant section as deprecated
- Move historical material to an archived section
- Mark future-facing material as planned

### Step 4: Preserve Decision History

Do not silently delete important architectural rationale.

When replacing an old design, preserve the reasoning if it still explains why alternatives were rejected.

Prefer adding an ADR for major changes.

## Expected Output

When using this skill, report:

- Which design documents were checked
- Which design documents were updated
- Which implementation behavior changed
- Whether any design document remains intentionally ahead of implementation
- Whether an ADR is recommended
