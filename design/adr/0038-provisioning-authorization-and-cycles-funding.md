# 0038. Provisioning authorization and cycles funding

Date: 2026-07-04
Status: proposed
Last revised: 2026-07-04
Anchor timestamp: 2026-07-04 13:05:02 UTC +0000

## Context

Provision needs permission to create resources and enough cycles to attach to management-canister
creation calls. Router already owns user identity and per-graph RBAC. Provision must not duplicate
that policy, accept arbitrary user calls, or maintain an accounting total that can diverge from its
actual canister cycle balance.

Billing, quotas, and charging users are product concerns distinct from resource admission and cycle
reservation.

## Decision

### Initial authorization contract

The first implementation is deliberately narrow:

- only a Router `Admin` may request a new logical graph;
- an existing graph's owner/admin may request an allowed expansion or deletion;
- Router resolves that authorization and forwards the resolved ADR 0035 envelope; and
- Provision accepts it only from the registered Router principal for the deployment.

Provision records the asserted caller for audit but never reads Router tenancy or re-evaluates user
roles. Broader self-service graph creation requires a later Router policy decision.

### Cycle source and reservation algebra

Provision uses its actual canister cycle balance as the only treasury total. Platform/governance
top-ups fund that balance; callers and Router do not repay Provision.

Provision stores only outstanding reservations:

```text
reserved = sum(active reservation amounts)
available = observed_canister_balance - reserved
```

Before accepting a create step, Provision estimates a bounded allocation, verifies
`available >= requested_reservation`, and durably records:

```text
CycleReservation = {
  reservation_id, request_id, amount,
  lease_generation, job_state, expires_at_ns
}
```

Each reservation covers one concrete `create_canister` step. The call explicitly attaches
`amount` cycles from Provision's balance. On confirmed success, that exact amount is removed from
the active-reservation sum because it has already left the observed balance; on confirmed failure
with no spend, the same amount is released. Multi-canister jobs hold one reservation per pending
create step. No second treasury variable subtracts actual spend.

`expires_at_ns` is only the time after which a reconciler may attempt takeover; it is not an automatic
TTL release. The reconciler may release a reservation only after acquiring a newer lease generation
and proving from the durable ADR 0035 job state that no create/install call is in flight and no
created canister remains unaccounted for. Ambiguous outcomes remain reserved and enter reconciliation.

The pre-create check prevents concurrent overcommit; it does **not** make multi-canister issuance
atomic or prevent partial creation. ADR 0035 records partial progress and ADR 0037 cleans up resources
that cannot be registered.

### Caller-controlled policy

The public request does not accept artifact ids, controller lists, raw install settings, or arbitrary
cycle amounts. Provision obtains release, controller, and allocation policy from deployment-owned
configuration and the accepted ADRs.

Billing, invoicing, quotas, and cost attribution are outside this ADR. They may fund the platform in
the future but do not add a Router repayment obligation or mutate issuance receipts.

## Ownership and invariants

| Invariant | Enforcer |
|---|---|
| Router is the only user-authorization decision point. | Router RBAC and resolved-envelope construction |
| Provision accepts only its deployment's registered Router. | Provision ingress validation |
| Available cycles equal observed balance minus durable active reservations. | Provision reservation boundary |
| A reservation cannot expire while its external operation may be in flight. | Lease generation, job-state fence, reconciler |
| Actual spend is never double-subtracted. | Observed balance plus reservation removal |
| Callers cannot override artifact, controller, or allocation policy. | Provision request schema and validation |
| Billing is separate and Router owes no repayment. | Absence of repayment/accounting API |

## Alternatives

- **Caller-attached cycles:** rejected for the managed path because it exposes low-level allocation
  mechanics and couples admission to payment.
- **Router pays or repays Provision:** rejected because it creates cross-canister debt and duplicate
  accounting.
- **TTL-only reservation reclamation:** rejected because an external create may still be in flight.
- **Unspecified eligible-user creation:** rejected for the initial contract; Admin-only is explicit.

## Consequences

The algebra has one actual balance and one durable reservation aggregate. Low balance blocks new
work, while ambiguous operations remain safely reserved until reconciled. Broader admission and
billing remain separable future decisions.

## Implementation status

**Proposed.** Provision authorization, reservations, lease fencing, and cycle reconciliation are not
implemented.

## Cross-links

- [ADR 0028](0028-per-graph-tenancy-metadata-reads.md) — Router tenancy and admin policy.
- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — durable jobs and resolved envelope.
- [ADR 0036](0036-versioned-wasm-artifact-catalog.md) — deployment-owned release selection.
- [ADR 0037](0037-provisioned-canister-lifecycle-and-controllers.md) — partial-resource cleanup.
