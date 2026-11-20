# ADR 0134: Deployment suspension and termination

Status: accepted; phase 1 (suspend/resume) implemented, deletion deferred.

## Decision

Deployment shutdown is split into a reversible **suspension** and an irreversible **deletion**.
Phase 1 implements suspension only:

```text
Active --request--> SuspensionPending --all stopped--> Suspended --cancel--> Cancelled
                                                         \--cancel (retry)--> Cancelled
```

A suspended deployment keeps every canister and all data. It rejects new provisioning envelopes;
existing state is untouched. Cancellation starts the recorded canisters again. Deletion is a later
phase and needs its own ADR amendment because canister deletion is irreversible.

## Authority

- request: deployment owner, configured service operator, or governance
- cancel: deployment owner, configured service operator, or governance
- read: the `get_deployment` audience (owner/operator/billing authority/governance)

The operator may suspend for non-payment, but suspension is not deletion and must remain
reversible. Deleting owner data requires an explicit owner confirmation or a governance
emergency path in the deletion phase.

## Durable record

One `DeploymentTermination` per deployment:

- deployment_id, initiator, reason
- state (`SuspensionPending`, `Suspended`, `Cancelled`)
- requested_at_ns, grace_until_ns (30 days from suspension)
- cancelled_at_ns
- pending_canisters (resumable stop list)

The record is kept after cancellation for audit. A new request may overwrite a Cancelled record;
it may not overwrite a live Suspended/SuspensionPending record.

## Safety

- Refuse to suspend while any canonical job is not terminal; the operator must finish or repair it
  first.
- Persist `SuspensionPending` before the first management call. A failed stop keeps the remaining
  list and a replay resumes it.
- Cancellation starts every recorded canister; a failure leaves the record Suspended and returns
  the failed canisters for retry.
- `accept_envelope` returns `DeploymentSuspended` while a live suspension exists.
- No canister is deleted in phase 1.

## Deferred

- deletion order, export acknowledgement, credit settlement, and fiat refunds
- grace-period expiry automation
- Managed operator deletion without owner confirmation
