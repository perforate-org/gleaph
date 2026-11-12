# 0111. Router-bound inference canister and per-graph routing enablement

Date: 2026-11-12
Status: proposed (design agreed 2026-11-12; implementation not started)
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 09:30:00 UTC +0900

## Context

The natural-language routing stack is currently client-owned: the SDK tokenises a question,
calls the inference canister's `route_and_extract` directly, applies the abstain policy and binds
slot values, and only then calls the Router with a prepared query. Every application therefore
needs the inference canister principal and must implement the routing policy itself.

The Router already owns the external API, graph tenancy, authorization and orchestration, and the
Provision canister already creates and installs graph shards, property-index, vector and text
canisters from an active release. Routing is a property of a graph: only a graph whose catalog has
prepared routing operations and whose deployment wants model-based dispatch should accept a
natural-language request.

## Problem

- Inference association is per client, not per graph or deployment, so a graph cannot turn model
  routing on or off as a product capability.
- The client talks to a model canister it does not own, and each client duplicates the routing
  policy and slot contract.
- Provisioning an inference canister is outside the existing deployment flow.

## Existing architecture assessment

The existing concepts absorb the change:

- **Router** already owns external ingress, graph access checks
  (`RouterStore::caller_may_access_graph`), provisioning orchestration and stable runtime config.
- **Provision flow** already creates a canister and installs a release artifact per
  `LogicalResource`; the Router is the governance controller of the canisters it provisions.
- **Graph runtime state** already stores per-graph flags (element-id key, shard ids); a per-graph
  enablement set is the same shape.
- **SDK** already abstracts the routing transport behind `RoutingTransport`; a Router transport
  replaces the direct-canister transport without changing `RoutingClient`.

No new subsystem is introduced. The alternatives below differ only in ownership and plumbing.

## Alternatives

- **A. Keep client-direct routing.** Rejected: no graph-level policy, every client duplicates
  the inference principal and routing policy, and the Router cannot audit or meter model use.
- **B. Router proxy with an externally deployed inference canister.** The Router stores a
  principal set by a controller. Smaller change, but inference canisters remain outside the
  release/provisioning flow and deployments drift between manual and provisioned environments.
- **C. Router proxy, inference canister provisioned by the Provision canister (chosen).**
  The Router provisions one inference canister through the existing flow and becomes its
  governance controller. One release manifest pins the inference wasm with the other canisters.
- **D. One inference canister per graph.** Rejected: the pack is 9 MB and the loaded model holds
  ~328 MB of heap; per-graph duplication multiplies fixed cost with no isolation benefit. The
  model is deployment-wide.
- **E. Router accepts question text and the inference canister tokenises.** Deferred. The
  tokenizer is a 33 MB asset and BPE work would run on-chain. Token IDs stay the wire input until
  the tokenizer cost and ownership are decided separately.

## Decision

1. **One inference canister per Router deployment.** Provision it lazily through the existing
   Provision flow with a singleton logical resource
   `LogicalResource::Inference` (no payload id), so the intent key `(deployment_id, Inference)`
   makes provisioning replay-safe. Add `CanisterKind::Inference` and a
   `ReleaseManifest::inference_artifact`; releases stay atomic compatible sets (fresh-install
   policy, no mixed release).
2. **Router persists the binding.** A stable singleton holds the inference canister principal and
   the wire version observed at binding time. On binding and on each first use, the Router checks
   `wire_version()` and `weights_installed()`; a mismatch fails closed.
3. **Per-graph enablement is explicit Router state.** A stable map/set keyed by `GraphId` records
   whether model routing is enabled. The enable/disable admin methods are controller/governance
   gated in the first slice; graph-owner delegation is a later widening.
4. **The Router proxies routing calls.** New ingress
   `route_and_extract(graph_name, token_ids)` resolves the graph, checks enablement and
   `caller_may_access_graph`, checks the bound canister and weights, then forwards to the
   inference canister's `route_and_extract` and returns the wire result. `route_options` is
   proxied the same way. The inference canister stays directly callable for tests and for
   operators; the SDK no longer needs its principal.
5. **Token IDs remain the Router input for now.** The SDK keeps tokenising locally and applies
   the abstain policy and slot binding, unchanged. Moving the tokenizer onto the inference
   canister (`route_and_extract_text`) is a separate decision with its own size, instruction and
   release-artifact analysis.
6. **Pack installation is a Router admin step.** The Provision flow installs the inference wasm;
   the Router, as governance controller, forwards pack chunks (`admin_install_chunk`,
   1.9 MB per ingress) and `admin_install_finish` to the inference canister. Pack bytes are not
   stored in the Router beyond the forwarding call. A later release could carry the pack as an
   artifact through the Provision canister; that is not the first slice.
7. **SDK and CLI target the Router.** A new `RouterTransport` implements
   `RoutingTransport` with `(agent, router_principal, graph_name)`. `gleaph ask` takes
   `--graph` instead of `--inference-canister`; the direct `CanisterTransport` remains available
   for tests and for talking to a known inference canister.

## Consequences

- Natural-language routing becomes a graph capability with one deployment-wide model and one
  Router-owned binding; clients configure a graph name, not a model canister.
- The Provision release manifest and `CanisterKind` gain an inference variant; this is a
  pre-production fresh-install change, not a compatibility shim.
- Router ingress pays the inference update call; the call is authorized but not yet rate-limited
  or metered per caller.
- The Router owns two new stable regions (binding + enablement set) and their MemoryIds must be
  entered in `design/storage/stable-memory-inventory.md`.

## Trade-offs

- Router cycles fund model inference for every enabled graph; quotas/rate limits are deferred and
  must be designed before any untrusted multi-tenant deployment.
- Pack installation is outside the Provision release for now: an operator must upload the pack
  through the Router after provisioning. This keeps the Provision artifact model wasm-only.
- The graph enablement flag is separate from prepared-query registration; enabling a graph with
  no routing prepared queries still authorizes model calls and must be treated as an operator
  choice.

## Migration

Pre-production fresh install: change `LogicalResource` (new variant tag), `CanisterKind`,
`ReleaseManifest`, and the Router init/bootstrap surface together; do not add legacy decoders or
mixed-release compatibility. Existing deployments must be reinstalled with the new release.

## Design Documentation Impact

- `design/storage/stable-memory-inventory.md`: new Router regions (binding, graph enablement).
- Provisioning contracts (ADR 0035/0036): new logical resource, canister kind and release field.
- `design/index/text-index.md` or a routing contract page: record the Router-owned routing ingress
  and per-graph enablement.
- SDK/CLI documentation: Router transport and `--graph`.

## Review axes

- **Encapsulation**: the Router owns the association, policy and proxy; the inference canister
  keeps owning model execution; clients stop reaching into model infrastructure.
- **Separation of concerns**: provisioning (Provision), orchestration/authorization (Router),
  model compute (inference canister), tokenisation (client) stay distinct.
- **Invariants**: one binding per Router, one inference artifact per release, enablement checked
  before every proxied call, wire version checked at binding.
- **Consistency**: the binding and enablement have one writer each; the inference canister
  principal never diverges into per-graph copies.
- **Fitness for purpose**: matches a deployment-wide model and a graph-level product capability
  without a new subsystem.

## Implementation slices

1. Provision contract: `LogicalResource::Inference`, `CanisterKind::Inference`,
   `ReleaseManifest::inference_artifact`, install mapping and contract tests.
2. Router stable state: inference binding, per-graph enablement, admin enable/disable and lazy
   provisioning; unit tests for replay, disabled graphs and version checks.
3. Router ingress: `route_and_extract`/`route_options` proxy, graph access check, pack-chunk
   forwarding, and a PocketIC end-to-end with a provisioned inference canister.
4. SDK and CLI: `RouterTransport`, `gleaph ask --graph`, docs; keep `CanisterTransport` for tests.
5. Deferred: tokenizer ownership (`route_and_extract_text`), end-to-end `ask` mapping in the SDK,
   per-caller quotas/rate limits.

## Open questions

- Which privilege gates the routing ingress in the first slice: graph tenancy only, or a
  prepared-query `EXECUTE` requirement? Tenancy-only is the minimum viable gate; a dedicated
  routing privilege may be cleaner.
- When should lazy provisioning run: on the first `enable_graph_inference`, or at Router init when
  the deployment requests inference? Enabling on demand avoids an inference canister for
  deployments that never use routing.
