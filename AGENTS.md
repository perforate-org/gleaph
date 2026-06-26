# AI Agent Instructions

## Error Handling

Do not fight repeated errors.

When you encounter the same error twice, research the web and identify 3-5 plausible fixes. Then choose the most efficient solution and implement it.

## Skills

Additional project-specific guidance is stored under:

`.agents/skills/`

When working on architecture, design documents, tests, benchmarks,
or major refactors, inspect relevant skills before proceeding.

Consult `.agents/skills/INDEX.md` before major changes.

## Repository Integrity

Preserve encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

Prefer concrete boundary language such as data ownership, invariant enforcement, API surface, dependency direction, execution flow, and source of truth. Avoid vague umbrella terms when a more testable boundary can be named.

Before introducing a new module, abstraction, data structure, dependency, or boundary split, check whether an existing concept already owns the data, invariant, API surface, or execution flow.

Prefer a single source of truth over duplicated knowledge.

Do not place the same domain rule, schema, metadata definition, storage invariant, or boundary contract in multiple locations.

Use the `architecture-integrity` skill for structural changes, boundary changes, new dependencies, new modules, or large refactors.

Use the `gleaph-architecture` skill for changes that affect Gleaph-specific boundaries, including Router, Graph, Property Index, Vector Index, Edge Value, Property Store, GQL extensions, or ICP integration.

## Design Documents

The design/ directory contains active design contracts, not archival notes.

When a code change affects architecture, storage layout, query semantics, public APIs, canister boundaries, indexing behavior, benchmark assumptions, migration requirements, or failure modes, update the relevant design documents in the same patch.

If a design document describes planned behavior rather than implemented behavior, mark that status explicitly.

Use the `design-sync` skill for changes that may invalidate, refine, or require status updates in design documents.

Use the `adr-review` skill for major architectural decisions, especially storage layout, persistence format, query semantics, canister boundaries, indexing strategy, migration strategy, or public API changes.

## Date Accuracy in Documents

When creating, editing, or reviewing documents that include dates, relative time,
timelines, release dates, deadlines, schedules, milestones, or words such as
`today`, `recent`, `latest`, `current`, `now`, `as of`, `last`, or `next`, use the
`document-date-accuracy` skill.

Do not rely on model memory for the current date. Use UTC for document time
notation. Get the anchor timestamp from the OS with:

    date -u +"%Y-%m-%d %H:%M:%S UTC %z"

Convert relative dates to exact calendar dates where possible. Verify unstable
current-state claims before writing them as fact, and mark uncertain or planned
dates explicitly.

## Test-First Contract

Tests are first-class architectural boundaries.

They must preserve the intended domain behavior and cover high-risk areas with sufficient precision.

When behavior changes intentionally, tests may be updated to reflect the new contract. When coverage is insufficient, add tests proactively.

Do not weaken tests merely to make an implementation pass.

Use the `test-contract` skill before modifying domain behavior, storage behavior, query planning, parser behavior, serialization, indexing, or public APIs.

## Benchmark Discipline

For performance-sensitive code, consider adding or updating benchmarks.

Benchmark regressions should be investigated and fixed unless they are justified by a necessary semantic, safety, or architectural change.

Use the `benchmark` skill when modifying traversal, storage layout, indexing, parsing, planning, serialization, or canister-facing execution paths.

## Format, Test, and Benchmark

After completing a meaningful code change, explicitly run formatting, tests, and relevant benchmarks.

Use the `rust-workflow` skill for the expected local validation sequence.

## gql and gql-planner

The gleaph-gql and gleaph-gql-planner crates contain the name Gleaph as an identifier, but they should remain general-purpose crates for GQL (ISO/IEC 39075).

Gleaph-specific, Internet Computer-specific, or ICP-specific implementations and terminology must not encroach upon these crates.

Do not introduce Gleaph-only syntax, ICP-specific built-ins, canister assumptions, or project-specific semantic rules into these crates.

Project-specific behavior should live outside these crates, typically in planning, execution, integration, or extension layers that are explicitly owned by Gleaph.

## Internet Computer

Tested implementation patterns for ICP development are available as agent skills.
Before writing any ICP code, fetch the skills index and remember each skill's name and description:
https://skills.internetcomputer.org/.well-known/skills/index.json

When a task matches a skill's description, fetch its content on-demand from:
https://skills.internetcomputer.org/.well-known/skills/{name}/SKILL.md

Skills contain correct dependency versions, configuration formats, and common pitfalls that prevent build failures.
Always prefer skill guidance over general documentation when both cover the same topic.

## Expected Completion Summary

At the end of a meaningful change, report:

- What changed
- Which skills were used, if any
- Which tests were added or updated
- Which design documents were checked or updated
- Which format, test, and benchmark commands were run
- Any remaining risks or skipped checks
