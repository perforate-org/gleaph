# Benchmark Discipline

## Purpose

This skill governs benchmark creation, execution, and regression handling.

Use benchmarks for performance-sensitive paths, especially storage, traversal, indexing, query planning, parsing, serialization, and canister execution.

## Benchmark Frameworks

Use canbench by default for Rust code that is Internet Computer-facing or canister-relevant.

For crates that are not directly tied to the Internet Computer, such as gleaph-gql and gleaph-gql-planner, consider using criterion when it is a better fit.

## Running Benchmarks

canbench can run benchmarks whose names contain a specific pattern:

```sh
canbench [PATTERN]
```

At the moment, multiple patterns cannot be specified in one command.

Use pattern-based runs for focused local investigation.

For final benchmark result updates, run `canbench --persist` for every affected crate that has canbench benchmarks.

Do not use pattern matching with --persist.

Update canbench_results.yml comprehensively for all affected benchmark suites.

## Regression Policy

Significant benchmark regressions should be investigated.

Fix the regression unless it is caused by a necessary semantic, safety, or architectural change.

Benchmarks should be kept as lightweight as possible while preserving useful signal.

Benchmark design must remain fit for purpose:

- Measure the path whose performance contract matters, not a convenient proxy unless the proxy is documented.
- Preserve correctness invariants while benchmarking; do not disable maintenance, indexing, tombstone handling, or consistency updates unless the benchmark explicitly measures that variant.
- Keep setup cost, mutation cost, query cost, and derived-state maintenance separate when they answer different questions.
- Record when a benchmark reflects planned behavior rather than implemented behavior.
