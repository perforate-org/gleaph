# Fiat launch decisions

Status: working checklist (2026-11-19). This is the operational memo for launching fiat credit
sales; the protocol contracts are ADR [0126](../adr/0126-operating-entity-managed-hosting-and-fiat-billing.md),
[0132](../adr/0132-provision-fee-and-reserve-withdrawal.md), and
[0133](../adr/0133-fiat-order-service-and-refund-policy.md).

## Already decided

- The operating company is the merchant/counterparty; the canister never holds fiat, card data, or
  customer identity.
- The fiat order service is outside the IC. It owns payment-provider integration, orders, and
  reconciliation.
- `order_id` is the `grant_credit` / unused `reverse_credit` idempotency key; the provider event id
  is a separate webhook dedupe key.
- The JPY→cycles rate and credit amount are snapshotted at order creation. Settlement never
  re-prices an order.
- Webhooks commit a durable intent; a worker calls Provision. A webhook never acknowledges success
  by calling the canister inline.
- Launch refund policy: only an **unused full grant** is reversed on-chain. A partially consumed
  grant is refunded in fiat only and the remaining credit stays on the deployment.
- The billing authority is a dedicated principal with KMS/HSM-held key, separate from the operator
  withdrawal identity.
- `crates/fiat-orders` implements the provider-independent state machine and the
  `PaymentGateway` / `CreditGateway` seams.
- Provision exposes `grant_credit`, `reverse_credit`, `get_deployment`, `get_service_ledger`,
  `get_withdrawal`, and `withdraw_fees`.
- The operator CLI exposes manual `billing grant`, `billing reverse`, `deployment show`,
  `treasury ledger`, `treasury withdraw`, and `treasury status` as the launch fallback.

## Must decide before taking real money

| Area | Decision | Owner / note |
| --- | --- | --- |
| Payment provider | Stripe, a Japanese PSP, bank transfer/invoice, or a combination | Provider choice determines webhook signatures and settlement files |
| Merchant account | Legal entity, bank account, card-network descriptor | Entity must match ADR 0126 |
| Pricing | JPY per cycle, rounding rule, minimum/maximum order, rate refresh source | Store the exact integer snapshot; never recompute at settlement |
| Tax / invoice | Consumption-tax treatment, invoice format, qualified-invoice registration | Accounting/legal signoff |
| KYC / AML | When identity verification is required and who performs it | Depends on provider and Japanese regulation |
| Chargebacks | Who bears the loss, how credit is frozen/reclaimed, whether a chargeback can create negative credit | Do not silently edit a grant row |
| Partial refunds | Launch stays fiat-only; a later partial on-chain reversal needs a separate debt model ADR | Do not approximate by editing the grant |
| Refund approval | Who can approve, limits, dual control, customer communication | Operator runbook |
| Key custody | KMS/HSM choice, rotation, break-glass, who can call `grant_credit` | Separate principal from treasury withdrawal |
| Service hosting | Postgres, worker runtime, region, backups, secrets, outbound access to IC | IC boundary stays outside the canister |
| Reconciliation | Daily provider settlement ↔ orders ↔ grants ↔ deployment credit; alert thresholds | Define Paid-but-not-Granted and Granted-but-not-Settled SLAs |
| Pause / kill switch | How to stop new grants, stop refunds, and drain the worker safely | Must not corrupt in-flight intents |
| Data retention | Order, webhook, and customer-data retention; deletion requests | Keep PII outside the canister |
| Support / SLA | Refund turnaround, incident communication, platform-fault compensation path | Link to the reserve policy |
| Launch gate | Sandbox provider E2E, legal/accounting signoff, runbook, on-call | No production keys before this |

## External service MVP acceptance

1. Signed webhook verification with raw-body preservation and provider-event dedupe.
2. Durable order and grant/refund intents before acknowledging the provider.
3. `CreditGateway` maps `order_id` to `grant_credit`; retries reuse the same key.
4. Unused full refund maps to `reverse_credit`; consumed refunds never call the canister.
5. Reconciliation job reports mismatches and stale `CreditGrantPending` rows.
6. Operator runbook covers manual grant, reverse, ledger inspection, and withdrawal.
7. Sandbox E2E covers payment success, duplicate webhook, grant retry, unused refund, consumed
   refund, and provider failure.

## Not in scope at launch

- Partial on-chain reversal / debt accounting.
- Storing card data or customer PII in a canister.
- SNS/DAO custody of fiat or deployment canisters.
- Automatic compensation payouts without an operator-approved incident record.
