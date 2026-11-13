# 0118. Public ask/interpret API

Date: 2026-11-13
Status: accepted (2026-11-13). Implemented for the Router natural-language surface: `interpret`,
`ask`, `debug_interpret`, `debug_interpret_batch`, their public result types, SDK methods, and the
configuration renames. The generic prepared-query ingress is still `prepared_query`; renaming that
cross-SDK surface to `execute` is a separate mechanical slice.
Last revised: 2026-11-13
Anchor timestamp: 2026-11-13 09:00:00 UTC +0900

## Context

ADR 0114 introduced the schema-conditioned route and ADR 0116 made it a query-only composite
query. The public method was named `route_general`, with `admin_general_route_raw*` for
calibration. Those names describe the implementation history:

- `general` only distinguished the new router from the retired fixed-intent pack;
- `route` is an internal verb, not what an application asks for;
- `admin_` encoded authorization in the method name;
- `raw` encoded a calibration mode;
- the response exposed probabilities, a context fingerprint and a model version that applications
  do not need.

`prepared_query` is already a composite query, so interpretation and read-only execution can be
composed in one free query call. The missing piece was a contract, not a canister feature.

## Decision

1. **`interpret(graph, text) -> Interpretation`** is the plan-only public query. It returns
   `status` (`Answered`, `NeedsInput`, `Unsupported`), the chosen operation, typed parameters and
   missing parameter names. It never executes.
2. **`ask(graph, text, read_mode) -> Answer`** interprets and, when the status is `Answered`,
   executes the selected read-only prepared query. It returns `AnswerRows` or a staged
   `AskFailure` (`Authorization`, `Catalog`, `Execution`). `NeedsInput` and `Unsupported` answers
   never execute.
3. **Execution is re-authorized.** `ask` calls the prepared-query path as the original caller; the
   interpretation is not authority. A catalog change between interpretation and execution fails
   closed.
4. **Internal values stay off the public contract.** Probabilities, threshold, context fingerprint
   and model version are available only through `debug_interpret` /
   `debug_interpret_batch`, which retain the controller/tenant authority predicate.
5. **Configuration is named for the product, not the implementation.**
   `ask_config`, `set_ask_config`, `parameter_values`, `set_parameter_values`, `enable_ask` and
   `disable_ask` replace the routing/inference/domain names. Deployment-level
   `admin_set_inference_canister` remains an explicitly controller-only deployment operation.
6. **One `ReadMode` contract.** `ask` takes the read freshness contract and passes it to the
   prepared query; composition must not weaken consistency.
7. **No compatibility aliases.** This is pre-production: the old public names are removed rather
   than retained beside the new ones.

## Non-goals

- Renaming the generic `prepared_query`/`prepared_mutate` data-plane ingress to `execute`. That
  surface is shared by the Rust/JS/canister SDKs and is a separate mechanical change.
- Renaming the inference canister's internal wire methods; applications do not see that contract.
- Exposing probabilities or a confidence score on `ask`; calibration uses the debug surface.

## Consequences

- Applications see three concepts: `ask`, `interpret`, `prepared_query`; they no longer need to
  distinguish `abstain` and `incomplete` booleans.
- `ask` is a composite query; its execution can fail after interpretation, so the `Answer` carries
  a failure stage instead of turning every execution error into an untyped reject.
- The Router performs one extra in-query authorization/catalog check on the `ask` path.
- `interpret` remains useful for clarification UIs and for cheap route-only evaluation.

## Evidence

- `crates/router-wire/src/ask.rs` owns the public types.
- `crates/router/src/api/inference.rs` implements `interpret` and `ask`; `interpret_impl` is the
  one interpretation owner used by both.
- SDK tests cover interpretation, answer rows and unsupported status; Router tests and the Candid
  export test pass.
