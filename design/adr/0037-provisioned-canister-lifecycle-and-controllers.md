# 0037. Provisioned canister lifecycle and controllers

Date: 2026-07-04
Status: Partially Implemented
Last revised: 2026-11-22
Anchor timestamp: 2026-07-04 13:05:02 UTC +0000

## Context

Provision-created Graph and index canisters need a controller policy and a durable deletion path.
Router owns logical topology and authorization, but should not become a controller or invoke the
management canister. A delete spans Router state, Provision job state, and management calls, so a
failed callback after successful deletion must be reconcilable.

The IC management API's `stop_canister` call returns when the canister is stopped (or returns an
error), and `delete_canister` returns after deletion. The **overall Router-to-Provision workflow** is
still asynchronous and retryable because it contains several calls and a final Router acknowledgement.

## Decision

### Controller policy

Normally issued Graph, Property Index, and Vector Index canisters have exactly:

- Provision, as lifecycle executor; and
- a governance/recovery principal configured at deployment bootstrap.

Router is not a controller. End users are not controllers. This policy applies only to canisters
issued by Provision. Router and Provision installation, upgrade, and recovery remain bootstrap or
governance operations outside this ADR; Provision does not control or upgrade itself.

Provision supplies exactly `[Provision, governance/recovery]` in the `create_canister` controller
settings and persists that intended setting before the call. It verifies the observed settings before
installation and Router registration; a mismatch enters reconciliation rather than activation.

Routine upgrade orchestration is also outside this ADR. ADR 0036 chooses compatible artifacts, but a
future decision must define upgrade ordering, rollback, and data compatibility.

### Router deletion projection

On an authorized delete request, Router atomically:

1. sets the existing graph status to `GraphStatus::Deleting`;
2. creates or updates its separate provisioning/cleanup request projection; and
3. rejects new work that requires the graph to be active.

This ADR does not invent `GraphStatus::CleanupFailed`. Detailed cleanup progress belongs to the
separate Router projection introduced by ADR 0035 and is derived from Provision notifications.

Router sends a deletion-specific resolved envelope; creation-only fields such as `release_id` are
not reused:

```text
ProvisionDeleteRequest = {
  deployment_id, request_id, request_fingerprint,
  intent_key, graph_id, authorized_caller,
  resources[{kind, canister_id}], router_callback_principal
}
ProvisionDeleteResult = { request_id, request_fingerprint, deleted_resources, outcome }
RouterDeleteAck = { request_id, accepted_tombstone_version }
```

The same fingerprint-conflict and unfinished-intent-lock rules as ADR 0035 apply. Router persists its
request projection before sending the envelope; Provision persists the cleanup job before stopping a
resource.

### Provision cleanup job

Provision persists:

```text
CleanupRequested -> StopPending -> Stopped -> DeletePending
                 -> Deleted -> RouterAckPending -> Completed
```

Each target canister id comes from the accepted issuance receipt or a Router-resolved request and is
recorded before the first management call. After `stop_canister` returns success, Provision records
`Stopped`; after `delete_canister` returns success, it records `Deleted`. An already-stopped or
already-absent target is reconciled as progress when platform evidence confirms that state.

Provision notifies Router only after all requested resources are deleted. Router verifies the
request/fingerprint, commits its tombstone/catalog removal, and returns an acknowledgement. If that
callback fails, Provision remains `RouterAckPending` and retries notification; it never repeats the
successful delete as a new job.

Retry exhaustion produces an operator-visible nonterminal blocked/reconcile state, not a false
all-or-nothing failure. Governance recovery compares the Provision receipt/job with Router's
`Deleting` entry and tombstone, then either resumes cleanup, acknowledges an already deleted resource,
or records a concrete manual intervention.

## Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Router authorizes deletion and owns the logical graph tombstone. | Router admin/API and catalog transaction |
| Provision alone executes normal stop/delete calls. | Provision management-call boundary |
| Router never becomes a controller of issued data/index canisters. | Provision controller settings validator |
| A successful remote delete survives callback failure. | `Deleted` / `RouterAckPending` durable states |
| Completion requires Router acknowledgement of canonical removal. | Cleanup state transition |
| Recovery reconciles both Provision evidence and Router state. | Governance recovery procedure |

## Alternatives

- **Router controls and deletes canisters:** rejected because it mixes topology with lifecycle
  execution.
- **Immediate synchronous delete API:** rejected because the full distributed flow cannot be atomic.
- **Terminal failure after callback loss:** rejected because it loses evidence of successful remote
  effects.

## Consequences

Deletion is eventually completed and explicitly observable rather than pretending to be atomic.
Router requires a deletion projection and tombstone semantics; Provision requires a durable cleanup
worker and recovery tooling.

## Amendment 2026-11-22: resource deletion

### Decision

One add-on canister can be deleted on its own, without deleting its graph. This is what
`DROP TEXT INDEX` needs: before this amendment the drop removed the catalog row and left the text
canister running with nothing referencing it.

- **Scope.** Property-index, vector-index, text-index and inference canisters can be deleted one at
  a time. A Graph shard or the Router is deleted only with its graph or deployment; Provision
  answers `UnsupportedResource` for both.
- **Wire.** Router calls Provision
  `delete_resource(ResourceDeletionRequest { deployment_id, logical_resource, canister_id })` and
  gets `Deleted | Replay` or a `ResourceDeletionError`. The types live in
  `gleaph_graph_kernel::provisioning::wire`.
- **No callback, no Router ack.** This replaces the `ProvisionDeleteResult` notification and the
  `RouterDeleteAck` sketched above for this case. ADR 0035 keeps Provision from calling Router, so
  Router waits for the reply and owns the retry. Router's own durable tombstone (for TEXT, the
  `Dropping` catalog row) is what resumes an unfinished deletion. Provision's record becomes
  terminal at `Deleted` and replays from there, so Provision does not need to learn that Router
  committed.
- **States.** The job states above collapse to `DeletePending → Deleted`. One step stops and
  deletes the canister. A canister that `canister_status` cannot find counts as deleted.

### Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Provision deletes only the canister that its own issuance job recorded for that exact `(deployment, logical_resource)` intent, and only after the job installed it (`RouterRegistrationPending` or `Completed`). | `delete_resource_with_caller` (crates/provision) |
| Only a granted issuer may call, and an `OwnDeployment` issuer only for its own deployment. This is the same rule as the registration ack. | `delete_resource_with_caller` |
| The deletion is recorded before the first management call, so a retry resumes it instead of starting another. | `ProvisionJobStore::begin_resource_deletion`, MemoryId 23 keyed by canister |
| Finishing a deletion releases that intent's Map 2/3 rows in the same step, and only if they still belong to the job that issued the canister. | `ProvisionJobStore::finish_resource_deletion` |
| A deleted canister is left out of every deployment scan, so handover, termination and repair never act on it. | `deployment_canister_records_inner` |
| Router retires its issuance records (Maps 45/46/47), completed or not, only after Provision confirms the deletion. | `provisioning::resource::delete_provisioned_resource_with`, `RouterProvisioningRequestStore::retire_requests_for_intent` |
| Provision finds the issuing job by scanning its canonical job map, because a job whose registration completed no longer owns its Map 2 row. | `ProvisionJobStore::jobs_issuing` |

### TEXT orchestration (Router)

`DROP TEXT INDEX` runs these steps in order. Each step is idempotent, and repeating the drop
resumes from the first step that did not finish:

1. Reject the drop if a backfill build is live.
2. Mark the row `Dropping`. The definition becomes planner-invisible and DML-inert. It is never
   attached to a shard again, no backfill migration may start on it
   (`text_index_catalog::ensure_backfill_admissible`), and its name stays taken.
3. Retire the definition on every live shard. Graph `admin_retire_text_target` sets the shard's
   `TextTarget::Retired`, and from then on the drain discards that definition's rows without a
   call.
4. Ask Provision to delete the canister.
5. Clear the Router issuance record and remove the row.

### Vector orchestration: retire, do not delete

A graph has one vector canister, and every vector index of the graph lives on it (ADR 0031 target
model B). Dropping one index therefore cannot delete the canister: the other indexes still use it.
`DROP VECTOR INDEX` on a targeted definition retires that index on the canister instead:

1. Reject the drop if the target has unresolved direct-ingestion intents (MemoryId 53 rows in
   `AwaitingGraph` or `AwaitingVector`; an `AwaitingFrontier` row is already applied). This check
   and the next step run in one synchronous segment with no await between them, so no intent is
   admitted between the check and the mark.
2. Mark the row `Dropping` (`VectorIndexLifecycle`). Ingestion and search refuse the index, the
   graph-shard catalog leaves it out (so shards dispatch no new removes for it), and
   `CREATE VECTOR INDEX` on the graph is refused until the drop finishes.
3. Drive the Vector canister's `admin_retire_vector_index` to its last step. The canister marks
   its definition row `Retiring`, purges the index's subjects, pages, heads and centroids, and
   writes `Retired`, which it keeps as the fence against a late sync op
   ([vector index retirement](../index/vector-index.md#index-retirement)). A rebuild in progress
   refuses the retirement; the drop fails with `Conflict` and the row stays `Dropping`.
4. Remove the row.

The drop deletes no canister, so it does not use the deletion protocol above. That holds for the
last vector index of a graph as well: the canister stays the graph's vector target, and the next
`CREATE VECTOR INDEX` on the graph uses it again. **Decided, not implemented:** whether the last drop
should also delete the canister. Deleting it needs more than this protocol: each shard's vector
route must be cleared, its durable vector work discarded, the Router shard registry and ingestion
lanes reset, and a later `CREATE VECTOR INDEX` provisioned anew.

### Implemented versus not

- **Implemented:** the Provision endpoint and record, the Router deletion sender, the TEXT drop
  flow, the Graph retired text target, and the vector retirement flow above.
- **Not implemented:**
  - Deleting a whole graph through this protocol.
  - Deleting the vector canister when the last vector index of a graph is dropped.
  - A Provision-side sweep for `DeletePending` records. Only the Router's retry drives them.
  - Controller enforcement and governance reconciliation.

## Implementation status

**Partially Implemented (2026-11-22).** Resource deletion and the vector retirement flow are
implemented as described in the amendment above. Controller enforcement, graph-level cleanup jobs, the Router graph deletion
projection, and reconciliation do not exist yet.

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — request, ack, and receipt protocol.
- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — release selection; upgrades remain separate.
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — deletion authorization and cycle funding boundary.
- [IC management canister](https://docs.internetcomputer.org/references/management-canister/) — authoritative stop/delete semantics.
