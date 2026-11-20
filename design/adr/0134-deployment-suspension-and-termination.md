# ADR 0134: Deployment suspension and termination

Status: accepted; phase 1 (suspend/resume) and phase 2 (guarded deletion) implemented.

## Decision

Deployment shutdown is split into a reversible **suspension** and an irreversible **deletion**.
Phase 1 implements suspension only:

```text
Active --request--> SuspensionPending --all stopped--> Suspended --cancel--> Cancelled
                                                         \--delete (after grace)--> Deleting --> Deleted
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

## Phase 2: guarded deletion

Deletion is two independent checks beyond suspension:

1. `now_ns >= grace_until_ns` unless governance sets `force`.
2. `credit_cycles == 0`; the operator must reverse or consume unused credit first. The reversal is
   the existing full-unused grant path, so deletion never destroys customer value.

Free admission (`require_prepaid_admission = false`) quotes zero cycles, so Account does not
attach a deposit that would be credited without an admission charge. The caller must repeat the
deployment principal in `confirmation`; a typo fails closed. The
authority is the owner or governance, not the operator alone. The record moves to `Deleting` before
the first delete call and keeps `pending_canisters` for retry. Every recorded canister is stopped
and deleted; a missing canister is skipped. When the list is empty the record is `Deleted` and
`deleted_at_ns` is set. Billing grants and the deployment record are retained as audit history.

## Phase 3: grace sweep

A bounded operator/governance update `expire_deployment_suspensions(limit)` scans live suspensions,
marks those whose `grace_until_ns` has passed as `GraceExpired`, and sets
`grace_expired_at_ns`. It never deletes and is idempotent. Cancellation still works from
`GraceExpired`; deletion accepts `Suspended` or `GraceExpired`. The operator runs this from its
cron/manual sweep, then reviews the expired list before deleting.

## Deferred

- deletion order, export acknowledgement, credit settlement, and fiat refunds
- automatic deletion after expiry (the sweep only marks `GraceExpired`; a human call still deletes)
- Managed operator deletion without owner confirmation
