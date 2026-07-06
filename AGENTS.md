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

Use the `code-quality` skill during both implementation and review when a change grows functions,
APIs, modules, helpers, flags, parameters, or abstractions. Keep responsibilities cohesive, avoid
invalid public states and excessive argument lists, remove superseded paths, and reject accidental
complexity or code growth that is disproportionate to the behavior delivered.

Use the `implementation-integrity` skill for architecture-sensitive implementation work so boundary,
atomicity, variant, test-contract, and code-quality checks happen before handoff to review.

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

## PocketIC E2E and Canbench Execution

Use the underlying commands directly by default:

- `cargo test -p gleaph-pocket-ic-tests` — run the full PocketIC E2E suite.
- `cargo test -p gleaph-pocket-ic-tests --test <test-name>` — run one PocketIC test target.
- Run `canbench [PATTERN]` from the affected crate for focused benchmark work.
- Run unfiltered `canbench --persist` from every affected crate when updating final benchmark artifacts.

Do not route ordinary PocketIC or canbench runs through `just` when the direct commands work.

Some macOS editor-hosted terminals cannot run PocketIC's canister-sandbox process chain (server → sandbox launcher → canister sandbox): `install_canister` may hang or the processes may fail to communicate under that terminal/pty context. Use the `just` recipes that delegate to Terminal.app **only as a fallback in an environment where direct PocketIC/canbench execution is known not to work or has failed for that environment-specific reason**:

- `just ic-e2e` — run the full PocketIC E2E suite in Terminal.app (window stays open)
- `just ic-e2e --close` — run the full suite and close the window when done
- `just ic-e2e --all` — alias for the full suite (same as no target)
- `just ic-e2e smoke` — run only the smoke test
- `just ic-e2e smoke --close` — run only the smoke test and close the window when done
- `just ic-e2e <test-name>` — run a specific PocketIC test file in Terminal.app, e.g. `just ic-e2e router_graph_resolution`
- `just ic-e2e <test-name> --close` — run a specific test file and close the window when done
- `just canbench <crate>` — run an affected crate's full canbench suite in Terminal.app
- `just canbench <crate> <pattern>` — run matching benchmarks in Terminal.app

When the direct command fails for an unrelated code, build, or test reason, diagnose that failure normally; do not use `just` merely to bypass it.

### Long-running validation budget

Keep the implementation/review loop responsive. Do not spend tens of minutes waiting synchronously
for PocketIC, full-workspace tests, or canbench.

- Prefer the smallest affected PocketIC target and focused canbench pattern during development.
- Do not start the full PocketIC suite, full workspace test suite, or unfiltered canbench merely for
  extra confidence unless the task, plan, or user explicitly requires it. Unfiltered
  `canbench --persist` remains required when intentionally updating final benchmark artifacts.
- After starting a long-running command, observe it for at most 5 minutes without meaningful output
  and at most 10 minutes total in the active agent turn. If it has not completed, stop it when safe;
  do not keep polling for tens of minutes.
- A command that exceeds this observation budget is **not** a pass. Report it as incomplete or
  deferred, including the last observed state and the exact command the user or a later environment
  can resume.
- Do not replace a timed-out direct run with another long synchronous fallback. Use Terminal.app
  delegation only for a known editor-hosted process-chain failure, then continue other useful work
  instead of waiting for the delegated run.
- Never claim completion based only on `--no-run`, successful compilation, or a background/delegated
  process that has not returned a result. Distinguish build verification from runtime verification.

### herdr plan / implementation / review workflow

When `HERDR_ENV=1`, read and use `.agents/skills/herdr-workflow/SKILL.md` for the repository-specific
plan → implementation → iterative review → validation → final approval → commit → reset workflow.
The primary/coordinator or any pane explicitly supervising that role must also use
`.agents/skills/supervisor-integrity/SKILL.md` for current-state recovery, scope and prerequisite
decisions, independent final inspection, commit authorization, and cross-stage workflow improvement.
Routine pane routing, checkpoints, plan/review iterations, validation dispatch, and skill learning
belong to the designated supervisor and related panes directly; the primary must not be used as a
message relay. Until the user explicitly grants mature-supervisor authority, the primary retains only
the final independent approval and safe-commit gate plus genuine architecture/scope escalations.
Use the global `herdr` skill for CLI mechanics. Keep detailed herdr coordination policy in that skill,
not in `AGENTS.md` or unrelated implementation/review/validation skills.

## Format, Test, and Benchmark

After completing a meaningful code change, explicitly run formatting, tests, and relevant benchmarks.

Use the `rust-workflow` skill for the expected local validation sequence.

### Focused local test loop

The workspace uses `debug = "line-tables-only"` for `[profile.dev]` and
`[profile.test]` to reduce debug-artifact size and link work while keeping
line-level backtraces. Release, bench, and canister profiles are intentionally
unchanged.

During iterative development prefer focused, scoped commands:

- `cargo test -p <crate> --lib <filter>`
- `cargo check -p <crate> --tests`
- `cargo clippy -p <crate> --all-targets --all-features -- -D warnings`

Reserve full-workspace validation, PocketIC E2E runtime, and unfiltered
canbench runs for explicitly required final validation. Use ordinary
`cargo test` for PocketIC/canbench targets and doctest-sensitive paths; use
`cargo nextest run` only where compatibility with the suite is known.

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
