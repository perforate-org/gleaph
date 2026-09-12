# AI Agent Instructions

## Error Handling

When a failure recurs, stop speculative retries. Inspect diagnostics, owning code, and
dependency versions; choose the smallest evidence-backed fix that preserves the contract.
Research external sources only when relevant external facts cannot be established locally.
If blocked, report the missing evidence rather than keep guessing.

## Skills

Additional project-specific guidance is stored under:

`.agents/skills/`

When working on architecture, design documents, tests, benchmarks,
or major refactors, inspect relevant skills before proceeding.

Consult `.agents/skills/INDEX.md` before major changes.

## Repository Integrity

Preserve encapsulation, separation of concerns, invariants, consistency, and fitness for purpose.

## Pre-production Simplicity

Gleaph is pre-production: keep one canonical current layout and contract, require fresh state or
reinstall when that layout changes, and delete superseded paths. Do not add backward-compatibility
branches, migrations, legacy decoders, rebuild cursors, fallbacks, or obsolete pre-launch commentary
unless the user explicitly requires compatibility with deployed production data.

Prefer concrete boundary language such as data ownership, invariant enforcement, API surface, dependency direction, execution flow, and source of truth. Avoid vague umbrella terms when a more testable boundary can be named.

Before introducing a new module, abstraction, data structure, dependency, or boundary split, check whether an existing concept already owns the data, invariant, API surface, or execution flow.

Prefer a single source of truth over duplicated knowledge.

Do not place the same domain rule, schema, metadata definition, storage invariant, or boundary contract in multiple locations.

Use the `architecture-integrity` skill for structural changes, boundary changes, new dependencies, new modules, or large refactors.

Use the `gleaph-architecture` skill for changes that affect Gleaph-specific boundaries, including Router, Graph, Property Index, Vector Index, Edge Value, Property Store, GQL extensions, or ICP integration.

Use the `code-quality` skill during both implementation and review when a change grows functions,
APIs, modules, helpers, flags, parameters, or abstractions. Keep responsibilities cohesive, avoid
invalid public states and excessive argument lists, remove superseded paths, and reject accidental
complexity or code growth that is disproportionate to the behavior delivered.

Use [implementation-integrity](.agents/skills/implementation-integrity/SKILL.md#when-to-use) when
implementing persisted-state, write-atomicity, schema/enum-variant, or ownership/API-boundary
changes, or fixing concrete implementation review findings.

## Design Documents

The design/ directory contains active design contracts, not archival notes.

When a code change alters a contract that a design document describes (architecture, storage layout, query semantics, public APIs, canister boundaries, indexing behavior, benchmark assumptions, migration requirements, or failure modes), update that document in the same patch. Do not update design documents for changes that do not affect their contracts.

If a design document describes planned behavior rather than implemented behavior, mark that status explicitly.

Use the `design-sync` skill for changes that may invalidate, refine, or require status updates in design documents.

Use the `adr-review` skill for major architectural decisions, especially storage layout, persistence format, query semantics, canister boundaries, indexing strategy, migration strategy, or public API changes.

## Date Accuracy in Documents

Use [document-date-accuracy](.agents/skills/document-date-accuracy/SKILL.md#when-to-use) when adding,
changing, or verifying calendar dates, deadlines, or time-dependent release/update claims.
A document's type or incidental words such as `current` and `next` are not triggers.

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

## Validation

After meaningful code changes, run formatting, affected tests, and relevant benchmarks.
Before Rust implementation checks or assigned Rust/PocketIC/canbench validation, read
[rust-workflow](.agents/skills/rust-workflow/SKILL.md). It is the source of truth for command
selection, execution budgets, direct-command fallbacks, independent-validation restrictions,
and completion evidence.

For test placement and fixture cost, use
[cost-aware-validation](.agents/skills/cost-aware-validation/SKILL.md); it does not define a
separate execution sequence.

## gql and gql-planner

gleaph-gql and gleaph-gql-planner must remain general-purpose GQL crates (ISO/IEC 39075). Do not introduce Gleaph-specific, ICP-specific, or canister-specific implementations, terminology, syntax, or semantic rules into them. Project-specific behavior belongs in planning, execution, integration, or extension layers owned by Gleaph.

## Internet Computer

When a change depends on ICP-specific APIs, configuration, lifecycle, or persistence behavior,
consult the relevant official ICP skill. Use the index only when needed to find that skill:
https://skills.internetcomputer.org/.well-known/skills/index.json

Fetch only the task-relevant skill content:
https://skills.internetcomputer.org/.well-known/skills/{name}/SKILL.md

Check guidance against the repository's dependency versions. Resolve missing or conflicting
guidance using the official specification or API documentation for the relevant version.


