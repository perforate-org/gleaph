# Skills Index

## architecture-integrity

Review encapsulation, separation of concerns, invariants, consistency, fitness for purpose, SSOT, DRY, and module boundaries.

## implementation-integrity

Implement architecture-sensitive changes with invariant mapping, complete variant audits, atomic
write validation, adversarial tests, and pre-review self-inspection.

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

Review test refactors and consolidations by mapping plan criteria to assertions and constructing
wrong implementations that could still pass before approval.

## cost-aware-validation

Design tests, PocketIC fixtures, Rust validation loops, and canbench benchmarks for high signal at
bounded compile and runtime cost.

## benchmark

Review benchmark impact, performance regressions, invariant-preserving measurement, and benchmark fitness for purpose.

## rust-workflow

Format, type-check, clippy, test, benchmark, and completion reporting for Rust changes.

## document-date-accuracy

Ensure document dates, relative dates, timelines, release dates, deadlines, and
latest/current claims are anchored to the OS date and verified when unstable.
