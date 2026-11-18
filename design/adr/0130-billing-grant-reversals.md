# 0130. Full billing-grant reversals

Date: 2026-11-18
Status: accepted (2026-11-18). No partial reversals, debt accounting, fiat webhook, or payment
provider code.

Related: [0126](0126-operating-entity-managed-hosting-and-fiat-billing.md) (fiat boundary),
[0128](0128-billing-credit-grants.md) (grants).

## Context

A fiat payment can be refunded or charged back after `grant_credit`. The on-chain credit is pooled
per deployment, so there is no durable attribution of which grant funded which consumed cycles.
Without a reversal path, a refunded payment would leave spendable credit behind.

## Decision

`reverse_credit(grant_authority, idempotency_key)` is callable only by the **current**
`ServiceConfig.billing_authority`.

- It looks up the original grant by `(grant_authority, idempotency_key)`. Specifying the original
  authority lets a rotated billing authority reverse grants made before rotation.
- Reversal is **full amount only**. It succeeds only when the deployment still holds at least the
  grant amount; then it debits that amount and sets `reversed_at_ns` on the grant.
- A second reversal is an idempotent replay (`replayed = true`, no debit).
- If the credit is insufficient, the reversal fails with `InsufficientCredit` and the grant is not
  marked reversed. The operating company handles the shortfall off-chain.
- `AnonymousGrantAuthority` is rejected; an unknown grant is `UnknownGrant`.
- Partial refunds are deliberately not supported: pooled credit has no per-grant consumption
  history, and a partial debit would invent an allocation rule.

No fee/reserve ledger is touched: grants are pure credit, not admission charges.

## Consequences

- Unused fiat credit can be clawed back exactly once.
- Consumed credit cannot be reversed on-chain; the company carries the receivable/chargeback.
- A billing-authority rotation does not orphan old grants.
- Refund UI and webhook reconciliation remain off-chain.

## Validation

Store tests cover grant → reversal, idempotent replay, insufficient credit, unknown grant, and
credit overflow. Endpoint tests cover current-authority authorization, rotation with the original
`grant_authority`, and anonymous rejection. Candid is updated.
