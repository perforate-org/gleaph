# General routing: per-graph calibration tool (2026-11-12)

Status: implemented. The calibration tool is dev-only; it recommends a profile and never writes it.

## Surface

- Router diagnostic (controller-only update):
  `admin_general_route_raw(graph_name, text)` builds the caller-visible context with the abstain
  threshold forced to `0.0` and returns the un-thresholded general route (operation, probabilities,
  incomplete/abstain flags, fingerprint). It does not consume quota and never executes a query.
- SDK: `RoutingTransport::route_general_raw` / `RoutingClient::route_general_raw` call that
  diagnostic; non-calibration transports fail closed by default.
- CLI, dev-only:

  ```sh
  gleaph ask --calibrate \
    --graph <NAME> --canister <ROUTER> --network local --identity <PEM> \
    --eval dev-questions.jsonl \
    --target-oos 0.95 --target-selective 0.85 \
    --policy-version 2 --calibration-out calibration.json [--json]
  ```

  Each dev item must be a normal golden-set row (`question`, `expected_op` or `expect_abstain`).
  The tool calls the raw route once per item, sweeps thresholds in basis points, and chooses the
  candidate that meets both targets with the highest coverage, then selective accuracy, then OOS
  abstention, then the lower threshold. If no candidate meets the targets it still prints the best
  candidate with `meets_targets: false`. Apply the recommendation with
  `admin_set_general_routing_profile`.

## Invariants

- The raw route is **controller-only** and does not bypass prepared-query authorization for
  execution; it only scores the caller-visible context.
- Calibration is dev-only: no automatic profile write, no test/holdout leakage.
- The stored profile threshold is not modified by the tool; the raw route's threshold override is
  per-request context data.
- OOS items rely on the strong rule (absent operation in the raw reply) or the swept probability
  threshold. Incomplete slots keep the operation and are not treated as a rule abstention.

## Verification

```sh
cargo test -p gleaph-sdk --features routing
# 23 routing/lib tests pass, including raw transport usage

cargo test -p gleaph-cli
# 201 lib tests + 13 binary tests; includes calibration_sweep_prefers_coverage_under_targets

POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-pocket-ic-tests --test general_router_route
# 3 passed; the movie test sets a 10000-bps profile, observes abstain, then calls the raw route and
# observes the un-thresholded operation/probability
```

## Caveats

- The tool calibrates from caller-visible dev questions; per-caller grants can change the context
  and therefore the recommended threshold.
- No confidence interval or small-sample warning yet; require enough OOS and in-domain items.
- The current policy is a single threshold; per-language/per-catalog-size profiles remain future
  work.
- Applying the profile is controller-governed until ADR 0115.
