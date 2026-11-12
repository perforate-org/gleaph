# General routing: SDK/CLI text path switch (2026-11-12)

Status: measured, implementation evidence for ADR 0114 slice 4 (partial). The neural pack and its
upload/artefact wiring are still present; the SDK/CLI production text path no longer uses them.

## Implemented

- SDK:
  - public `GeneralRoute` (operation, probabilities, typed slots, incomplete/abstain, context
    fingerprint, model version) and `RoutingClient::route_general`;
  - `RoutingTransport::route_general` with a working `RouterTransport` implementation calling
    Router `route_general`; token-only transports fail closed.
- CLI `gleaph ask --graph`:
  - `--plan` and `--execute` call `route_general` instead of the fixed-intent neural text route;
  - no local tokenizer is loaded; `--tokenizer` was removed from the CLI surface;
  - typed slots map onto the existing prepared-execution binding surface (`start`/`anchor`,
    `depth`/`hops`, `from`/`to`, `topic`/`query`/`text`, `team`);
  - incomplete results abstain, and the routed operation is checked against the local catalog
    before `execution_plan`;
  - `--routing-log` records a `route: "general"` event with operation, flags, probabilities and
    typed slots.
- The old SDK neural text APIs (`route_text`, `TextExtraction`, byte-span binding) remain only for
  direct/operator and legacy tests; the CLI no longer calls them.

## Commands

```sh
cargo test -p gleaph-sdk --features routing general_route
# 1 passed: mock general transport exposes decision, int/text slots, fingerprint and model version

cargo test -p gleaph-cli general_bindings_map
# 1 passed: general slots map to SlotBindings; execution_plan yields citation-reach-depth-2
```

## Caveats

- The neural pack, pack-upload methods and fixed-intent methods are still compiled and installed in
  this slice. Removing them, the artifacts manifest and the operator install path is the next
  slice.
- The onboarding command for the general path still installs the tokenizer; Router `route_general`
  uses the token-count call for ADR 0112 quota, so the canister tokenizer remains required even
  though the feature model itself does not use it.
- `GeneralRoute` currently carries probabilities in context operation order but no operation ids;
  callers that need the full ranking must use the Router context/catalog. The CLI only needs the
  selected operation.
- No local CLI E2E against a live Router was added in this slice; SDK unit transport and Router
  PocketIC E2E cover the two ends.
