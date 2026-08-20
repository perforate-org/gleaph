# 0069. Rust application-client SDK and shared Router wire contract

Date: 2026-08-20
Status: accepted
Last revised: 2026-08-20 11:28:11 UTC +0000
Anchor timestamp: 2026-08-20 11:28:11 UTC +0000

## Context

Prepared queries have a stable Router execution surface and typed bindings for
TypeScript/JavaScript, Rust canisters, and Motoko. The Rust **application** client (code that
calls the Router from outside a canister, over `ic-agent`) was left as an open decision in
[ADR 0053](0053-prepared-query-codegen-and-client-runtime-boundary.md) (decision 5). The
Rust application-client codegen profile (`generate_rust`) emitted a provisional
`PreparedExecutor` + `PreparedQueries` facade whose transport and response decoding were a
runtime-boundary scaffold, and it did not mirror the mature `PreparedExt` profile used by the
canister SDK (`gleaph-cdk`).

The canister SDK (`gleaph-cdk`, `sdk/canister/rust`) owns the Router data-plane wire contract
directly in `src/types.rs`: the `GqlQueryResult` envelope, `ReadMode`, `MutationToken`,
`RouterError`, and the durable bulk-load family. A client SDK would have to either duplicate
those types or depend on `gleaph-cdk`, which pulls in `ic-cdk` (a canister-only runtime) and is
inappropriate as an application-client dependency.

## Decision

### 1. A neutral `gleaph-router-wire` crate owns the Router data-plane wire contract

Move the Router-facing wire types out of `gleaph-cdk::types` into a new crate
`gleaph-router-wire` (`crates/router-wire`). It owns:

- the query/mutation response envelope (`GqlQueryResult`) and its `decode_*` helpers;
- read freshness contracts (`ReadMode`) and mutation tokens (`MutationToken`,
  `MutationTokenShard`, `MutationLifecyclePhase`);
- the `RouterError` type and `VectorActivationBlockReason`;
- the durable bulk-load command family (ADR 0057) and its receipts/status page;
- the row-decode helpers shared by generated bindings (`FromGqlRow`, `take_gql_row_field`,
  `gql_value_to_json`, `gql_record_to_json_map`, `gql_wire_value_to_json`,
  `gql_principal_from_value`).

`gleaph-cdk` re-exports these under its existing `gleaph_cdk::types` module so existing canister
consumers (including `gleaph-codegen` output and the `rust-canister-app` example) are unchanged.
`gleaph-cdk` no longer declares `ic-cdk`'s optional numeric crates (`ethnum`, `half`,
`rust_decimal`, `f256`) as direct dependencies; they move behind the shared crate's feature
gates.

`CallError` is intentionally **not** shared: it is transport-specific (the canister `ic-cdk`
reject surface versus the client `ic-agent` error surface) and each SDK owns its own.

### 2. A new `gleaph-sdk` crate is the Rust application client

`gleaph-sdk` (`sdk/client/rust`) is the application-client counterpart to `gleaph-cdk`. It
depends on `gleaph-router-wire` (never on `ic-cdk`) and mirrors the canister SDK's typed surface:

- `GleaphClient<Prepared>` with a `NoPrepared` default marker and `with_prepared::<Prepared>()`,
  matching `gleaph-cdk` and the generated `PreparedExt` trait;
- dynamic GQL (`gql_query`, `gql_query_with_mode`, `gql_mutate`), prepared operations
  (`prepared_query`, `prepared_mutate`), bulk-load, `prepare`, `drop_prepared`, and
  `list_prepared`;
- `CallError` in the same `Reject` / `Decode` / `Router` shape, mapping `ic_agent::AgentError`
  rejections onto the `Reject` variant.

Transport is a small `GleaphTransport` trait
with a concrete `IcAgentTransport` built from `GleaphClientOptions`. The `Arc<dyn GleaphTransport>`
indirection lets application code substitute a fake transport in tests while the generated
`PreparedExt` implementation stays transport-agnostic.

### 3. Caller identity is injected through the agent identity

The application caller is the `ic_agent::Identity` bound to the agent. `GleaphClientOptions`
carries an optional `Box<dyn ic_agent::Identity>` (defaulting to the anonymous identity); the
`IcAgentTransport` sets it on the `AgentBuilder` before building. `GleaphClient::caller()`
surfaces `agent.get_principal()` so caller-dependent prepared queries (`IC.MSG_CALLER()` patterns)
can be debugged. II delegation is supported by passing a `DelegatedIdentity`.

### 4. The Rust codegen profiles share one renderer

Extract the canister profile's mature generator into a shared renderer
`crates/codegen/src/rust/shared.rs`, parameterized by a `RuntimeProfile` (`path` and whether
Candid row derives are emitted):

- `gleaph_cdk` profile: `path = "gleaph_cdk"`, Candid derives on rows/envelope (canisters return
  prepared rows over their Candid interface).
- `gleaph_sdk` profile: `path = "gleaph_sdk"`, no Candid derives (application clients only
  deserialize locally).

Both profiles now emit the same `*Params` / `*Row` / `PreparedResponse` / `Prepared` /
`PreparedExt for GleaphClient<Prepared>` shape; only the runtime path differs. The provisional
`PreparedExecutor` / `PreparedQueries` facade and the old `PreparedDateTime` /
`PreparedZonedDateTime` / `PreparedDuration` client structs are removed; the generated client
profile now binds exotic row types through `gleaph_sdk::` row-binding wrappers exactly like the
canister profile.

## Consequences

Positive:

- `gleaph-cdk` and `gleaph-sdk` share one authoritative Router wire contract; the two SDKs can
  no longer drift on the response envelope, `RouterError`, read modes, or bulk-load types.
- Application clients get a typed, canister-parity client with caller identity injection and the
  same generated `PreparedExt` boundary as canisters.
- The codegen Rust profiles share one renderer, so a runtime-path or row-binding fix applies to
  both the canister and application-client output.
- The Rust client no longer re-implements transport/decoding per generated file; it owns a single
  `ic-agent` transport.

Accepted costs and risks:

- `gleaph-router-wire` is a new crate; `gleaph-cdk`'s wire types move but are re-exported, so the
  public `gleaph_cdk::` surface is unchanged.
- The generated Rust application-client output changed from the `PreparedExecutor` scaffold to
  the `PreparedExt` model; any existing consumer of the scaffold must regenerate and switch to
  `GleaphClient::with_prepared::<Prepared>`.
- The SDK and canister profiles still differ on the `CandidType` derive, which is intentional and
  owned by the `RuntimeProfile` flag.

## Alternatives considered

### Depend on `gleaph-cdk` from the client SDK

Rejected. `gleaph-cdk` depends on `ic-cdk`, which is a canister-only runtime. Making an
application client depend on it would force canister-runtime types into a process-side SDK and
couple the two SDKs' transport concerns.

### Duplicate the wire types in `gleaph-sdk`

Rejected. It would create two sources of truth for the Router wire contract; a `ReadMode`,
`RouterError`, or bulk-load shape change would require updating both SDKs in lockstep.

### Keep the `PreparedExecutor` scaffold and add a transport impl

Rejected. It left the Rust client at a lower quality than the canister profile (serde-json
parameter maps, a distinct envelope shape) and duplicated row/parameter-type logic that the
canister profile already owned.

## Related documents

- [ADR 0053](0053-prepared-query-codegen-and-client-runtime-boundary.md) — resolves decision 5
  (non-CDK Rust client ownership) to `gleaph-sdk`.
- `sdk/README.md` — updated to document `sdk/client/rust`.
- `design/implementation-gaps.md` — records the now-resolved client-SDK gap.
