# 0037. Provisioned canister lifecycle and controllers

Date: 2026-07-04
Status: proposed
Last revised: 2026-07-04
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

## Implementation status

**Proposed.** Controller enforcement, cleanup jobs, Router deletion projection, and reconciliation do
not exist yet.

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — request, ack, and receipt protocol.
- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — release selection; upgrades remain separate.
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — deletion authorization and cycle funding boundary.
- [IC management canister](https://docs.internetcomputer.org/references/management-canister/) — authoritative stop/delete semantics.
