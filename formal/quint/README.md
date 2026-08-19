# Quint protocol pilot

This directory contains an executable, bounded protocol model for one Router,
one Graph shard, and one Property Index. It is an auxiliary specification of
the current ADRs and implementation contracts; it is not a replacement for
Rust tests, PocketIC, Lean, or the ADRs themselves.

## Scope

The model covers two finite mutation ids (`m0`, `m1`) and represents a Property
Index posting by the mutation id that justified it. It exercises:

- Graph canonical commit plus durable projection intent as one atomic boundary;
- asynchronous delivery, rejection, retry, duplicate delivery, and partial
  acknowledgement;
- restart and recovery from durable work;
- independent Router completion and Property Index convergence.

It deliberately excludes multiple shards, canister creation, migration
backfill, label statistics, retirement handshakes, vector bytes/ANN state,
stable-memory layout, Candid ABI, query planning, liveness/fairness, and
production recovery code.

## Traceability

| Quint concept | Contract source | Implementation/test anchor |
| --- | --- | --- |
| `canonical` is Graph-owned | `design/adr/0029-shard-local-atomicity-and-cross-canister-consistency.md` | Graph mutation + repair journal tests |
| `durableIntent` survives await/restart | ADR 0023, ADR 0024, ADR 0057 | `crates/graph/src/index/repair_journal.rs` |
| `projection` and `acknowledged` are derived progress | ADR 0023 | `crates/graph/src/index/inv_oracle.rs` |
| Router completion is separate from index convergence | ADR 0024, ADR 0057 | `crates/pocket-ic-tests/tests/router_gql_query.rs` |
| `restart` clears volatile delivery only | ADR 0029, ADR 0057 | Router recovery and repair-journal tests |
| `indexPendingMinMutationId` is a finite watermark analogue | ADR 0023 | repair-journal minimum tracked mutation test |

If an implementation or ADR changes, update this table and the model together.
Do not silently strengthen a contract in Quint.

## First pilot finding

The first draft allowed Graph to finalize `m1` while `m0` was still pending. A
deterministic scenario exposed that the model had captured acknowledgement but
not the contiguous-prefix removal rule. `graphFinalize` now blocks that path,
and the partial-acknowledgement test keeps the suffix durable until `m0` is
acknowledged. This is a model-fidelity finding, not evidence of a production
bug; the corresponding Rust repair-journal contract remains a required check.

## Reproducible commands

The pilot uses the installed CLI (`quint 0.32.0`) without adding a workspace
dependency or changing `pnpm-lock.yaml`:

```sh
quint --version
quint typecheck formal/quint/router_graph_property_projection.qnt
quint typecheck formal/quint/router_graph_property_projection_tests.qnt
QUINT_HOME=/private/tmp/gleaph-quint-home \
  quint test formal/quint/router_graph_property_projection_tests.qnt \
  --main=routerGraphPropertyProjectionTests \
  --match='^(healthyDelivery|rejectionThenRetry|duplicateDeliveryIsIdempotent|partialAcknowledgementPinsSuffix|restartPreservesDurableWork|routerMayCompleteBeforeProjection)$' \
  --seed=0x259
QUINT_HOME=/private/tmp/gleaph-quint-home quint run formal/quint/router_graph_property_projection.qnt \
  --main=routerGraphPropertyProjection --invariant=protocolSafety \
  --max-steps=20 --max-samples=10000 --seed=0x259 --backend=typescript
QUINT_HOME=/private/tmp/gleaph-quint-home quint verify formal/quint/router_graph_property_projection.qnt \
  --main=routerGraphPropertyProjection --invariant=protocolSafety \
  --max-steps=4 --backend=apalache --verbosity=0
```

`quint run` is randomized simulation and only reports the sampled executions.
`quint verify` is bounded model checking; a passing bound is not a proof of
unbounded safety or liveness. Record the CLI version, bounds, seed, elapsed
time, and any counterexample trace with each pilot result.

The focused Rust contracts that should be rerun when this model changes are:

```sh
cargo test -p gleaph-graph --lib index::repair_journal::tests::drain_retries_unacknowledged_suffix_after_partial_batch_progress -- --exact
cargo test -p gleaph-graph --lib index::repair_journal::tests::min_tracked_mutation_id_pins_lowest_unapplied_and_ignores_untracked -- --exact
cargo test -p gleaph-graph --lib index::inv_oracle::postings_converge_to_store_projection_after_failure_and_compaction -- --exact
cargo test -p gleaph-router --lib facade::stable::label_stats::tests::lifecycle_phase_never_completes_with_outstanding_work -- --exact
cargo test -p gleaph-pocket-ic-tests --test router_gql_query single_shard_mutation_token_barrier_status_lifecycle -- --exact
```

No CI gate is added by this pilot. Adoption requires at least one new defect,
ambiguity, or missing regression test, bounded verification that remains within
the local validation budget, and an owner who will keep the model synchronized.

## Current validation record

On 2026-08-20 (UTC), the six deterministic Quint scenarios passed in 0.34 s,
the TypeScript simulation passed with 10,000 samples at depth 20 in 18.18 s,
and the Apalache smoke check passed at depth 4 in 5.33 s. The first model draft
also exposed and corrected the non-prefix finalization omission described above.

The five focused Rust comparisons were run once: four passed. The PocketIC
test `single_shard_mutation_token_barrier_status_lifecycle` failed before any
Quint integration with `Err(NotFound("no-such-key"))` where the test expected
`InvalidArgument`. Treat this as an existing live-contract/test mismatch, not
as evidence that the Quint model is invalid or that production behavior is
correct; it must be triaged separately before using the test as an adoption
gate.
