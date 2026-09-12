---
name: benchmark
description: Review benchmark impact, performance regressions, invariant-preserving measurement, and benchmark fitness for purpose.
---

# Benchmark Discipline

## Purpose

This skill governs benchmark creation, execution, and regression handling.

Use benchmarks for performance-sensitive paths, especially storage, traversal, indexing, query planning, parsing, serialization, and canister execution.

## Benchmark Frameworks

Use canbench by default for Rust code that is Internet Computer-facing or canister-relevant.

For crates that are not directly tied to the Internet Computer, such as gleaph-gql and gleaph-gql-planner, consider using criterion when it is a better fit.

## Running Benchmarks

Follow [rust-workflow — PocketIC and Canbench](../rust-workflow/SKILL.md#pocketic-and-canbench)
for focused commands and complete final-artifact updates. Its execution budget and completion
evidence rules apply to benchmark runs.

## Regression Policy

Significant benchmark regressions should be investigated.

Fix the regression unless it is caused by a necessary semantic, safety, or architectural change.

Benchmarks should be kept as lightweight as possible while preserving useful signal.

Benchmark design must remain fit for purpose:

- Measure the path whose performance contract matters, not a convenient proxy unless the proxy is documented.
- Preserve correctness invariants while benchmarking; do not disable maintenance, indexing, tombstone handling, or consistency updates unless the benchmark explicitly measures that variant.
- Keep setup cost, mutation cost, query cost, and derived-state maintenance separate when they answer different questions.
- Record when a benchmark reflects planned behavior rather than implemented behavior.
