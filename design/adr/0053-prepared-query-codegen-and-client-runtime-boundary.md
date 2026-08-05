# 0053. Prepared-query code generation and client-runtime boundary

The manifest scalar vocabulary and result-wire scalar identity are defined by ADR 0055. This
ADR owns the prepared-query/runtime boundary, while exact scalar representation remains owned by
ADR 0055.

Date: 2026-07-29
Status: proposed
Last revised: 2026-08-04
Anchor timestamp: 2026-08-04 18:18:33 UTC +0000

## Context

Gleaph prepared queries have a stable Router execution surface, but there is not yet a
supported, typed way to consume that surface from application code. The intended targets are
TypeScript, JavaScript, Rust application clients, Rust canisters, and Motoko canisters.

The repository already has two runtime boundaries:

- `@gleaph/sdk` in `sdk/client/js`, whose current public runtime is `GraphClient` constructed by
  `createGraphClient` or `createIcGraphClient`. It exposes dynamic GQL and low-level prepared
  execution methods such as `executePrepared` and `executePreparedMutation`.
- `gleaph-cdk` in `sdk/canister/rust`, which is an `ic-cdk` helper for canisters calling the
  Router's prepared_query endpoint, including caller-selected sort specifications. It owns Candid argument encoding and
  inter-canister call/decode errors.

`crates/cli` owns the thin top-level `gleaph` command and delegates `gleaph codegen` to the
shared CLI implementation in `crates/codegen`; it is not the generator's source of truth.
`crates/codegen` owns the generator and its CLI contract. The
current Router stable prepared-query record stores the query source and registration metadata, not
a compiled AST or plan. Parsed ASTs, preserved doc comments, and compiled plans are heap cache data
that can be rebuilt after an upgrade.

This ADR intentionally excludes Graph Procedures, arbitrary canister extensions, ORM/query
builders, and general canister-method generation. Those capabilities require separate execution,
authorization, and consistency decisions.

## Problem

The code generator must produce useful typed APIs without duplicating transport, Candid, prepared
parameter encoding, error handling, or Router execution semantics in every generated language.
It must also avoid exposing Router stable storage or planner internals as a client contract.

The older SDK-side prepared DTOs are insufficient as the long-term cross-language contract:
they contain client-facing parameter hints and query metadata, but do not yet define a stable,
language-neutral result type schema used by code generation. The Router now exposes the
graph-scoped `prepared_manifest` endpoint backed by `gleaph-prepared-api`, and the JS SDK exposes
it as `GraphClient.getPreparedManifest`. The generator library consumes an explicit local manifest
snapshot; the `gleaph-codegen` CLI can also fetch the same manifest from Router.

## Existing architecture assessment

The problem cannot be solved by extending only the current CLI. The CLI is an entrypoint, while
the required behavior spans:

1. Router-owned public prepared metadata and graph scoping;
2. a language-neutral manifest contract;
3. runtime-specific call and encoding ownership; and
4. language-specific typed wrappers.

Putting all four concerns into `crates/cli` would make the CLI an accidental source of truth and
would force Rust canister, Rust client, and JS/TS consumers to reproduce runtime behavior.

The existing SDK/CDK boundaries can absorb the runtime concerns:

- `@gleaph/sdk` owns JS/TS transport and dynamic GQL execution;
- `gleaph-cdk` owns Rust canister inter-canister calls; and
- a future non-CDK Rust client SDK can own Rust application-client transport.

The new concept required by this ADR is therefore the code-generation contract and its generated
adapter boundary, not another execution engine.

## Decision

### 1. Package and crate boundary

The generator is named `gleaph-codegen` and lives in `crates/codegen`.

`codegen` is preferred over `bindgen`: the target is not only Candid actor binding generation. It
also generates prepared-query-specific parameter types, result types, encoding calls, and typed
operation wrappers for several runtimes and languages.

`crates/cli` provides a thin `gleaph codegen` wrapper. It delegates to the same CLI implementation
used by the standalone `gleaph-codegen` binary, so the generator remains the architecture and API
source of truth.

### 2. Manifest is the generator input contract

The generator consumes a versioned prepared-query manifest, either from a local snapshot or from
a Router metadata endpoint. It does not inspect Router stable memory, plan blobs, or planner
internals, and it does not re-parse GQL as part of normal generation.

The language-neutral manifest types are owned by the `gleaph-prepared-api` crate. Router metadata
endpoints and `gleaph-codegen` must depend on this contract crate; the contract crate must not
depend on Router internals or a renderer implementation.

The manifest is graph-scoped and represents one coherent metadata snapshot. It must identify at
least:

- manifest/API version;
- graph identity;
- prepared operation name and query/update kind;
- parameter names, requiredness, nullability, and language-neutral semantic types; and
- result column names and language-neutral semantic types.

Dynamic sort specifications are declared by allowed_sorts and passed to prepared_query; the Router validates them and applies the selected ordering before execution. Caller requirements, consistency capabilities, idempotent-update support, and diagnostic/source fields are optional contract fields and must not be silently reconstructed by a renderer.

The `prepared_manifest` Candid field layout and endpoint are owned by `gleaph-prepared-api` and
the Router. The manifest contract, not the older `ApiPreparedQueryInfo` DTO, is the
cross-language source of truth. Live manifest retrieval by the codegen CLI is implemented through
the Router's `prepared_manifest` query.

### 3. Generated code does not own runtime behavior

Generated code owns only query-specific types, encoders/decoders, operation names, and typed
wrappers. It must delegate transport and common error behavior to the selected runtime.

The intended runtime relationships are:

```text
TypeScript / JavaScript generated code -> @gleaph/sdk
Rust application-client generated code -> future non-CDK Rust SDK
Rust canister generated code -> gleaph-cdk
Motoko canister generated code -> supported Motoko runtime helper
```

The generator must not independently reimplement `ic-agent`, `ic-cdk`, Candid call semantics, or
prepared parameter encoding in every output language.

### 4. Generated APIs compose with the base client

The long-term JS/TS target is a unified Gleaph client with dynamic GQL and generated prepared
operations available together:

```ts
const client = withPreparedQueries(createGleaphClient(actor));

await client.gql.query({ query, params });
await client.prepared.searchUsers(params);
```

The current `GraphClient`/`createGraphClient` API is existing implementation state and is not
renamed by this ADR. Any migration to `GleaphClient`/`createGleaphClient` requires a separate SDK
API decision and compatibility plan.

For Rust, generated code exposes a manifest-specific `PreparedExt` trait implemented for the
SDK's `GleaphClient<Prepared>`, where `Prepared` is a generated marker type selected by
`GleaphClient::with_prepared::<Prepared>`. It must remain possible to use the SDK's dynamic GQL
API without generated code.

### 5. Profiles are separate, but share one manifest model

The generator uses one language-neutral intermediate representation and separate output profiles:

```text
Prepared manifest
  -> normalized codegen IR
       -> TypeScript / JavaScript client profile
       -> Rust application-client profile
       -> Rust canister profile
       -> Motoko canister profile
```

Client and canister profiles are not the same runtime. They share operation metadata and semantic
types but select different call helpers and error surfaces.

### 6. Query/update semantics remain explicit

Generated APIs must preserve the Router execution contract:

- read-only prepared operations call the query endpoint;
- mutations call the update endpoint;
- idempotent mutation calls are exposed distinctly from ordinary updates; and
- explicit read-consistency options are generated only for operations/endpoints that support them.

The generator must not hide a mutation behind a generic query method or infer idempotency from an
arbitrary options object.

## Alternatives considered

### Keep all generation in `crates/cli`

Rejected. This couples a user-facing command to the cross-language manifest and runtime boundary,
and makes the obsolete CLI implementation an accidental architecture owner.

### Name the crate `bindgen`

Rejected for this scope. `bindgen` accurately describes Candid-to-language binding generation, but
this feature also generates prepared operation contracts and runtime adapters for multiple
consumer types.

### Generate self-contained clients with transport and encoding included

Rejected. It duplicates runtime behavior across languages, makes SDK/CDK fixes ineffective for
existing generated code, and exposes multiple implementations of the prepared wire contract.

### Generate only generic `executePrepared(name, params)` helpers

Rejected as the primary API. It can remain a low-level runtime escape hatch, but it does not
provide the typed parameter/result boundary that motivates codegen.

### Make the generated file own `createGleaphClient`

Rejected as the default composition model. It would duplicate the SDK's dynamic GQL client and
couple generated output to a particular SDK version. A generated prepared adapter should compose
with the base client. Renaming the existing SDK factory remains a separate decision.

## Consequences

Positive consequences:

- SDK/CDK remain the source of truth for transport and common call behavior.
- Generated files are replaceable artifacts rather than new runtime libraries.
- The same manifest can produce client and canister bindings.
- Dynamic GQL and generated prepared operations can coexist in one client.
- Router stable plan storage remains encapsulated.
- A future non-CDK Rust SDK has a clear integration point without being conflated with
  `gleaph-cdk`.

Accepted costs and risks:

- Router needs a public, graph-scoped prepared metadata contract. The initial provisional
  endpoint and stable-record storage now exist; the registration payload and authorization policy
  remain subject to this ADR's open decisions.
- The semantic type system and result schema require explicit design and compatibility rules.
- Runtime packages need coordinated version compatibility with generated output.
- Existing `GraphClient` APIs cannot be silently replaced by this ADR.
- Generated code may need a manifest-specific namespace or trait when multiple manifests are used
  in one application.

## Open decisions before accepting this ADR

The following points are intentionally not resolved by this proposed ADR:

1. **Router metadata API:** Is the endpoint `prepared_manifest`, a revised
   `list_prepared_api`, or another name? Is graph selection explicit, and what caller visibility
   and public-execution policy applies?
2. **Manifest authority:** Are parameter/result types inferred and frozen at registration, supplied
   explicitly by the registrar, or allowed to be generated only from a checked-in manifest?
3. **Result schema:** What is the stable row/column wire shape, including nullability, nested
   records, large integers, decimals, paths, and temporal values? Scalar width and floating-point
   representation are governed by [ADR 0055](0055-exact-scalar-types-at-router-api-boundary.md).
4. **SDK naming migration:** Does the JS SDK eventually rename `GraphClient`/`createGraphClient`
   to `GleaphClient`/`createGleaphClient`, and is that in this slice or a separate SDK ADR?
5. **Rust SDK:** What package owns the non-CDK Rust client, and does it share a transport trait with
   generated code or expose a concrete `ic-agent` implementation first?
6. **Generated composition:** Is the primary generated API `withPreparedQueries(client)`, a
   manifest-specific Rust trait/facade, or another composition mechanism?
7. **Compatibility policy:** Once the API is released, which runtime and manifest versions may
   be combined, and does generation fail closed when the versions are unsupported? Until then,
   manifest version `1` remains the development contract and may receive destructive changes
   without a compatibility layer or a version-number increment.

These decisions are required before changing the Router public API or declaring the manifest ABI
accepted. They do not block recording the boundary and scope decisions in this proposed ADR.

## Migration

No implementation migration is authorized by this ADR.

When implementation begins, the bounded order should be:

1. define and review the Router-owned manifest wire contract;
2. add graph-scoped metadata exposure and authorization;
3. align SDK/CDK low-level prepared runtime interfaces;
4. implement one normalized codegen IR and one initial output profile;
5. add the remaining profiles after the wire contract is exercised; and
6. expose the standalone codegen command through a thin `crates/cli` wrapper without moving
   generator ownership out of `crates/codegen`.

### Initial implementation slice

The first implementation is intentionally partial and does not yet accept the Router metadata ABI
as a release-stable contract.
`gleaph-codegen` now provides:

- a versioned local `PreparedManifest` model and fail-closed validation for graph identity,
  operation uniqueness, parameter/result names, sort keys, and query/update semantics;
- optional operation and parameter documentation in the manifest, emitted as target-language line
  documentation by every current generator profile;
- source documentation convention for metadata-aware registration: ordinary /// lines describe
  the operation and /// @param <name> <text> describes an input parameter; explicit metadata
  descriptions take precedence;
- typed GQL value bindings may declare prepared input parameters, for example
  VALUE term TYPED STRING = $term. Metadata-aware registration validates the declared GQL
  semantic type and nullability against the manifest parameter contract before catalog mutation;
- when a parameter is used with a schema-typed graph property, the GQL Phase B constraint API
  may infer its type from the active Graph Type schema. Metadata-aware registration fills in a
  missing parameter declaration from this inference and validates declarations supplied by the
  caller; unconstrained or conflicting inference remains fail-closed;
- a `gleaph-prepared-api` contract crate containing the Candid/Serde manifest types shared by
  future Router metadata endpoints and the generator;
- a GQL-generic output-type inference API used as the substrate for Router-owned manifest
  materialization, without coupling `gleaph-gql` to Gleaph runtime or codegen types;
- a provisional Router `prepared_manifest` query and metadata-aware registration path backed by
  the prepared stable record; the manifest contract remains version `1` and is allowed to change
  destructively before release;
- a source-first prepared runtime record: registration builds the parsed query and physical plan
  before committing the stable query, execution lazily rebuilds a missing cache entry, and
  post-upgrade performs a bounded best-effort cache warmup without trapping on an individual
  parse or planning failure;
- a normalized `CodegenIr` shared by the language profiles;
- TypeScript and JavaScript output profiles exposed by `generate_typescript` and
  `generate_javascript`;
- a Rust application-client profile exposed by `generate_rust`, emitting operation-specific
  parameter/result declarations and a `PreparedExecutor` facade; and
- a Rust canister profile exposed by `generate_rust_canister`, emitting operation-specific
  parameter/result declarations, a `Prepared` marker type, and a `PreparedExt` trait implemented
  for `gleaph_cdk::GleaphClient<Prepared>` that wraps the Router's `prepared_query` (reads) and
  `prepared_mutate` (idempotent updates with an explicit `client_mutation_key`). Path-typed
  parameters and results reference the shared `gleaph-cdk::PathElement` binding whose
  fixed-length vertex/edge ids mirror the Router wire contract (ADR 0005). Generated `*Params`
  structs are `Clone + Debug` only (converted to logical GQL parameters by `into_gql_params` at
  call time, allowing raw `f128`/`f256` fields); generated `*Row` structs and `PreparedResponse`
  derive `CandidType` + serde, with exotic row fields bound through the cdk row-binding wrappers
  (`GqlInt256`, `GqlUint256`, `GqlDecimal`, `GqlFloat16`, `Float128`, `Float256` in
  `gleaph-gql-ic-wire`, plus candid-native `Principal`) so canisters return prepared rows
  directly over their candid interface; and
- a Motoko canister profile exposed by `generate_motoko`, emitting operation-specific
  parameter/result declarations and a transport-neutral typed executor boundary; and
- a standalone `gleaph-codegen` entrypoint that accepts either a local `--manifest <path>` or a
  Router query source (`--canister <principal> --graph <name>`), and writes to stdout or an
  explicit output path. Remote retrieval uses the Router's `prepared_manifest` query and
  supports icp-cli-compatible `-n/--network` selection: `ic` is the default mainnet endpoint,
  `local` selects `http://localhost:8000`, and custom URLs require `--fetch-root-key`.
- a `gleaph` top-level entrypoint in `crates/cli` whose `codegen` subcommand delegates to that
  same CLI implementation and accepts the same codegen options.

The generated TypeScript composes with the current `@gleaph/sdk` `GraphClient`, emits
operation-specific parameter and row types, encodes semantic parameter values, and selects
`executePrepared` versus `executePreparedMutation`. Transport, Candid, authorization, and
common errors remain SDK-owned. The shared manifest shape is an implementation scaffold, not yet
the accepted Router endpoint ABI. Consistency options and idempotent updates fail closed in this
profile until the corresponding runtime methods are part of the stable SDK boundary. The Rust
profile similarly delegates transport, response decoding, and error handling to a generated
`PreparedExecutor` implementation; its `serde_json` parameter map is a provisional runtime
boundary and is not the Router wire ABI.

The Rust canister profile binds generated operations directly to the `gleaph-cdk` client; the
Motoko canister profile remains a runtime boundary scaffold: a future Motoko CDK adapter must
implement Candid encoding, Router calls, response decoding, and error conversion. The
`prepared_manifest` query is implemented; accepted result-wire compatibility policy and
authenticated identity selection for CLI retrieval remain outside this slice.

## Design documentation impact

- `design/architecture/overview.md` should link this ADR as the planned codegen/runtime boundary.
- `sdk/README.md` and the SDK package READMEs should be updated when the runtime integration is
  implemented, not treated as evidence that the planned manifest API already exists.
- `design/implementation-gaps.md` should record the Router metadata/API gap and any prepared
  visibility decision that remains open.
