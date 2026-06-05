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

A patch may modify tests when it intentionally changes domain behavior.
A patch should not weaken tests merely to make an implementation pass.

## Review Rules

Before changing code, identify the behavioral contract affected by the change.
If the existing tests do not cover that contract, add or improve tests.

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
- Whether any test contract changed
- Why the change is valid
