# General routing: Router context builder and additive ingress (2026-11-12)

Status: measured, implementation evidence for ADR 0114 slice 3. The SDK/CLI switch and the
fixed-intent neural retirement are not implemented.

## Implemented

- `crates/router/src/general_routing.rs` is the single context builder. Per request it derives
  `GeneralRoutingContext` from:
  - the graph's parsed graph type (`parsed_graph_type_definition_for_graph_id`), reduced to labels,
    properties and edges;
  - prepared-operation records bound to that graph, with metadata and `OperationKind::Query` only;
  - the caller's `authorize_prepared_execute` result, so non-executable operations never enter the
    model's candidate set;
  - the temporary abstain threshold (`0.2`, one constant until the per-graph routing profile lands),
    policy version and a Candid-derived fingerprint.
- `route_general(graph_name, text)` on the Router:
  - requires graph access and `inference_enabled`;
  - bounds question text and context operation count;
  - counts tokens through the inference canister, then consumes the ADR 0112 quota;
  - forwards `GeneralRouteRequest` to the canister;
  - rejects an empty fingerprint, a non-abstaining empty operation, a fingerprint mismatch, and an
    operation outside the caller-visible catalog;
  - re-checks `authorize_prepared_execute` after the inter-canister call.
- `crates/pocket-ic-tests/tests/general_router_route.rs` covers a typed graph with one prepared
  operation, an inference canister and tokenizer, the successful operation+slot result, and the
  out-of-scope abstain path.

## Commands and evidence

```sh
cargo test -p gleaph-router general_routing
# 3 passed (schema conversion, metadata conversion, stable-state context filtering)

POCKET_IC_BIN=<pocket-ic 15> \
  cargo test -p gleaph-pocket-ic-tests --test general_router_route -- --nocapture
# 2 passed: typed-graph route returns find-films with a query slot; OOS text abstains
```

PocketIC used 15.0.0 at
`/Users/yota/.config/gleaph/launcher/icp-cli-network-launcher-arm64-darwin-v15.0.0-2026-08-13-03-55/pocket-ic`;
the repository launcher points at PocketIC 10, which the pocket-ic 15 client rejects.

## Behaviour confirmed by the E2E

- `CREATE OR REPLACE GRAPH <name> { ... }` binds the inline graph type to the registered test graph.
- Prepared metadata with a typed `query` text parameter reaches the model card.
- Router `route_general` returns `operation = Some("find-films")`, a `query` slot, and a 32-byte
  context fingerprint.
- `Delete the film document.` returns `abstain = true` with no operation.

## Caveats

- The abstain threshold is a temporary constant, not a per-graph calibrated profile; ADR 0114 item 7
  remains.
- General routing requires a typed graph and metadata-bearing prepared operations. Untyped graphs and
  metadata-less records fail closed with `NotImplemented` rather than inventing a context.
- The Router sends the context per request; there is no canister context cache yet.
- The neural `route_and_extract_text` path is still the SDK/CLI contract; this slice adds the Router
  endpoint only.
- No graph-owner catalog authoring (ADR 0115) and no entity linking.
