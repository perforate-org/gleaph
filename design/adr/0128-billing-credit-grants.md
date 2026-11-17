# 0128. Billing credit grants

Date: 2026-11-18
Status: accepted (implemented). No fiat webhook, payment provider, or cycle transfer is included.

Related: [0126](0126-operating-entity-managed-hosting-and-fiat-billing.md) (operating company and
customer-money boundary), [0127](0127-provision-deployment-record-and-service-config.md)
(`DeploymentRecord.credit_cycles` and `ServiceConfig`), [0035](0035-provision-canister-and-issuance-protocol.md).

## Context

ADR 0126 makes the operating company the fiat merchant and keeps customer money off-chain.
Provision only needs the resulting cycles credit. Attached cycles cover an ICP-native owner, but a
fiat customer pays the company, not Provision.

The company therefore needs one narrow on-chain capability: turn a confirmed external payment into
a credit for a deployment, exactly once.

## Decision

### Billing authority

`ServiceConfig.billing_authority: Option<Principal>` is the only principal allowed to grant credit.
`None` disables grants. Governance sets it through `admin_set_service_config`; an anonymous
authority is rejected. The field is versioned with the rest of the policy.

### API

```candid
grant_credit : (GrantCreditArgs) -> (Result_7);

type GrantCreditArgs = record {
  deployment_id : principal;
  amount : nat;
  idempotency_key : blob;
};

type GrantCreditResult = record {
  deployment_id : principal;
  amount : nat;
  credit_cycles : nat;
  replayed : bool;
};
```

The `idempotency_key` is the external payment identifier. Provision keys the durable grant by
`(billing_authority, idempotency_key)`, so two authorities cannot collide and a key cannot be reused
with a different payload.

### Semantics

- Caller must equal `billing_authority`; otherwise `Unauthorized`.
- No configured authority is `BillingAuthorityNotConfigured`.
- `amount = 0` is `AmountZero`.
- Unknown deployment is `UnknownDeployment`.
- Exact replay `(same deployment, same amount)` returns the current credit with `replayed = true`
  and does not add again.
- Same key with a different deployment or amount is `IdempotencyConflict`.
- Credit addition is checked; overflow is `CreditOverflow`.
- The grant is a ledger credit only. It does not accept or transfer cycles; the company has already
  received the fiat off-chain and is responsible for backing the credit.

### Storage

`BILLING_GRANTS` (MemoryId 20): `(authority, idempotency_key) -> BillingGrantRecord { deployment_id,
amount, credited_at_ns }`.

The grant row and the `credit_cycles` update are validated before either is written. No await occurs
between them.

## Alternatives

- **Attached cycles only.** Rejected for fiat customers; they should not need ICP.
- **Off-chain credit balance.** Rejected: provisioning must be able to check and debit a durable
  on-chain credit.
- **Grant without idempotency.** Rejected: payment webhooks retry, and a duplicate credit is a real
  loss.
- **Governance grants per payment.** Rejected: too slow and operationally noisy.
- **Accept fiat on-chain.** Rejected: no customer fiat on Provision; the company is the merchant.

## Consequences

- The company can map a confirmed payment to a deployment credit once.
- Billing retries are safe.
- Key reuse with a different payload is visible and rejected.
- The credit is a company liability until consumed; consumed cycles are not refundable.
- Billing authority key management is an operational responsibility (multisig or service canister).

## Migration

Pre-SNS fresh state. MemoryId 20 is initialized empty; no decoder or migration. A grant record is
never removed; failed customer actions do not delete the payment history.

## Design Documentation Impact

- ADR 0127: link this grant boundary from `ServiceConfig`.
- `stable-memory-inventory.md`: add MemoryId 20.
- `stable_layout.rs`: add the region and baseline count.
- Add a billing operations document for webhook signature verification, retry policy, key
  generation, and reconciliation before fiat goes live.

## Open Questions

- Whether the billing authority is a single service canister or a multisig.
- Whether grants can be reversed for chargebacks (current answer: no; handle off-chain and grant a
  compensating credit only through a new external id).
- How the company proves that a credit is backed by cycles (operational reconciliation).
