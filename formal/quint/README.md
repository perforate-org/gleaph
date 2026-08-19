# Quint protocol pilot

Status: **Experimental, non-blocking**. This directory contains a bounded
auxiliary specification for one Router, one Graph shard, and one Property
Index. ADRs and production tests remain normative; this model does not replace
Rust, PocketIC, Lean, or ADR validation.

## Current model

The model has two finite mutation ids (`m0`, `m1`). Each Graph commit appends
exactly two postings to the Graph-owned FIFO `durableQueue`. Delivery, Property
Index prefix application, response observation, and Graph acknowledgement are
separate transitions. Applied and rejected response loss, restart, and
recovery replay the durable queue; Property Index application is set-like and
idempotent.

Router completion is independent of Property Index convergence. The model
intentionally does not claim that its FIFO is the live
`index_pending_min_mutation_id` watermark: `oldestDurableMutationRank` is a
conceptual rank derived from the queue. In production, the first-delivery
outbox and the failed-flush repair journal are distinct durable mechanisms; the
model abstracts durable work without conflating those owners.

The model omits an idle-pending restart because it is an unobservable safety
stutter. Maintenance re-arm behavior and fairness/liveness are also excluded.
The earlier model-fidelity defects—non-prefix finalization and collapsed
posting/transport phases—are corrected in the current model.

## Traceability

The following tables are exhaustive for the model's declared `var`, `action`,
and safety-value symbols. An entry marked **model-only** is an intentional
abstraction or test guard; it has no production transition with the same
name and is not presented as live behavior.

### State variables

| Quint symbol | Exact contract/document anchor | Exact implementation/test anchor |
| --- | --- | --- |
| `requested` | ADR 0029 §2: Router owns orchestration and client-key idempotency | `crates/router/src/facade/stable/label_stats.rs::RouterMutationRecord`; `crates/router/src/gql.rs::attach_mutation_token`; `crates/pocket-ic-tests/tests/router_gql_query.rs::single_shard_mutation_token_barrier_status_lifecycle` |
| `routerCompleted` | ADR 0024, “Consistency vocabulary (ADR 0029)”; ADR 0029 §2 Phase 4 | `crates/router/src/gql.rs::recover_mutation_outcome` and `::recover_mutation_record`; `crates/graph-kernel/src/plan_exec.rs::MutationJournalState::Completed`; tests `router_recovery_timer_converges_projection_pending_saga_autonomously` and `single_shard_mutation_token_barrier_status_lifecycle` |
| `canonical` | ADR 0029 §1: Graph is the sole canonical owner | `crates/graph/src/gql_run.rs::apply_canonical_mutation_segment` and `::run_wire_plans_inner`; `crates/graph/src/facade/store/label_stats_delta.rs::commit_record_incomplete_mutation_journal`; `crates/graph/src/index/inv_oracle.rs::postings_converge_to_store_projection_after_failure_and_compaction` |
| `durableQueue` | ADR 0023 D5 and ADR 0029 §2: durable ordered projection intent; model deliberately abstracts two queues | `crates/graph/src/facade/store/derived_index_outbox.rs::GraphStore::{derived_index_outbox_append,derived_index_outbox_peek,derived_index_outbox_remove}` plus `crates/graph/src/facade/store/repair_journal.rs::GraphStore::{repair_journal_append,repair_journal_peek,repair_journal_remove}`; `crates/graph/src/index/repair_journal.rs::drain_once`; tests `drain_retries_unacknowledged_suffix_after_partial_batch_progress` and `postings_converge_to_store_projection_after_failure_and_compaction` |
| `projection` | ADR 0029 §2: Property Index is derived state, not the canonical owner | `crates/graph-index/src/canister.rs::posting_batch`; `crates/graph-kernel/src/index.rs::IndexPostingBatchProgress`; tests `graph_index_batch_posting_survives_index_upgrade` and `postings_converge_to_store_projection_after_failure_and_compaction` |
| `delivery` | ADR 0023 D5: ordered delivery may be repeated | `crates/graph/src/index/lookup.rs::dispatch_posting_batch`; `crates/graph/src/index/repair_journal.rs::drain_once`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `response` | ADR 0023 D5: index progress is acknowledged only after the bounded apply result | `crates/graph-index/src/canister.rs::posting_batch` returns `IndexPostingBatchProgress`; `crates/graph/src/index/repair_journal.rs::drain_once`; test `graph_index_batch_posting_survives_index_upgrade` |
| `restartPending` | ADR 0023 upgrade/compaction contract and ADR 0029 §2 durable recovery | `crates/graph/src/facade/maintenance_timer.rs::run_maintenance_pass`; `crates/pocket-ic-tests/tests/canister_upgrade_persistence.rs::graph_index_batch_posting_survives_index_upgrade`; no direct Router/Graph transport-state restart test |
| `lastAction` | **Model-only** witness instrumentation; no production state owner | The 12 Quint witness values below; no Rust analogue by design |

### Actions

| Quint action | Exact contract/document anchor | Exact implementation/test anchor |
| --- | --- | --- |
| `init` | **Model-only** initial state; ADR 0029 §1–2 supplies the ownership baseline | PocketIC fixture `crates/pocket-ic-tests/src/lib.rs::install_single_shard_federation`; no live action equivalent |
| `disabled` | **Model-only** statically disabled match branch | No live symbol or test; transition cannot be selected |
| `routerDispatch` | ADR 0029 §2: Router dispatches to Graph shards | `crates/router/src/gql.rs::gql_execute_idempotent_with_batch` and `::dispatch_plan_blob`; test `single_shard_mutation_token_barrier_status_lifecycle` |
| `graphCommit` | ADR 0029 §1: canonical write plus durable intent in one Graph message segment | `crates/graph/src/gql_run.rs::apply_canonical_mutation_segment`; `crates/graph/src/gql_run.rs::run_wire_plans_inner`; test `postings_converge_to_store_projection_after_failure_and_compaction` |
| `routerComplete` | ADR 0024 consistency vocabulary: Router saga completion is independent of index convergence | `crates/router/src/gql.rs::recover_mutation_outcome` and `::recover_mutation_record`; tests `router_recovery_timer_converges_projection_pending_saga_autonomously` and `lifecycle_phase_never_completes_with_outstanding_work` |
| `deliver` | ADR 0023 D5: durable work is sent to graph-index asynchronously | `crates/graph/src/index/lookup.rs::dispatch_posting_batch`; `crates/graph/src/index/repair_journal.rs::drain_once`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `recover` | ADR 0029 §2 Phase 4: recovery is projection-only and does not redispatch canonical DML | `crates/router/src/gql.rs::recover_mutation_record`; `crates/graph/src/facade/maintenance_timer.rs::run_maintenance_pass`; test `router_recovery_timer_converges_projection_pending_saga_autonomously` |
| `indexApplyPrefix` | ADR 0023 D5 and ADR 0060: bounded posting progress is explicit | `crates/graph-index/src/canister.rs::posting_batch`; `IndexPostingBatchProgress`; tests `graph_index_batch_posting_survives_index_upgrade` and `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `indexReject` | ADR 0023 D5: failed flush remains durable for repair | `crates/graph/src/index/repair_journal.rs::drain_once` error path; test `drain_stops_at_failure_and_retains_remaining` |
| `loseAppliedResponse` | **Model-only** transport fault after a Property Index write; ADR 0029 §2 only requires repeat delivery to be safe | No direct Rust/PocketIC transport-loss test; `postings_converge_to_store_projection_after_failure_and_compaction` covers idempotent re-drain, not response loss |
| `loseRejectedResponse` | **Model-only** transport fault after rejection; ADR 0029 §2 retry contract | No direct Rust/PocketIC transport-loss test; `drain_stops_at_failure_and_retains_remaining` covers durable retry after an error |
| `graphObserveRejected` | ADR 0023 D5: failed work is retained until a later drain succeeds | `crates/graph/src/index/repair_journal.rs::drain_once`; tests `drain_stops_at_failure_and_retains_remaining` and `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `graphAcknowledge` | ADR 0023 D5: remove only the acknowledged prefix | `crates/graph/src/index/repair_journal.rs::drain_once`; `GraphStore::derived_index_outbox_remove` and `::repair_journal_remove`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `restart` | ADR 0023 upgrade durability; ADR 0029 §2 retry/recovery | `crates/pocket-ic-tests/tests/canister_upgrade_persistence.rs::graph_index_batch_posting_survives_index_upgrade`; no direct pending-transport restart analogue |
| `step` | **Model-only** nondeterministic action union | No live symbol; each constituent action is mapped above |

### Derived values and safety invariants

| Quint symbol | Exact contract/document anchor | Exact implementation/test anchor |
| --- | --- | --- |
| `canonicalOps` | ADR 0029 §1–2 derived postings are justified by Graph canonical state | `canonicalPostings(canonical)` is model-only; live owner `apply_canonical_mutation_segment`; test `postings_converge_to_store_projection_after_failure_and_compaction` |
| `queuedOps` | ADR 0023 D5 durable repair/outbox work | `asSet(durableQueue)` is model-only; live owners `GraphStore::derived_index_outbox_*` and `::repair_journal_*`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `orderedProjectionPendingSuffix` | ADR 0023 D5 ordered, acknowledged-prefix drain | `crates/graph/src/index/repair_journal.rs::drain_once`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `deliverySafe` | ADR 0023 D5 delivery is a queue prefix | `crates/graph/src/index/lookup.rs::dispatch_posting_batch`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `responseCausalityAndExclusivity` | ADR 0023 D5 progress is downstream of delivery and apply | `crates/graph-index/src/canister.rs::posting_batch` and `IndexPostingBatchProgress`; test `graph_index_batch_posting_survives_index_upgrade` |
| `sameMutationProjectionPrefix` | ADR 0023 D5 ordered posting progress | `crates/graph-index/src/canister.rs::posting_batch`; `crates/graph-kernel/src/index.rs::IndexPostingBatchProgress`; tests `drain_retries_unacknowledged_suffix_after_partial_batch_progress` and `posting_batch_wire_roundtrip_preserves_order_and_progress` |
| `ownershipAndJustification` | ADR 0029 §§1–2 canonical ownership and derived-state justification | `apply_canonical_mutation_segment`; test `postings_converge_to_store_projection_after_failure_and_compaction` |
| `noLostPendingWork` | ADR 0023 D5 and ADR 0029 §2 durable idempotent propagation | `GraphStore::repair_journal_append` plus `drain_once`; tests `drain_stops_at_failure_and_retains_remaining` and `postings_converge_to_store_projection_after_failure_and_compaction` |
| `responseCausality` | ADR 0023 D5: rejection does not acknowledge; applied response follows index write | `drain_once` and `posting_batch`; test `drain_stops_at_failure_and_retains_remaining` |
| `exactAckPrefix` | ADR 0023 D5: remove only the acknowledged prefix, retain suffix | `drain_once`; test `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `routerCompletionIndependence` | ADR 0024 “Consistency vocabulary (ADR 0029)” | `recover_mutation_record`; tests `router_recovery_timer_converges_projection_pending_saga_autonomously` and `single_shard_mutation_token_barrier_status_lifecycle` |
| `oldestDurableMutationRank` | **Model-only** queue-derived rank | Explicitly not `crates/graph/src/facade/store/repair_journal.rs::GraphStore::index_pending_min_mutation_id`; that live watermark is covered by `min_tracked_mutation_id_pins_lowest_unapplied_and_ignores_untracked` |
| `conceptualOldestDurableMutationRank` | **Model-only** bounded rank guard; no claim about the live watermark | Same deliberate gap and exact watermark test as above |
| `protocolSafety` | ADRs 0023, 0024, and 0029 combined; aggregate only | Composition of the invariant rows above; no single live invariant or single Rust test |

### Deterministic scenarios

| Quint scenario | Exact live anchor and coverage status |
| --- | --- |
| `healthyDelivery` | ADR 0023 D5; `crates/graph/src/index/inv_oracle.rs::postings_converge_to_store_projection_after_failure_and_compaction` (healthy drain and final projection equality) |
| `rejectionObservedThenRetry` | ADR 0023 D5; `crates/graph/src/index/repair_journal.rs::drain_stops_at_failure_and_retains_remaining` and `::drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `sameMutationPartialPrefixDrain` | ADR 0023 D5; `crates/graph/src/index/repair_journal.rs::drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `appliedResponseLossDuplicateReplay` | ADR 0029 §2 repeated delivery; `postings_converge_to_store_projection_after_failure_and_compaction` verifies idempotent re-drain; response-loss transport itself is a deliberate coverage gap |
| `restartDuringRequestThenRetry` | ADR 0023 upgrade durability; `crates/pocket-ic-tests/tests/canister_upgrade_persistence.rs::graph_index_batch_posting_survives_index_upgrade`; pending-transport restart is not directly exercised |
| `restartAfterIndexApplyBeforeGraphObserveThenReplay` | ADR 0029 §2 retry/idempotency; same graph-index upgrade test; response-loss plus upgrade ordering is a deliberate coverage gap |
| `routerCompleteWithPendingProjection` | ADR 0024 consistency vocabulary; `crates/pocket-ic-tests/tests/router_gql_query.rs::router_recovery_timer_converges_projection_pending_saga_autonomously` injects `ProjectionPending` then observes completion |
| `applyWithoutDeliveryFails` | Model-only negative transition guard for delivery causality; no direct live test with this action shape |
| `rejectWithoutDeliveryFails` | Model-only negative transition guard; no direct live test with this action shape |
| `acknowledgeWithoutAppliedResponseFails` | Model-only negative transition guard for acknowledgement causality; live prefix behavior is covered by `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |
| `observeRejectionWithoutRejectedResponseFails` | Model-only negative transition guard; no direct live test with this action shape |
| `overAcknowledgeFails` | Model-only negative transition guard for exact-prefix accounting; live suffix retention is covered by `drain_retries_unacknowledged_suffix_after_partial_batch_progress` |

If an implementation or ADR changes, update this table and the model together.
Do not silently strengthen a contract in Quint.

## Deterministic coverage

The test module contains exactly 12 anchored tests:

```text
healthyDelivery
rejectionObservedThenRetry
sameMutationPartialPrefixDrain
appliedResponseLossDuplicateReplay
restartDuringRequestThenRetry
restartAfterIndexApplyBeforeGraphObserveThenReplay
routerCompleteWithPendingProjection
applyWithoutDeliveryFails
rejectWithoutDeliveryFails
acknowledgeWithoutAppliedResponseFails
observeRejectionWithoutRejectedResponseFails
overAcknowledgeFails
```

The model exposes 12 action witnesses for dispatch, commit, completion,
delivery, recovery, apply, rejection, both response-loss paths, restart,
rejection observation, and acknowledgement.

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
  --match='^(healthyDelivery|rejectionObservedThenRetry|sameMutationPartialPrefixDrain|appliedResponseLossDuplicateReplay|restartDuringRequestThenRetry|restartAfterIndexApplyBeforeGraphObserveThenReplay|routerCompleteWithPendingProjection|applyWithoutDeliveryFails|rejectWithoutDeliveryFails|acknowledgeWithoutAppliedResponseFails|observeRejectionWithoutRejectedResponseFails|overAcknowledgeFails)$' \
  --seed=0x259
```

The command above is the exact anchored 12-test witness command. The bounded
simulation command is:

```sh
QUINT_HOME=/private/tmp/gleaph-quint-home quint run \
  formal/quint/router_graph_property_projection.qnt \
  --main=routerGraphPropertyProjection --invariant=protocolSafety \
  --max-steps=20 --max-samples=10000 --seed=0x259 --backend=typescript \
  --witnesses routerDispatchWitness graphCommitWitness routerCompletionWitness \
  requestDeliveryWitness recoveryWitness indexApplyWitness indexRejectWitness \
  appliedResponseLossWitness rejectedResponseLossWitness restartWitness \
  graphRejectionObservationWitness graphAcknowledgementWitness
```

Post-repair validation is recorded at the OS UTC anchor `2026-08-19 21:19:37
UTC +0000`: the model typecheck passed in `0.88s`, the test-module typecheck
passed in `0.75s`, the exact anchored 12-test command passed in `1.07s`, and
the TypeScript simulation passed all 10,000 samples at depth 20 with seed
`0x259` in `38.37s`. The simulation passed `protocolSafety` and all 12
witnesses. Witness counts were:

| Witness | Count |
| --- | ---: |
| `routerDispatchWitness` | 10000 |
| `graphCommitWitness` | 10000 |
| `routerCompletionWitness` | 9967 |
| `requestDeliveryWitness` | 10000 |
| `recoveryWitness` | 9797 |
| `indexApplyWitness` | 8176 |
| `indexRejectWitness` | 9211 |
| `appliedResponseLossWitness` | 4631 |
| `rejectedResponseLossWitness` | 4816 |
| `restartWitness` | 9859 |
| `graphRejectionObservationWitness` | 4831 |
| `graphAcknowledgementWitness` | 1090 |

`quint run` is randomized sampled evidence. `quint verify` is bounded model
checking, not an unbounded safety or liveness proof. It was not rerun after
the current model repair; any earlier result applies only to the superseded
model and is not current evidence.

The focused Rust comparisons to rerun when this model changes are:

```sh
cargo test -p gleaph-graph --lib index::repair_journal::tests::drain_retries_unacknowledged_suffix_after_partial_batch_progress -- --exact
cargo test -p gleaph-graph --lib index::repair_journal::tests::min_tracked_mutation_id_pins_lowest_unapplied_and_ignores_untracked -- --exact
cargo test -p gleaph-graph --lib index::inv_oracle::postings_converge_to_store_projection_after_failure_and_compaction -- --exact
cargo test -p gleaph-router --lib facade::stable::label_stats::tests::lifecycle_phase_never_completes_with_outstanding_work -- --exact
cargo test -p gleaph-pocket-ic-tests --test router_gql_query single_shard_mutation_token_barrier_status_lifecycle -- --exact
```

Current-repair status (OS UTC anchor `2026-08-19 21:39:57 UTC +0000`): these
four Rust unit checks and one PocketIC check were **not run** during the
current model repair. The commands above are reproducibility instructions,
not pass evidence; the adoption gate remains incomplete for this comparison
surface, and no Rust/PocketIC pass is claimed here.

## Adoption decision

Decision: **`revise`**. The pilot remains experimental and non-blocking. The
corrected model has a useful executable boundary, and its post-repair
typecheck, exact anchored tests, and bounded simulation pass. The decision
remains `revise` because a current `quint verify` result is not available, the
smaller Router `atomic_insert` slice remains, and a later investigation is
needed to determine whether the `AtLeast` barrier's repair-only watermark is
sufficient while first-delivery outbox work is pending. Do not add a CI gate or
broaden the protocol model until those prerequisites are complete.

Validation evidence anchor from the OS: `2026-08-19 21:19:37 UTC +0000`.
