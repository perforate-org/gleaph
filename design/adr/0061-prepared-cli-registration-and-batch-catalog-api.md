# 0061. Prepared-query CLI registration and batch catalog API

This ADR defines the operator registration workflow for prepared queries: the `gleaph prepared`
subcommand, the `prepared/` artifact format, and the Router registration API surface
(multi-operation `prepare` plus `get_prepared`). It resolves ADR 0053 open decisions 1 (Router
metadata API) and 2 (manifest authority). The metadata contract, codegen boundary, and scalar
vocabulary remain owned by ADR 0053 and ADR 0055 respectively.

Date: 2026-08-05
Status: accepted
Last revised: 2026-08-05
Anchor timestamp: 2026-08-05 13:10:38 UTC +0000

## Context

ADR 0053 established the codegen boundary and a provisional Router prepared surface: the
idempotent upsert `prepare(name, query, metadata)`, `drop_prepared`, `list_prepared`
(`PreparedManifest`), and the execution endpoints `prepared_query` / `prepared_mutate`. There is
no operator-facing registration path yet: the only in-tree registration is hand-authored Candid
in `scripts/check-codegen-local-e2e.sh`, which escapes the GQL source with `sed` and hand-writes
the full `PreparedOperation` metadata — including `result.columns`, which it emits empty. The
script also calls the superseded `prepared_upsert_with_metadata` method name (ADR 0056 replaced
it with `prepare`), so it is stale relative to the current Router candid surface.

Router metadata completion is partial: `prepared_documentation.rs` completes parameter
types/nullability from `VALUE <name> TYPED ...` declarations and schema inference, and applies
`///` / `/// @param <name> <text>` doc comments, but it does **not** complete the result schema.
Registered with empty columns, `list_prepared` → codegen produces row types with no columns.

ADR 0058 established immutable, checksummed, ledger-backed artifact discipline for schema
migrations and explicitly reserves a boundary for operations that are not schema migrations. The
project is pre-release: public Candid and CLI surfaces may change without compatibility wrappers
(ADR 0053).

## Problem

Prepared-query registration needs a reproducible, reviewable, locally validated workflow, but the
only existing path is hand-built Candid: manual metadata (result columns, sorts), manual escaping,
no local validation, no drift detection, and no source artifact. It is also unclear whether
registration belongs inside the migration workflow, whose invariants do not fit prepared
semantics.

## Existing architecture and ownership

- **Router** owns the prepared catalog (`ROUTER_PREPARED_PLANS`, MemoryId 8), registration
  authorization (`authorize_prepared_catalog_change`: Admin or Manager with `PREPARE_REGISTER`),
  planning, and metadata completion (`prepared.rs`, `prepared_documentation.rs`).
- **gleaph-prepared-api** owns the language-neutral manifest wire contract (`PreparedManifest`,
  `PreparedOperation`, `SemanticType`).
- **gleaph-prepared-runtime** owns heap-only parse and write-path classification of prepared
  sources (no IC dependencies), so the CLI can reuse it for local `plan` validation.
- **gleaph-cli** owns filesystem artifacts and the IC-agent transport; `RemoteTransport`
  (`remote.rs`) already implements the network/identity conventions shared by `migration` and
  `load`.
- **gleaph-codegen** consumes `PreparedManifest` from Router `list_prepared` or a local JSON file.
- **Graph resolution** at registration is program-derived (`USE GRAPH`) or the caller's home
  graph (ADR 0011); the registration API takes no graph argument.

## Decision

### 1. Dedicated `gleaph prepared` subcommand; registration is not folded into migration

The ADR 0058 migration invariants conflict structurally with prepared semantics:

| Axis              | Migration (ADR 0058)                                                                           | Prepared registration                                                      |
| ----------------- | ---------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------- |
| Statement         | Exactly one additive catalog statement (`CREATE GRAPH TYPE` / `CREATE GRAPH` / `CREATE INDEX`) | Full transaction program: `MATCH … RETURN`, `NEXT` chains, parameters, DML |
| Mutability        | Immutable, checksum identity, append-only chain                                                | Replace-in-place idempotent upsert                                         |
| Durability        | Router `ROUTER_SCHEMA_MIGRATIONS` ledger + exact replay                                        | No ledger; the prepared catalog is the state                               |
| Forbidden content | Parameters, `SESSION`, `NEXT` chains                                                           | All of these are normal                                                    |

Folding prepared registration into migration would weaken its statement validation, inflate the
manifest, and pollute the schema-application audit trail with application-query history. ADR 0058
itself mandates an explicit boundary for non-schema operations.

### 2. Artifact format (v1)

A `prepared/` directory (default; `--dir` override), one operation per file:

- **`prepared/<name>.gql`** — the prepared source. The operation name is the file stem. Source
  conventions follow ADR 0053: `///` operation docs, `/// @param <name> <text>`,
  `VALUE <name> TYPED <type> = $<name>` typed parameters, optional `USE GRAPH`.
- **`prepared/<name>.toml`** (optional sidecar, strict TOML, `deny_unknown_fields`) — explicit
  metadata only: `description`, `allowed_sorts`, `supports_consistency`, `supports_idempotency`.
  Parameter types and the result schema are **not** authorable here; the Router completes them
  (§6).
- **Name rule:** `[a-z][a-z0-9-]*` (kebab-case; codegen derives camelCase methods).
- **Strict-file rules** mirror migration: no symlinks, no extra files, no nested directories; the
  source is bounded at 65,536 bytes (the same bound as a migration statement).
- **No `down`/rollback:** reversal is a replace (upsert) or `drop`, matching the replace-in-place
  contract.

### 3. Command surface

```text
gleaph prepared new <name> [--dir PATH] [--description TEXT]
gleaph prepared plan   [--dir PATH]
gleaph prepared status [--dir PATH] --canister PRINCIPAL [-n ic|local|URL] [--identity PEM] [--fetch-root-key]
gleaph prepared apply  [--dir PATH] --canister PRINCIPAL [-n ic|local|URL] [--identity PEM] [--fetch-root-key]
gleaph prepared drop <name> --canister PRINCIPAL [-n ic|local|URL] [--identity PEM] [--fetch-root-key]
```

- **`new`** scaffolds `prepared/<name>.gql` atomically (same-filesystem temporary + rename, as in
  migration) with the minimal schema-agnostic template `MATCH (n) RETURN n` plus a doc line; an
  existing file is rejected (no overwrite).
- **`plan`** is local-only: parse via `gleaph_prepared_runtime` (parse + `requires_write_path`
  classification), validate name format/uniqueness, strict-parse sidecars, enforce the size
  bound. No remote calls.
- **`status`** compares the local directory against the Router: for each local operation,
  `get_prepared(name)` (§5) reports `missing` / `drift` (query bytes or metadata differ) /
  `up-to-date`. Remote-only operations are detected from `list_prepared` names (metadata-bearing
  operations only in v1). Missing or drifted operations make the command exit non-zero.
- **`apply`** runs the local `plan` fast-fail first, then chunks the operations into
  `prepare(batch)` calls (§4). Each batch is all-or-nothing; the upsert is idempotent, so a
  failed batch converges on re-run.
- **`drop`** maps to `drop_prepared(name)`.
- Network/identity flags reuse `RemoteTransport`, identical to `migration status`/`apply`.

### 4. Batch registration API: `prepare` becomes multi-operation

Wire types live in gleaph-prepared-api:

```text
type PreparedRegistration = record { name : text; query : text; metadata : opt PreparedOperation };
prepare : (vec PreparedRegistration) -> (Result<(), RouterError>);
```

- **All-or-nothing within one call:** phase 1 parses, plans, and completes metadata for every
  operation with no writes; phase 2 inserts every stable record and heap-cache entry only after
  all operations planned. A single failure leaves the catalog unchanged.
- **Bounded batch:** `MAX_PREPARED_BATCH = 32` operations per call; planning dominates update
  instruction usage. The CLI chunks larger directories into multiple calls.
- **Duplicate names within a batch are rejected.**
- **Failure reporting** names the offending operation deterministically — the first failure in
  batch order: `InvalidArgument("prepared op '<name>': <reason>")`; no new `RouterError` variant.
- **Graph resolution** stays per-operation, program-derived, unchanged.
- **`drop_prepared`** remains single-operation (rare, operator-initiated).
- **Authorization** is unchanged and checked once per call via
  `authorize_prepared_catalog_change`.

This is a breaking change to the provisional `prepare` signature, permitted while pre-release
(ADR 0053). The single-operation shape is removed rather than kept alongside the batch form.

### 5. `get_prepared` for status diffing

```text
type PreparedOperationRecord = record { query : text; metadata : opt PreparedOperation };
get_prepared : (name : text) -> (Result<PreparedOperationRecord, RouterError>);
```

- Wire type in gleaph-prepared-api; authorization is `authorize_prepared_execute` (same as
  `list_prepared`). Graph resolution scans the caller's visible graphs for a record with that
  name (the same resolution `drop_prepared` uses): zero matches is `NotFound`, more than one is
  an `InvalidArgument` ambiguity error. The ambiguity error is fail-closed (never a silent
  wrong-graph read) and is acceptable for v1 single-graph or name-distinct deployments; a future
  optional graph selector can disambiguate without changing the wire record.
- `list_prepared` (the codegen input) is unchanged: it continues to return metadata-bearing
  operations only.

### 6. Metadata materialization authority

- **CLI authors:** the operation name, the kind (derived locally from `requires_write_path`
  classification), and the explicit sidecar fields (`description`, `allowed_sorts`,
  `supports_consistency`, `supports_idempotency`).
- **Router completes:** parameter types/nullability (existing `complete_parameter_metadata`), doc
  descriptions (existing `apply_to_operation`), and — new — the result schema.
- **Result-schema completion:** after `validate_with_seed`, typed output columns are derived from
  `gleaph_gql::type_check::infer_statement_block_output_types_with_schema(block, schema)`, mapped
  to `SemanticType` by reusing the existing `semantic_type_from_type` helper in
  `prepared_documentation.rs` (Router-owned; the mapping is not lifted — the CLI never needs it,
  and gleaph-prepared-api must stay free of gleaph-gql types), and used to fill
  `result.columns`. Output columns whose `Type` has no `SemanticType` mapping (vertex, edge,
  union, unknown) are **omitted** from the completed schema — the scaffold template
  `MATCH (n) RETURN n` returns a node binding and must not fail registration. Explicit columns
  are validated fail-closed (names, types, nullability) against the inferred output, mirroring
  parameter validation. The planner `OutputSchema` is **not** the substrate: its `OutputColumn`
  carries binding kinds, not semantic types.
- This resolves ADR 0053 open decision 1 (Router metadata API) and open decision 2 (manifest
  authority): registration is source-first with Router-owned materialization.

## Alternatives considered

| Alternative                                                    | Decision and reason                                                                                                                                            |
| -------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Fold registration into `gleaph migration`                      | Rejected: conflicts with ADR 0058 invariants (one additive catalog statement, immutability, ledger semantics) and pollutes the schema-application audit trail. |
| Separate `prepare_many` method                                 | Rejected: a single batch surface on `prepare` keeps one authorization check and one atomic contract; the pre-release API is broken once rather than grown.     |
| `PreparedManifest` JSON as the registration input              | Rejected: dual source of truth for source plus metadata; the manifest stays a codegen output contract.                                                         |
| GQL extension-syntax declarations (ADR 0034 style)             | Rejected for now: grammar cost is high and the existing source-first conventions (`///`, `VALUE … TYPED`) already carry the metadata.                          |
| Per-operation atomicity with partial-failure reporting         | Rejected: all-or-nothing keeps the `status`/`apply` mental model simple, and the idempotent upsert makes retries converge.                                     |
| `status` via an extended `list_prepared` that includes sources | Rejected: `list_prepared` remains a metadata-only codegen contract; `get_prepared` serves the drift check.                                                     |

## Consequences

Positive:

- Registration becomes reproducible, reviewable, and locally validated; the Candid-writing e2e
  script is replaced by `gleaph prepared apply`.
- Codegen row types become populated once the Router completes result schema.
- `status` provides CI-usable drift detection.
- One authorization check and one atomic contract per batch.

Costs and limits:

- Breaking Candid change to `prepare` (pre-release, permitted by ADR 0053); the single-operation
  shape is removed.
- `status` v1 does not detect remote-only operations registered without metadata.
- Result-schema completion maps the supported `Type` vocabulary; output columns without a
  `SemanticType` mapping (vertex/edge/union/unknown) are omitted, so node-returning operations
  keep an empty result schema instead of failing; explicit-column conflicts with inferred types
  remain fail-closed.
- The CLI still cannot validate against the graph type catalog; the Router remains the final
  validator (schema is Router-owned, ADR 0013).
- `get_prepared` / `drop_prepared` name resolution is ambiguous when the same operation name
  exists across multiple caller-visible graphs; the ambiguity fails closed (v1 accepts this for
  single-graph or name-distinct deployments).

## Validation (planned)

- Router unit tests: batch all-or-nothing (one failing op leaves the catalog unchanged),
  first-failure reporting in batch order, duplicate-name rejection, `get_prepared` round-trip
  and ambiguity/`NotFound` behavior, and result-schema completion mapping including
  fail-closed conflicts.
- CLI tests: `plan`/`status`/`apply`/`drop` against a fake transport (mirroring the migration
  `FakeMigrationTransport` pattern), local validation errors, and chunking beyond
  `MAX_PREPARED_BATCH`.
- PocketIC E2E: replace the shell-script Candid registration in `check-codegen-local-e2e.sh`
  with `gleaph prepared apply`, then verify `list_prepared` → codegen emits populated row types.
- Benchmark: a bounded batch-registration canbench (32 ops) to bound update instruction usage.

## References

- [ADR 0053 — Prepared-query code generation and client-runtime boundary](0053-prepared-query-codegen-and-client-runtime-boundary.md)
- [ADR 0055 — Exact scalar types at the Router API boundary](0055-exact-scalar-types-at-router-api-boundary.md)
- [ADR 0058 — Versioned additive schema migrations](0058-versioned-additive-schema-migrations.md)
- [ADR 0011 — GQL graph resolution and catalog scoping](0011-gql-graph-resolution-and-catalog-scoping.md)
- [RBAC and prepared queries](../security/rbac-and-prepared.md)
- `crates/prepared-api/src/lib.rs`, `crates/prepared-runtime/src/lib.rs`
- `crates/cli/src/migration.rs`, `crates/cli/src/remote.rs`, `crates/cli/src/main.rs`
- `crates/router/src/prepared.rs`, `crates/router/src/prepared_documentation.rs`
- `crates/gql/src/type_check.rs` (`infer_statement_block_output_types_with_schema`)
- `scripts/check-codegen-local-e2e.sh` (status quo registration path this ADR replaces)
