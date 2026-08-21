# Quint protocol pilot

Status: **Experimental, non-blocking**. This directory contains bounded
auxiliary specifications for the Router–Graph–Property Index projection and
the Router–Graph `CanonicalPending` exact-replay boundary, plus a bounded
Router–Graph–Vector subject-clock GC comparison. ADRs and production tests
remain normative; these models do not replace Rust, PocketIC, Lean, or ADR
validation.

## Current production gate snapshot

The targeted direct-ingestion PocketIC lifecycle gate passed independently of the Quint pilot:

```sh
cargo test -p gleaph-pocket-ic-tests --test adr0031_vertex_embedding_ingestion unavailable_vector_owner_rebinds_graph_and_router_direct_ingestion_outboxes -- --nocapture
```

Result: **1 passed, 0 failed, 4 filtered, 17.95s**. The test manually drives the Graph and Router
timer/recovery seams and covers Router upgrade, Vector reopen/rebind, exact GQL search, and idempotent
replay. It does not prove autonomous wall-clock timer firing or deferred watermark/tombstone GC
completion. `quint verify` remains unrun; this gate does not make the Quint pilot complete or normative.

## Current models

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
outbox and failed-flush repair journal remain distinct durable owners, while
Graph now maintains one exact fixed-index ordinary floor over both without exposing either owner
to Router. This slice changes no Quint state, action, derived value,
invariant, or test semantics.

The model omits an idle-pending restart because it is an unobservable safety
stutter. Maintenance re-arm behavior and fairness/liveness are also excluded.
The earlier model-fidelity defects—non-prefix finalization and collapsed
posting/transport phases—are corrected in the current model.

The separate `router_vector_tombstone_gc.qnt` model compares the production
`Frozen` Router watermark, an intentionally unsafe `MaxAcknowledged` policy,
and a candidate `Contiguous` frontier. The refined candidate models durable
pre-Graph-await intent, Graph rejection, response loss, exact Vector-prefix
acknowledgement, sparse allocation, restart, frontier-publication loss, and two
independent target/shard lanes. The required pre-Graph phase and frontier
publication path do not exist in production and remain a design candidate.

## Candidate and traceability index

Review anchor: 2026-08-21 22:01:28 UTC +0000.

ADRs and active design documents own intended behavior; production Rust and
PocketIC tests own executable evidence; this README owns formal-candidate
classification and traceability; and the implementation-gap ledger owns only
verified production defects or missing capabilities. A plan owns one proposed
slice and must not duplicate those sources of truth.

| Classification | Meaning and required next evidence |
| --- | --- |
| **Adopted / experimental** | The bounded artifact is accepted for its declared non-blocking scope; it remains non-normative and supplies neither a CI gate nor a proof. |
| **Modeled / experimental** | A bounded Quint artifact exists; its bounds, unrun checks, and abstractions remain part of its result. |
| **Quint candidate** | Multi-owner state, fault ordering, or replay merits a finite state-machine review after the production contract is settled. |
| **Rust-test-first** | A focused owner-local Rust or PocketIC regression can discriminate the behavior before a model adds useful evidence. |
| **Design/implementation prerequisite** | A required invariant or durable owner is absent or mismatched; resolve it and add a regression before treating a model as adoption evidence. |
| **Intentional / current** | Existing design deliberately chooses the current behavior; it is not a new formal or ledger item. |
| **Covered** | Direct tests already protect the behavior; it is not a new formal or ledger item. |
| **Planned / Deferred** | A future capability remains deliberately outside the current production and formal scope. |
| **Model-only / excluded** | A bounded-model abstraction or excluded transition makes no production or ledger claim. |
| **Quint-only investigate** | A model may later test the interaction, but static evidence does not yet prove a production defect. |

| Boundary | Classification | Exact live traceability | Required next evidence |
| --- | --- | --- |
| Router–Graph–Property Index durable projection | **Modeled / experimental** | The tables below map the bounded two-mutation model to ADRs 0023/0024/0029, Graph outbox/repair owners, and twelve scenarios. | Preserve revise: no current Quint verify result and focused Rust/PocketIC comparisons remain not run. |
| Router atomic_insert response loss while CanonicalPending | **Adopted / experimental** | ADR 0029 §2 and ADR 0049 §1558–1579 require exact journal reconciliation; the bounded model maps the implemented exact-receipt, explicit-`Absent`, background-query-only, and conflict boundaries. Commit `d700331c` plus the focused Rust/PocketIC comparisons are production evidence; this model remains non-normative. | Keep the one-request/one-shard boundary and record bounded formal results separately. Do not add CI or extend this slice to projection, retirement, routing/catalog mutation, or multi-shard behavior. |
| ReadMode::AtLeast versus first-delivery outbox work | **Covered** | DerivedIndexOutbox and RepairJournal remain distinct owners; Graph MemoryId 52 stores one fixed key per qualifying source row and exposes their exact ordinary floor. Exact Graph owner tests plus the passing stopped-index/Graph-upgrade PocketIC lifecycle protect the contract. | No production prerequisite remains. Preserve `durableQueue` as a conceptual FIFO abstraction; keep this pilot `revise` until its independent formal gates are rerun. |
| Router direct vector-ingest partial suffix | **Covered / modeled abstraction** | The typed-only `vector_sync_batch_outcome` path collects all successful Graph stamps, atomically revalidates lifecycle/target/definition, and persists an immutable target plus complete operation in the Router outbox before the first Vector await; terminal and later suffix rows remain `Pending`. The typed Vector driver chunks internally at 32 rows. The refined GC model covers a bounded two-operation exact prefix and retained suffix, but still abstracts Candid fitting and the production 32-row chunking mechanism. | GAP-2026-08-20-002 is **Resolved** for the targeted runtime gate. The gate manually drives the Graph/Router timer seams and covers Router upgrade, Vector reopen/rebind, exact GQL search, and idempotent replay; it does not prove autonomous timer firing or deferred watermark/tombstone GC completion. The Quint result is bounded protocol evidence, not a replacement for that runtime gate. |
| Router shard identity across unregister/re-register | **Design/implementation prerequisite; later Quint candidate** | GlobalVertexId has only ShardId plus local id while the live catalog can reuse a numeric shard id. | GAP-2026-08-20-006: select never-reuse, incarnation, or equivalent principal-pinning semantics before stale-delivery tests. |
| Property DROP INDEX retirement | **Rust-test-first + design/implementation prerequisite** | Catalog deletion precedes remote purge; purge progress is call-local; PhysicalIndexId namespaces can diverge for one logical property. | GAP-2026-08-20-005: define durable per-PhysicalIndexId retirement and first add same-property/two-index and lost-response regressions. |
| Index-build label membership and Sealing admission | **Rust-test-first** | Property-transition admission exists, but label gain/loss does not emit exact BuildDml or reject before the canonical label change. | GAP-2026-07-29-006: add focused Graph-owner regressions before migration-lifecycle PocketIC validation. |
| Vector Router watermark and tombstone retention | **Modeled / experimental; production deferred** | Router-originated watermark advancement remains disabled and production tombstone deletion remains conservatively paused. `router_vector_tombstone_gc.qnt` reproduces unsafe fence collection and stale resurrection under `MaxAcknowledged`; sampled and deterministic scenarios preserve safety under `Frozen` and the refined bounded `Contiguous` candidate. The candidate derives each lane frontier from the durable global allocation ceiling plus unresolved exact-lane intents, without a second settled-history owner. | Production still lacks an outbox phase before the Graph await and an authenticated per-target/shard frontier publication path. Settle those recovery, API, and stable-layout contracts before Rust implementation or resuming deletion. The model supplies no subject-map growth bound, exhaustive proof, or liveness guarantee. |
| Ordinary CREATE INDEX | **Intentional / current** | Ordinary non-migration CREATE INDEX is immediately Active by the current DDL contract. | No new gap or Quint slice absent an ADR change or contradicting regression. |
| bulk_load exact retry | **Intentional / current** | Exact append replay is intentionally client-driven by the durable bulk-load lifecycle. | No new gap or Quint slice absent an ADR change or contradicting regression. |
| duplicate index-build pull | **Covered** | Direct exact-replay and conflict coverage protects duplicate build pulls. | No new gap or Quint slice absent an ADR change or contradicting regression. |
| idle-pending restart | **Model-only / excluded** | The bounded model omits idle-pending restart as an unobservable safety stutter. | No production or ledger claim; keep the transition excluded until a concrete observable behavior is selected. |

## Router–Graph–Vector tombstone GC slice

Status: **Modeled as experimental, non-blocking**. The model has one Router,
two bounded `(Vector target, shard)` lanes, three Router operations, one Graph
remove at `m11`, and an unrelated allocation gap at `m13`. Admission atomically
advances the durable allocation ceiling and records the exact intent before the
first Graph await. Graph delivery/accept/reject, Graph response observation or
loss, Vector delivery/apply/exact-prefix observation or loss, frontier
publication, GC, replay, and restart are separate transitions.

The three policies are deliberately distinct:

- `Frozen` is the live production behavior: Router watermark stays zero and
  the m11 deleted subject clock remains a replay fence.
- `MaxAcknowledged` publishes m12 after its response is observed even while
  m10 is outstanding. With Graph watermark m11, GC removes the m11 fence and
  replay of m10 restores `Live(10)`. This is the rejected counterexample.
- `Contiguous` publishes per target/shard lane only below that lane's oldest
  unresolved Router intent. It keeps the primary frontier at 9 while m10 is in
  either Graph-pending or Vector-pending phase, then permits GC after exact
  resolution. An unrelated allocation gap and unresolved work in another lane
  do not create a false block. This is a candidate, not production behavior.

The candidate keeps no settled-history set. Once a Graph rejection or exact
Vector acknowledgement resolves an intent, the durable global allocation
ceiling proves that its stamp cannot be reused; the oldest remaining intent in
the exact lane is therefore sufficient to derive the bounded frontier. This
does not select a Rust representation or publication API. The model also does
not claim fairness, finite-time convergence, arbitrary batch size, topology
change safety, or physical subject-map bounds.

### State and source correspondence

| Quint symbol | Ownership and source correspondence |
| --- | --- |
| `routerState.allocatedThrough` | Bounded analogue of the existing durable `ROUTER_MUTATION_COUNTER`; unrelated allocated stamps are explicit non-vector gaps and cannot be reused. |
| `routerState.intents` | Candidate extension of `ROUTER_VECTOR_INGEST_OUTBOX`: one exact operation, target/shard lane, and `AwaitingGraph` or `AwaitingVector` phase. Production ownership currently begins only after all successful Graph responses. |
| `primaryMaxAcknowledged` / `secondaryMaxAcknowledged` | Model-only state for the rejected `MaxAcknowledged` comparison; not a proposed production owner. |
| `vectorState.targetClock` / lane clocks | `VECTOR_SUBJECT_TO_ID` clocks applied by `crates/vector-canister/src/facade/store/mutation.rs`. |
| lane Graph/Router watermarks | Bounded analogue of `ShardWatermarks` in `crates/vector-canister/src/facade/store/watermark.rs`; production Router watermark remains zero. |
| `graphTransport` / `vectorTransport` / `frontierTransport` | Volatile message/response abstractions. Serialization, Candid size fitting, timers, and a concrete frontier endpoint are excluded. |

The protocol safety property composes `routerOwnershipPartition`,
`frontierDoesNotPassOutstanding`, `noUnsafeFenceCollection`, and
`noStaleResurrection`. Nine deterministic tests cover the frozen replay fence,
the max-ack counterexample through resurrection, candidate-frontier delayed GC,
pre-Graph restart, lost Graph response, Graph rejection resolution, exact
partial-prefix retention, unrelated allocation plus lane isolation, and lost
frontier response.

### Reproducible validation

```sh
quint typecheck formal/quint/router_vector_tombstone_gc.qnt
quint typecheck formal/quint/router_vector_tombstone_gc_tests.qnt
QUINT_HOME=/private/tmp/gleaph-quint-home quint test \
  formal/quint/router_vector_tombstone_gc_tests.qnt \
  --main=routerVectorTombstoneGcTests --seed=0x270
QUINT_HOME=/private/tmp/gleaph-quint-home quint run \
  formal/quint/router_vector_tombstone_gc.qnt \
  --main=routerVectorTombstoneGc --init=initFrozen \
  --max-steps=35 --max-samples=5000 --seed=0x270 \
  --invariant protocolSafety
QUINT_HOME=/private/tmp/gleaph-quint-home quint run \
  formal/quint/router_vector_tombstone_gc.qnt \
  --main=routerVectorTombstoneGc --init=initContiguous \
  --max-steps=35 --max-samples=5000 --seed=0x271 \
  --invariant protocolSafety \
  --witnesses routerAdmissionWitness unrelatedAllocationWitness \
  graphDispatchWitness graphAcceptWitness graphRejectWitness \
  graphResponseLossWitness graphAcceptObservationWitness \
  graphRejectObservationWitness vectorDispatchWitness vectorApplyWitness \
  vectorResponseLossWitness vectorResponseObservationWitness \
  graphRemoveWitness frontierDispatchWitness frontierApplyWitness \
  frontierResponseLossWitness frontierResponseObservationWitness \
  subjectClockGcWitness restartWitness graphRejectionResolvedWitness \
  partialPrefixRetainedWitness laneIsolationWitness
QUINT_HOME=/private/tmp/gleaph-quint-home quint run \
  formal/quint/router_vector_tombstone_gc.qnt \
  --main=routerVectorTombstoneGc --init=initMaxAcknowledged \
  --max-steps=35 --max-samples=5000 --seed=0x272 \
  --witnesses unsafeFenceCollectionWitness staleResurrectionWitness
```

The installed CLI was `quint 0.32.0`. Both modules typechecked and all nine
deterministic tests passed. The two safe policy runs explored 5,000 sampled
traces through depth 35 without an invariant violation. In the candidate run,
all 22 action/scenario witnesses were reached; subject-clock GC appeared in 801
traces, Graph rejection resolution in 2,358, exact partial-prefix retention in
11, and lane isolation in 226. The max-ack run witnessed unsafe fence collection
in 60 traces and stale resurrection in 8 traces. `quint verify` was intentionally
not run, so these results are sampled bounded evidence, not exhaustive model
checking or a liveness proof.

## Router CanonicalPending exact-replay slice

Status: **Adopted as experimental, non-blocking**. This is a separate fixed
Client/Router/Graph model for one admitted ordered `atomic_insert`, one target,
and one Graph shard. It is deliberately not a projection, retirement, routing,
catalog, or multi-shard model. The production contract remains owned by ADRs
0029/0049 and the Rust/PocketIC comparisons recorded below.

### State and ownership map

| Quint symbol | Durable or volatile owner | Contract and live anchor |
| --- | --- | --- |
| `routerRecord` | Durable Router phase, immutable target, and optional receipt | ADR 0029 §2; `crates/router/src/facade/stable/label_stats.rs` request identity/target records and `crates/router/src/gql.rs` reconciliation arms |
| `graphJournal` | Durable Graph exact completed journal receipt | ADR 0029 §1 and ADR 0049 §1558–1573; `crates/graph-kernel/src/plan_exec.rs` journal identity/completion and ordered Graph handlers |
| `canonicalEffectCount` | Durable Graph canonical-effect count (bounded abstraction) | ADR 0029 §1; Graph ordered batch execution in `crates/graph/src/canister/handlers.rs` |
| `graphDispatchCount` | Model-visible dispatch accounting for at-most-once and no-background-dispatch checks | Model-only diagnostic; compared with Router/PocketIC focused reconciliation tests |
| `admissionTargetSnapshot` | Model-only immutable copy of the target at admission | Independent before-image used by `immutableStoredTarget`; no production state analogue |
| `transport` | Volatile request/response availability | ADR 0049 §1558–1569; Graph response-loss seam in `crates/graph/src/test_fault.rs` |
| `journalFence` | Durable last journal observation that gates redispatch | ADR 0029 §2 and ADR 0049 §1561–1579; Router trigger-aware reconciliation in `crates/router/src/gql.rs` |
| `lastAction` | Model-only witness/diagnostic instrumentation | Sensitive preservation actions carry one `OwnerSnapshot` before-image; no production state analogue |

### Actions and source map

| Quint action | Required transition | Exact live owner or model boundary |
| --- | --- | --- |
| `init` | Establish empty Router/Graph owners and idle transport | Model-only initial state |
| `admitPending` | Persist the immutable family, fingerprints, mutation id, shard, and request token before dispatch | Router admission in `crates/router/src/gql.rs` and stable request types in `crates/router/src/facade/stable/label_stats.rs` |
| `dispatchCanonical` | Dispatch exactly the stored target/request identity | ADR 0049 §1462–1478; ordered dispatch helpers in `crates/router/src/gql.rs` |
| `graphCommit` | Atomically create one canonical effect and its exact completed Graph journal receipt, then expose the response | ADR 0029 §39–48/§413–415; ordered Graph handlers and `crates/graph-kernel/src/plan_exec.rs` |
| `loseCanonicalResponse` | Remove only the volatile Graph-to-Router response | ADR 0049 §1558–1565; `crates/graph/src/test_fault.rs` and focused PocketIC tests |
| `persistCanonicalResponse` | Persist `CanonicalCommitted` from the ordinary canonical response path | Model-only separation of normal response persistence from journal-query adoption; live Router record completion helpers in `crates/router/src/gql.rs` |
| `queryJournal` | Query the exact stored Graph identity under explicit retry or background recovery | ADR 0029 §218–224; `reconcile_ordered_canonical_pending` in `crates/router/src/gql.rs` |
| `journalRespond` | Expose bounded exact/absent/error/non-exact journal evidence without canonical execution | Model-only semantic evidence boundary over Graph journal lookup |
| `adoptExactReceipt` | Accept only exact `Active` + `Completed` evidence and persist `CanonicalCommitted` | ADR 0049 §1561–1573; exact receipt and Router commit helpers in `crates/router/src/gql.rs` |
| `explicitAbsentRedispatch` | Redispatch the same stored request only after explicit `Absent` | ADR 0049 §1561–1575; trigger-aware reconciliation in `crates/router/src/gql.rs` |
| `observeBackgroundAbsent` | Keep pending and perform no canonical dispatch after background `Absent` | ADR 0029 §218–224; Router recovery path in `crates/router/src/gql.rs` |
| `observeQueryError` | Keep pending after query error with only diagnostic instrumentation | Model-only bounded error branch; live non-exact fail-closed branch |
| `rejectNonExactEvidence` | Keep target/phase/receipt/effect count unchanged for all non-exact evidence | ADR 0049 §1570–1579; focused Router reject test |
| `duplicateGraphDispatch` | Re-enter Graph with the same identity to exercise journal-first replay without a second effect | Model-only Graph replay boundary; exact replay semantics in ADR 0029 §414–416 |
| `restart` | Clear volatile transport only and preserve both durable owners | ADR 0029 §199–224; focused Router/PocketIC recovery comparisons |
| `exactReplay` | Return the stored receipt for the same key/family/fingerprint without a new effect | ADR 0029 §414–416; `respond_from_existing_ordered_atomic_insert` |
| `rejectConflict` | Keep all durable state/effect counts unchanged for a conflicting family or fingerprint | ADR 0049 §1544–1556; existing conflict handling |

The model-only `persistCanonicalResponse`, `journalRespond`, and
`duplicateGraphDispatch` actions keep ordinary response persistence, journal
query/adoption, and Graph journal-first replay distinct. They do not add
production APIs or claim that the model has a one-to-one wire transition.
`OwnerSnapshot` exists only inside sensitive `lastAction` variants so the
preservation invariants compare the resulting Router/Graph owners with the
actual pre-action values instead of inferring history from a tag.

### Model-only instrumentation and helper traceability

| Quint symbol | Role in this bounded model | Classification |
| --- | --- | --- |
| `admissionTargetSnapshot` | Captures the admitted target independently so `immutableStoredTarget` compares two state owners instead of recomputing the target from `routerRecord`. | Model-only immutable-identity witness; no production state analogue. |
| `OwnerSnapshot` | Stores one exact pre-action Router/Graph owner image for preservation-sensitive actions. | Model-only diagnostic type; it is carried by `lastAction`, not production durable state. |
| `ActionTag` snapshot payloads | `ObserveBackgroundAbsentAction`, `ObserveQueryErrorAction`, `RejectNonExactEvidenceAction`, `RestartAction`, `ExactReplayAction`, and `RejectConflictAction` each carry one `OwnerSnapshot`; the remaining tags are payload-free. | Model-only historical instrumentation; no production callback or message is implied. |
| `phaseOf` | Derives the bounded `RouterPhase` from `routerRecord` for `phaseMonotonicity`. | Pure model helper; `routerRecord` remains the source of truth. |
| `currentOwners` | Captures the current durable-owner values immediately before a sensitive action updates `lastAction`. | Model-only before-image value. |
| `coreOwnersMatch` | Compares Router record, Graph journal, effect count, dispatch count, and admission target with a before-image while allowing the journal fence to record an observation. | Model-only invariant helper. |
| `allOwnersMatch` | Extends `coreOwnersMatch` with exact journal-fence equality for restart, replay, and conflict actions. | Model-only invariant helper. |

### Invariants, deterministic scenarios, and witnesses

`protocolSafety` is exactly the conjunction of: `canonicalApplyAtMostOnce`,
`canonicalJournalAtomicity`, `immutableStoredTarget`,
`exactReceiptJustification`, `phaseMonotonicity`,
`backgroundNeverCanonicalDispatches`, `explicitRedispatchRequiresAbsent`,
`nonExactEvidenceFailsClosed`, `restartPreservesDurableOwners`, and
`replayAndConflictIsolation`.

`immutableStoredTarget` compares the live record with the independent
admission snapshot. The background, non-exact/query-error, restart, replay,
and conflict invariants compare exact Router record, Graph journal, effect and
dispatch counts, admission target, and—where it must not change—the journal
fence with the corresponding action's model-only before-image.

The test module contains exactly these ten anchored scenarios:

```text
lostResponseExplicitRetryAdoptsExactReceipt
lostResponseBackgroundRecoveryAdoptsExactReceipt
explicitAbsentRedispatchesStoredRequest
backgroundAbsentNeverRedispatches
nonExactEvidenceStaysPending
queryErrorStaysPending
restartPreservesPendingAndJournal
duplicateGraphDispatchAppliesOnce
exactCommittedReplayReturnsStoredReceipt
conflictingFingerprintOrFamilyFailsClosed
```

The sampled model exposes these positive witnesses:

```text
admitWitness dispatchWitness graphCommitWitness
canonicalResponseLossWitness journalQueryWitness exactReceiptAdoptionWitness
explicitAbsentRedispatchWitness backgroundAbsentWitness nonExactEvidenceWitness
restartWitness exactReplayWitness conflictWitness
```

### Reproducible validation and bounded results

The exact formal commands are:

```sh
quint --version
quint typecheck formal/quint/router_atomic_insert.qnt
quint typecheck formal/quint/router_atomic_insert_tests.qnt
QUINT_HOME=/private/tmp/gleaph-quint-home quint test \
  formal/quint/router_atomic_insert_tests.qnt \
  --main=routerAtomicInsertTests \
  --match='^(lostResponseExplicitRetryAdoptsExactReceipt|lostResponseBackgroundRecoveryAdoptsExactReceipt|explicitAbsentRedispatchesStoredRequest|backgroundAbsentNeverRedispatches|nonExactEvidenceStaysPending|queryErrorStaysPending|restartPreservesPendingAndJournal|duplicateGraphDispatchAppliesOnce|exactCommittedReplayReturnsStoredReceipt|conflictingFingerprintOrFamilyFailsClosed)$' \
  --seed=0x260
QUINT_HOME=/private/tmp/gleaph-quint-home quint run \
  formal/quint/router_atomic_insert.qnt \
  --main=routerAtomicInsert --invariant=protocolSafety \
  --max-steps=20 --max-samples=10000 --seed=0x260 --backend=typescript \
  --witnesses admitWitness dispatchWitness graphCommitWitness \
  canonicalResponseLossWitness journalQueryWitness exactReceiptAdoptionWitness \
  explicitAbsentRedispatchWitness backgroundAbsentWitness nonExactEvidenceWitness \
  restartWitness exactReplayWitness conflictWitness
```

Validation anchor: `2026-08-21 07:49:55 UTC +0000`. The installed CLI was
`quint 0.32.0`. The main and test modules typechecked in `0.834s` and `0.973s`.
The exact anchored suite selected and passed 10 tests in `404ms`. The bounded
TypeScript run explored 10,000 traces through depth 20 in `24.201s`, found no
`protocolSafety` counterexample in that sample, and reached every witness:

| Witness | Count |
| --- | ---: |
| `admitWitness` | 10000 |
| `dispatchWitness` | 9998 |
| `graphCommitWitness` | 5182 |
| `canonicalResponseLossWitness` | 2002 |
| `journalQueryWitness` | 8054 |
| `exactReceiptAdoptionWitness` | 319 |
| `explicitAbsentRedispatchWitness` | 397 |
| `backgroundAbsentWitness` | 380 |
| `nonExactEvidenceWitness` | 5763 |
| `restartWitness` | 8109 |
| `exactReplayWitness` | 2410 |
| `conflictWitness` | 9939 |

The production comparisons were run separately:

```sh
cargo test -p gleaph-router --lib canonical_pending_reconciliation
cargo test -p gleaph-pocket-ic-tests --test adr0057_atomic_insert \
  atomic_insert_canonical_pending_reconciliation
```

The Router filter selected and passed exactly four tests (`792` filtered out),
and the PocketIC filter selected and passed exactly two tests (`13` filtered
out). These are implementation comparisons, not proof supplied by the Quint
model. The unfiltered `adr0057_atomic_insert` target was not rerun; its earlier
14-pass/1-failure HTTP-adapter result remains non-terminal. `quint verify` was
intentionally not run. Sampled execution is not exhaustive verification or a
liveness proof.

## Router–Graph–Property Index projection traceability

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
| `oldestDurableMutationRank` | **Model-only** queue-derived rank | Explicitly distinct from `crates/graph/src/facade/store/index_pending_floor.rs::GraphStore::index_pending_min_mutation_id`; production reads the exact MemoryId 52 fixed-index floor. The conceptual rank remains FIFO and is not that live floor. |
| `conceptualOldestDurableMutationRank` | **Model-only** bounded rank guard; no claim about the live floor | Same deliberate distinction; production evidence is the exact Graph owner-floor tests and the passing stopped-index/Graph-upgrade PocketIC lifecycle. |
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

### Deterministic coverage

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

### Reproducible commands

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
cargo test -p gleaph-graph --lib facade::store::index_pending_floor::tests::quarantine_and_partial_ack_preserve_exact_multiplicity -- --exact
cargo test -p gleaph-graph --lib index::inv_oracle::postings_converge_to_store_projection_after_failure_and_compaction -- --exact
cargo test -p gleaph-router --lib facade::stable::label_stats::tests::lifecycle_phase_never_completes_with_outstanding_work -- --exact
cargo test -p gleaph-pocket-ic-tests --test router_gql_query single_shard_mutation_token_barrier_status_lifecycle -- --exact
```

Current-repair status (OS UTC anchor `2026-08-19 21:39:57 UTC +0000`): these
four Rust unit checks and one PocketIC check were **not run** during the
current model repair. The commands above are reproducibility instructions,
not pass evidence; the adoption gate remains incomplete for this comparison
surface, and no Rust/PocketIC pass is claimed here.

### Adoption decision

The Router–Graph–Property Index projection pilot remains **`revise`**, experimental,
and non-blocking. Its repaired model has passing typecheck, exact scenarios, and
10,000-sample evidence, but its current production-comparison surface was not rerun
and it has no current `quint verify` result. Its validation evidence anchor remains
`2026-08-19 21:19:37 UTC +0000`.

The separate Router–Graph `CanonicalPending` slice is **`adopt as
experimental/non-blocking`** for exactly its one-request, one-shard boundary. Both
modules typecheck, all 10 deterministic scenarios pass, the 10,000-sample run
reaches all 12 witnesses without a sampled `protocolSafety` counterexample, and the
focused Router and PocketIC comparisons pass 4/4 and 2/2. This decision adds no CI
gate and is not exhaustive verification, a liveness proof, or adoption of the
projection, retirement, routing/catalog, multi-shard, or other excluded behaviors.
The CanonicalPending evidence anchor is `2026-08-21 07:49:55 UTC +0000`.
