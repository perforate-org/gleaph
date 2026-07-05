# 0035. Provision canister and issuance protocol

Date: 2026-07-04
Status: Partially Implemented
Last revised: 2026-07-04 20:10:18 UTC +0000
Anchor timestamp: 2026-07-04 20:10:18 UTC +0000

## Context

Router owns graph identity, tenancy, routing, and the stable catalogs that collectively describe a
Gleaph deployment. A `GraphRegistryEntry` cannot represent a graph before its first canister exists:
it requires a `canister_id`, and its current `ProvisioningState` is not a complete issuance journal.
Canister creation and installation are irreversible cross-canister effects that may succeed before a
later callback or Router registration fails.

Automated provisioning therefore needs an idempotent executor without turning Router into a
management-canister client or creating a second topology registry.

## Decision

Introduce a dedicated **Provision** canister. Router remains the sole owner of logical graph identity,
tenancy, and routing; Provision owns only durable issuance jobs and receipts.

### Router orchestration state

Router adds a provisioning-request catalog separate from `GraphRegistryEntry`:

```text
ProvisioningIntentKey = (deployment_id, resource_kind, logical_resource_key)
RouterProvisioningRequest = {
  request_id, request_fingerprint, caller, graph_name, reserved_graph_id?,
  requested_resources, state, provision_receipt?
}
```

This record can exist before a canister id exists. Router creates `GraphRegistryEntry` and related
shard/index catalog records only after Provision reports installed canisters. The existing
`GraphRegistryEntry.provisioning_state` is not used as the pre-creation journal; a later implementation
may remove it or retain it only as a derived compatibility projection, with that migration decided in
the implementation slice.

### Resolved request and acknowledgement

After authenticating the caller and reserving graph identity, Router sends a resolved envelope:

```text
ProvisionRequest = {
  deployment_id, request_id, request_fingerprint,
  intent_key, reserved_graph_id?, graph_name,
  requested_resources,
  authorized_caller, release_id,
  router_callback_principal
}
```

Provision accepts envelopes only from the Router principal registered for `deployment_id`. It does
not read Router tenancy state or re-derive authorization. The same `request_id` and fingerprint
returns the existing job/receipt; the same id with a different fingerprint returns `Conflict`.
A durable intent lock rejects or joins a distinct request id targeting the same unfinished
`ProvisioningIntentKey`.

The `deployment_id -> Router principal, governance principal` binding is canonical Provision
bootstrap configuration, written only by the governance/recovery principal. It is authentication
configuration, not graph topology or tenancy. Router owns every logical graph record; Provision owns
only this deployment trust binding and its job journals.

Provision reports:

```text
ProvisionResult = {
  request_id, request_fingerprint, release_id,
  created_resources[{kind, canister_id, artifact_hash}],
  terminal_outcome
}
RouterProvisionAck = { request_id, accepted_registry_version }
```

Router verifies the fingerprint and intent lock, atomically commits the affected Router catalogs,
then returns the acknowledgement. Provision records `Completed` only after receiving that ack.

### Durable job state

Provision persists the next state before each remote effect and the observed result after it:

```text
Submitted -> Reserved -> CreatePending -> CanisterCreated
          -> InstallPending -> Installed
          -> RouterRegistrationPending -> RouterAckPending -> Completed
```

If creation or installation succeeds but a later step fails, the job resumes from the persisted
canister id; it never issues a fresh create. Failures requiring removal transition to
`CleanupPending` and use ADR 0037. `Failed` is terminal only when no external resource remains or
cleanup has been reconciled.

Deployment bootstrap is out of band: governance installs Router and Provision and binds their
principals. Subsequent logical-graph, shard, and index issuance uses this protocol.

## Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Router stable catalogs are the only topology and tenancy source of truth. | Router catalog transaction boundary |
| Provision owns request idempotency, effect progress, and receipts, but no graph RBAC or routing map. | Provision stable job store and API |
| A request cannot create twice after any successful management-canister call. | Persisted effect state and stored canister id |
| Concurrent requests cannot provision the same logical intent independently. | Provision intent lock |
| Completion means Router has acknowledged its canonical catalog update. | Provision `RouterAckPending -> Completed` transition |

## Alternatives

- **Router executes management calls:** rejected because it combines topology ownership with
  lifecycle execution and cycle/artifact policy.
- **Off-chain deployment only:** retained for local bootstrap, but rejected as the managed runtime
  protocol because it provides no durable idempotency or reconciliation.
- **Provision owns graph topology:** rejected because it duplicates Router state.

## Consequences

Provision adds a canister and a cross-canister saga, but each state has one owner and every
irreversible effect is resumable. Router APIs and stable layout require an implementation ADR/slice
before this proposal can be accepted.

## Implementation status

**Partially Implemented (2026-07-04).** This slice adds the Router-owned provisioning-request
catalog (three stable-memory regions and the `RouterProvisioningRequestStore` API) and all
ADR 0035 wire types (`ProvisionRequest`, `ProvisionResult`, `RouterProvisionAck`,
`ProvisionableResource`, etc.). The Provision canister, cross-canister envelope send/recv,
ingress endpoints, artifact catalog, lifecycle controller policy, and cycle algebra remain
proposed and are scheduled for later slices.

## Cross-links

- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — compatible release selection and artifacts.
- [ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md) — cleanup and controllers.
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — admission and cycle reservation.
