# Skills Index

## figma-file-io

Read and write Figma `.fig` binary files directly using `openfig-core` and `zstd-codec`.
Parse VARIABLE nodes, sync design tokens bidirectionally with JSON, and re-encode
modified `.fig` files without REST API round-trips.

## architecture-integrity

Review encapsulation, separation of concerns, invariants, consistency, fitness for purpose, SSOT, DRY, and module boundaries.

## implementation-integrity

Use for persisted-state, atomicity, variant, or ownership/API-boundary implementation and concrete
implementation review fixes. See [applicability](implementation-integrity/SKILL.md#when-to-use).

## code-quality

Keep implementations and reviews simple and maintainable by controlling complexity, API parameter
growth, invalid states, unnecessary abstraction, duplication, bloat, and change amplification.

## gleaph-architecture

Review Gleaph-specific boundaries with emphasis on encapsulation, invariant ownership, derived-state consistency, and crate fitness for purpose.

## design-sync

Keep design documents synchronized with implementation and explicit about boundaries, invariants, consistency mechanisms, and abstraction fit.

## adr-review

Evaluate major architectural changes against encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

## test-contract

Review behavioral contracts, invariants, consistency mechanisms, boundaries, and test coverage.

## adversarial-test-review

Perform strictly read-only diff and test-contract review by mapping plan criteria to assertions and
constructing wrong implementations that could still pass. Never edit, validate, manage processes,
or open external terminals in this mode.

## cost-aware-validation

Use when choosing test layers or adding or consolidating test and benchmark fixtures.
For execution policy, including independent validation, use `rust-workflow`.

## benchmark

Review benchmark impact, performance regressions, invariant-preserving measurement, and benchmark fitness for purpose.

## rust-workflow

Use for Rust/PocketIC/canbench command selection, execution budgets, independent validation,
and completion evidence.

## document-date-accuracy

Use when adding, changing, or verifying calendar dates, deadlines, or time-dependent release/update
claims. See [applicability](document-date-accuracy/SKILL.md#when-to-use).
