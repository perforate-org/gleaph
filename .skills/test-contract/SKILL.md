# Test Contract

## Purpose

This skill ensures that tests remain a first-class boundary for domain correctness.
Tests are not merely regression checks. They define and protect the intended behavior of the system.

## Principles

Tests should cover behavior that is:

- Domain-critical
- Easy to break accidentally
- Hard to validate manually
- Related to storage layout, query semantics, planning, serialization, indexing, or public APIs
- Required to preserve encapsulation, separation of concerns, invariants, consistency, or fitness for purpose

A patch may modify tests when it intentionally changes domain behavior.
A patch should not weaken tests merely to make an implementation pass.

## Review Rules

Before changing code, identify the behavioral contract affected by the change.
If the existing tests do not cover that contract, add or improve tests.

For boundary-sensitive changes, tests should make the intended contract observable:

- Encapsulation: tests exercise public APIs or intended internal seams, not unrelated private layout.
- Separation of concerns: tests fail when parsing, planning, routing, execution, storage, or indexing behavior leaks into the wrong layer.
- Invariants: tests cover both the write path that enforces the invariant and the read path that relies on it.
- Consistency: tests cover canonical state plus derived state such as postings, telemetry, reverse adjacency, or caches.
- Fitness for purpose: tests cover the concrete use case the abstraction was introduced to support, without locking in accidental generality.

When updating tests, distinguish clearly between:

- Correcting an outdated contract
- Extending coverage
- Weakening a test
- Removing obsolete behavior

Weakening or removing tests requires architectural justification.

## Expected Output

When using this skill, report:

- Which behavior is protected by tests
- Which tests were added or updated
- Which invariants, consistency mechanisms, or boundaries are protected
- Whether any test contract changed
- Why the change is valid
