# 0133. Fiat order service and refund policy

Date: 2026-11-19
Status: accepted (2026-11-19; provider-independent core implemented in `crates/fiat-orders`; external service not implemented).

Related: [0126](0126-operating-entity-managed-hosting-and-fiat-billing.md) (operating company as
merchant), [0128](0128-billing-credit-grants.md) (grant), [0130](0130-billing-grant-reversals.md)
(full reversal), [0132](0132-provision-fee-and-reserve-withdrawal.md) (operator treasury).

## Context

Non-ICP developers pay fiat; the operating company is the merchant of record and holds the fiat
relationship. Provision only holds prepaid cycles credit. A fiat order is therefore an external
business record plus one idempotent `grant_credit` call, not a payment rail inside the canister.

The current `reverse_credit` contract is full-amount only and succeeds only while the deployment
still holds the full grant. A launch policy is needed before the order state machine is fixed.

## Decision

### Refund policy

Launch supports **full refund of an unused grant only**:

- unused grant → `reverse_credit(original grant authority, original idempotency key)`;
- partially or fully consumed grant → no on-chain reversal; the operating company handles the
  fiat refund off-chain and the remaining credit stays on the deployment;
- the order service records the consumed amount and the refund reason, so accounting can explain
  why the fiat refund and the on-chain credit differ.

Partial on-chain reversal is deliberately out of scope. Adding it later requires a separate ADR
covering the debt/consumed-credit model; it must not be approximated by editing the grant row.

### External order service

The service is outside the IC and owns the payment-provider integration and order database.

Order states:

```text
Draft -> PendingPayment -> Paid -> CreditGrantPending -> CreditGranted -> Fulfilled
                                                    \-> CreditGrantFailed
RefundPending -> Refunded | RefundRejected
```

Invariants:

1. `order_id` is the single idempotency key for `grant_credit` and for an unused `reverse_credit`.
   The provider event id is a separate dedupe key for webhook delivery.
2. The JPY→cycles rate and the credit amount are snapshotted when the order is created. Payment
   settlement never re-prices an order.
3. A verified webhook first commits `Paid` (or `RefundPending`) and a durable grant/refund intent.
   It does not call Provision inline. A worker drives the intent and records the exact canister
   verdict; retries reuse `order_id`.
4. The `billing_authority` is a dedicated principal whose key is held in KMS/HSM, not the operator
   withdrawal identity. The canister stores no card data, customer identity, or fiat amount.
5. A `CreditGranted` order with a missing provider settlement, or a `Paid` order whose grant stays
   pending past the operational bound, is an alert. Reconciliation compares provider settlement,
   order rows, grant rows, and the deployment credit.
6. Refunds: the service attempts `reverse_credit` only for an unused full grant. Any other refund
   is fiat-only and leaves the credit intact.

### Provision read surface

The order service needs deployment credit for reconciliation. Provision exposes a deployment read
owned by the deployment/operator/governance boundary:

```candid
get_deployment : (DeploymentId) -> (Result<DeploymentRecord, ...>) query;
get_deployment_canisters : (DeploymentId) -> (Result<vec DeploymentCanister, ...>) query;
deployment_jobs : (DeploymentId) -> (Result<vec DeploymentJobSummary, ...>) query;
deployment_job : (DeploymentId, RequestId) -> (Result<opt ProvisionJobView, ...>) query;
repair_failed_deployment_canisters : (DeploymentId) -> (Result<vec FailedCanisterRepair, ...>);
```

Implemented: `get_deployment` accepts `DeploymentId` and returns `DeploymentRecord` to the owner,
operator, current billing authority, or governance. `get_deployment_canisters` uses the same
authority and returns every recorded canister with an `active` flag (false means only a Failed
job references it). `deployment_jobs` uses the same read authority and returns one summary per
canonical job (state, active resource, effect count, graph, timestamps) oldest first.
`repair_failed_deployment_canisters` is operator/governance only: it re-scans
the deployment, deletes only still-inactive Failed-job canisters, skips a canister that became
active, and reports `Deleted` / `AlreadyAbsent` / `SkippedActive` / `Failed`. Public reads are
queries only; repair is an update.

## Operator fallback

Before the order service ships, `gleaph-operator` provides the same credit operations for manual
bank-transfer/invoice handling: `billing grant`, `billing reverse`, `deployment show`, `deployment jobs`,
`deployment reconcile`, and `deployment repair`. Reconcile compares active canisters with `canister_status`
(missing/stopped/controller mismatch makes it exit non-zero); `--include-failed` also reports
canisters left by Failed jobs. Repair is the only mutating command: preview by default, `--yes`
deletes Failed-job canisters through Provision. They use the same idempotency keys where money is
involved and do not create an order record.

## Consequences

- Fiat failures cannot corrupt credit: the canister only sees idempotent credit grants/reversals.
- Partial refunds are an accounting operation, not a credit mutation; this avoids a second debt
  ledger in Provision before launch.
- The order service must be durable before it acknowledges a payment webhook.
- A future partial-reversal ADR can reuse the same order identity and reconciliation table.

## Test coverage

Canister side: `get_deployment` authorization and credit read is covered by the Provision handler
tests and the paid issuance E2E; grant/reversal idempotency is already covered by ADR 0128/0130
tests. Service side is not in this repository; its contract tests must
cover webhook signature/replay, order state transitions, grant worker retry, unused-only refund,
and reconciliation alerts before connecting a production provider.
