# General routing: fixed-intent neural path and pack retirement (2026-11-12)

Status: implemented and verified. This completes ADR 0114's retirement slice.

## Removed

- Inference canister: INT8 pack chunk/finish methods, `Model`/`Pack` state, `infer`,
  `route_and_extract(token_ids)`, `route_and_extract_text`, `route_options`, `weights_installed`,
  decision cache/cache stats/clear, BIO span grouping, `glev-infer` and kernel dependencies.
  The canister is now tokenizer + general route only.
- Wire (`WIRE_VERSION = 3`): Candid neural reply/span types removed; tokenizer fingerprint,
  token counts, general context/request/result remain.
- Router: pack upload/finish, weights-installed verification and the neural proxy methods removed;
  tokenizer install/count, enable/disable, profile and `route_general` remain.
- Operator: `gleaph-inference-install` no longer accepts/uploads a pack; it uploads/verifies the
  tokenizer and optionally enables the graph.
- SDK: tokenizer client, token-id/text neural APIs, BIO span binding and the neural wire mirror
  removed. `RoutingClient::route_general` + slot vocabulary/binding remain.
- Manifest/tests/docs: pack asset removed from `artifacts.toml` and the fetch script; neural
  canister/proxy tests removed or reduced to provisioning; ADR 0111/0113 amended.

## Verification

```sh
cargo test -p gleaph-routing-canister --lib
# 3 passed: general route select/extract, OOS + fingerprint mismatch, wire version 3

cargo test -p gleaph-routing-canister --test artifacts_manifest
# 1 passed: one tokenizer asset, glev-general-router revision, wire version 3

cargo test -p gleaph-sdk --features routing
# 30 passed (routing module is general-only; 2 new general route tests)

cargo test -p gleaph-cli
# 13 passed, including the routing-log general record and slot/execution-plan tests

cargo build -p gleaph-routing-canister --release --target wasm32-unknown-unknown --features bench
POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-routing-canister --test general_route_bench -- --nocapture
# wasm 3,770,939 bytes (was 3,999,808); worst cell 361.2M instructions, memory unchanged;
# ratio 2.43x

POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-pocket-ic-tests --test general_router_route
# 3 passed after the pack/wire removal: movie, knowledge and OOS abstain

POCKET_IC_BIN=... cargo test -p gleaph-pocket-ic-tests --test inference_provisioning_flow --no-run
# compiles after removing the pack install/route assertions
```

Scoped clippy (`--no-deps -D warnings`) passes for the routing wire and canister, and for the SDK/CLI
paths changed in this slice; workspace-wide clippy remains blocked by unrelated dirty paths.

## Notes

- Tokenizer remains required: `route_general` performs the exact token-count quota call (ADR 0112)
  and the canister reloads it after upgrade. No local client tokenizer exists.
- `glev-general-router` is still pinned by git revision and its artifact is embedded in the crate;
  removing the neural pack removed the large uploaded asset, not the small general artifact.
- Fresh-install semantics: no compatibility shims or legacy decoders were added; old canister/pack
  state is intentionally unreadable.
- Old dated investigations that measured the neural pack are historical evidence, not current
  contract references.
