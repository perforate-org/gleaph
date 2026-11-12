# General routing: Rust parity and canister cost benchmark (2026-11-12)

Status: measured, implementation evidence for ADR 0114 slices 1–2. The Router/SDK switch is not
implemented yet.

## What was measured

- Frozen Python model: `glev` commit `4fecedeaac0d7ea8bec0683f23d4257e53bfbecf`, model artifact
  `general-router-v1.json` (29 weights, exact IEEE-754 bits, checksum `eea74addf1a0bf53`).
- Rust port: `glev-general-router` with a 756-case parity runner over the frozen synthetic,
  real-paraphrase, independent holdout, movie, knowledge, wild-handwritten, and annotated demo-slot
  fixtures.
- Canister: additive `route_general` method in `gleaph-routing-canister` (wire types in
  `gleaph-routing-wire`, `GENERAL_ROUTE_VERSION = 1`), executed as a query.
- PocketIC: `cargo test -p gleaph-routing-canister --test general_route_bench -- --nocapture`
  with the canister built by
  `cargo build -p gleaph-routing-canister --release --target wasm32-unknown-unknown --features bench`.
  The benchmark feature exposes a test-only `bench_route_general` query that returns
  `performance_counter(0)` around the same implementation.

Environment: PocketIC server 15.0.0 at
`/Users/yota/.config/gleaph/launcher/icp-cli-network-launcher-arm64-darwin-v15.0.0-2026-08-13-03-55/pocket-ic`
(the repository launcher points at PocketIC 10 and is rejected by the pocket-ic 15 client).

## Rust/Python parity

`cargo run -p glev-general-router --example parity`:

- 756/756 cases, zero failures;
- operation/top-1 exact for every case;
- every probability within 1e-6 (observed differences are JSON/float-rounding scale; the runtime
  decodes exact weight bits, not JSON decimals);
- slot value, surface, character span, rule, missing and ambiguous sets exact;
- wild-handwritten OOS rule decision exact;
- expected suite metrics within 1e-9 (threshold comparisons use a 1e-12 decision epsilon at exact
  ties).

Rust quality table reproduced: synthetic top-1 100%, real-paraphrase real-test 86.1%, independent
holdout 100%, movie 80%, knowledge 90.9%, wild rule top-1 88.8% / OOS 96.2%, slot demo exact 90.9%
/ complete 95.5%, 50-operation clone scale top-1 99.0% / top-5 100%.

## Canister instruction grid

Wasm size (bench build): 3,999,808 bytes. Memory before/after the whole grid: 9,976,572 bytes (no
growth). The feature model does not need the BPE tokenizer; its deterministic regex tokenization is
part of the router. The tokenizer remains owned by the canister for the separate token-count/quota
path (ADR 0113).

| operations | tokens | instructions | max probability | operation | abstain |
| ---: | ---: | ---: | ---: | --- | --- |
| 6 | 32 | 149,114,553 | 0.997830 | similar-films | false |
| 6 | 96 | 155,098,320 | 0.997830 | similar-films | false |
| 6 | 256 | 170,230,996 | 0.997830 | similar-films | false |
| 10 | 32 | 154,429,643 | 0.994536 | similar-films | false |
| 10 | 96 | 163,949,033 | 0.994536 | similar-films | false |
| 10 | 256 | 187,644,765 | 0.994536 | similar-films | false |
| 20 | 32 | 167,930,819 | 0.986862 | similar-films | false |
| 20 | 96 | 186,008,622 | 0.986862 | similar-films | false |
| 20 | 256 | 230,903,003 | 0.986862 | similar-films | false |
| 50 | 32 | 208,323,755 | 0.965436 | similar-films | false |
| 50 | 96 | 252,036,181 | 0.965436 | similar-films | false |
| 50 | 256 | 361,402,806 | 0.965436 | similar-films | false |

Worst cell: 361.4M instructions, 2.42x the 6-card/32-token cell, well under the ADR 0114 target
(1B) and query/update caps (5B / 40B). The selected operation is `similar-films` for every cell
because the synthetic long question contains similarity wording; the benchmark measures cost, not
the quality table.

## Interpretation

- All-card feature scoring remains viable at 50 operations; the scale result is unchanged.
- The general route is a query-safe primitive at this size: no update call, no tokenizer asset, no
  neural pack.
- The cost is dominated by tokenization/feature extraction over the context, not by model size;
  adding cards is roughly linear and still cheap.
- The context/tokenizer memory was stable across the grid; the 64 MiB transient cap was not
  approached.

## Caveats

- The benchmark uses synthetic operations/questions and a generic movie-like catalog, not real user
  traffic. The quality table is the measured check; the instruction grid is a cost proxy.
- The Router still sends the context per request in this slice; context caching and the Router
  ingress/authorization path are not implemented. The 32 KiB/64-card caps are enforced in the
  canister.
- `GENERAL_ROUTE_VERSION` is additive; `WIRE_VERSION` remains 2 until the SDK/Router switch. That
  keeps the existing fixed-intent path compiling while the benchmark slice is validated.
- Entity linking, vector-byte parameters and graph-owner catalog authoring remain out of scope.
