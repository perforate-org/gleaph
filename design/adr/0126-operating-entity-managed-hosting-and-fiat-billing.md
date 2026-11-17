# 0126. Operating entity, managed hosting, and the fiat-billing boundary

Date: 2026-11-17
Status: proposed (2026-11-17). **No company, payment contract, billing service, credit ledger, or
controller-mode code exists.** This ADR records the chosen boundary so the implementation does not
bake the DAO into customer contracts or customer money.

Related: ADR [0035](0035-provision-canister-and-issuance-protocol.md) (Provision jobs, singleton
Router issuance, Relay boundary), ADR [0068](0068-account-canister-and-per-developer-router-issuance.md)
(Account and per-developer Router issuance), [stable-memory
inventory](../storage/stable-memory-inventory.md).

## Context

Gleaph is aimed at developers who do not know ICP. They should be able to buy provisioning with a
fiat card, not acquire ICP and attach cycles to a canister call. At the same time, Gleaph is expected
to become governed by an SNS, and the current Provision implementation pays every cycle from its own
balance: `create_canister_with_extra_cycles` uses Provision's balance and creates controllers
`[Provision, governance]` (`crates/provision/src/canister/mod.rs`). There is no customer billing,
credit balance, or refund accounting.

Three facts drive the boundary:

- An SNS has no legal personality. It can govern canisters and spend its treasury through proposals,
  but it cannot sign a customer contract, employ staff, hold a card-processor account, or issue a
  refund.
- Provision owns the resource lifecycle; Account owns deployment identity. Neither owns customer
  money, tax, KYC, chargebacks, or a service contract.
- Customer prepayments are a liability of whoever receives them. Mixing them with a DAO treasury
  makes the DAO the counterparty by accident.

## Problem

Who contracts with the customer, holds fiat, buys cycles, bears refund/chargeback risk, and controls
managed canisters? If the answer is "the DAO", the project takes on legal, tax, and operational
obligations that SNS governance cannot discharge and that token holders did not agree to.

## Existing Architecture Assessment

Provision's job store, singleton issuance ledger, and Account's deployment registry already own the
technical facts: which deployment exists, which Router is current, and which resources are
installed. They do **not** own a payment instrument, a customer identity, a refund obligation, or a
legal contract.

The missing capability is therefore a separate **billing and operating domain**, not another
Provision state machine. Reusing Provision for fiat balances would mix resource lifecycle with
customer money and would force every refund/chargeback policy into a canister upgrade.

The on-chain part can stay small: a per-deployment cycles credit and a controller mode. The fiat
contract, payment provider, invoices, KYC/KYB, tax, and refunds stay off-chain with the operating
company.

## Alternatives

### A. DAO is the merchant and contract party

Reject. SNS governance cannot sign contracts or hold a processor account, customer funds become DAO
treasury, and refunds/chargebacks become governance proposals. This is the failure mode this ADR
exists to avoid.

### B. One Japanese operating company now; foundation only before token/SNS

Chosen. The company is the merchant of record, employs the team, contracts with customers, holds
fiat and prepaid credits as deferred revenue, operates managed canisters, and buys cycles. The DAO
does not exist yet; when it does, it governs the protocol, not the customer relationship.

### C. Foundation + company from day one

Deferred. A Cayman foundation company (or Swiss foundation) is the common crypto structure for
protocol IP and token treasury, with the company as service provider. It is the right shape before a
token/SNS launch, but it adds formation, accounting, and intercompany agreements that are not
justified before there is a token or a treasury to hold.

### D. DAO LLC (Wyoming / Marshall Islands)

Reject as the primary contracting entity. Recognition under Japanese law and payment regulation is
uncertain, and it does not remove the need for an operating company that employs people and runs
managed hosting.

### E. One Provision canister per Account

Reject for now. It gives clean cost isolation but multiplies canisters, upgrades, monitoring, and
bootstrap complexity. A shared Provision with a per-deployment credit ledger gives the same
owner-pays economics with one service to operate.

## Decision

### Entity sequence

1. **Now:** one Japanese operating company (株式会社 or 合同会社). It is the merchant of record,
   customer counterparty, employer, and managed-service operator. The company owns the fiat account,
   processor relationship, invoices, refunds, chargebacks, and tax filings.
2. **Before a token/SNS launch:** form a foundation (Cayman foundation company or Swiss foundation)
   for protocol IP, token treasury, and the legal seat of the SNS. The foundation licenses IP to the
   company and contracts the company as service provider. The SNS governs the protocol; it does not
   become the customer counterparty.

The foundation is created **before** the token/SNS launch, not after: moving IP, token allocation, and
customer contracts after launch is more expensive and less credible.

### Customer and money boundary

- The operating company is the only party that receives fiat. Customer funds never enter the DAO
  treasury.
- Prepaid credits are deferred revenue of the company. Paid, unused credits are refundable under the
  company's published policy; consumed cycles are not. Promotional credits are non-refundable and may
  expire. A chargeback freezes the credit until resolved.
- The company buys cycles and funds Provision's `DeploymentCredit`. Only a billing-service principal
  can grant credit; the grant is idempotent and audited. Provision never mints credit for an
  arbitrary caller.

### Payment rails

```text
PaymentRail = Cycles | Fiat
```

- `Cycles`: an ICP-native owner attaches cycles and pays Provision directly. Self-custody is
  available.
- `Fiat`: the customer pays the company; the company grants a cycles credit to that deployment.
  Managed hosting is the default.

`BillingAccount` is separate from the deployment owner: an organization may pay for deployments owned
by its members.

### Controller mode

```text
ControllerMode = Managed | SelfCustody
```

- **Managed (default for fiat):** provisioned canisters are controlled by `[Provision, Operator]`,
  where `Operator` is a principal controlled by the operating company (multisig or operator
  canister). The customer owns the data and the service contract; the company owns day-to-day
  canister operation.
- **SelfCustody:** controlled by `[Provision, Owner]`. The owner accepts ICP operational
  responsibility. Moving from Managed to SelfCustody is an explicit operation that adds the owner as
  controller and records the new mode.
- The SNS/DAO is **never** a controller of an individual deployment's canisters. It controls the
  platform canisters (Provision, Account) through SNS root.

### Pricing and policy

`ServiceConfig` owns the mutable commercial parameters:

```text
platform_fee_bps, reserve_bps, compensation_cap, covered_failure_classes, version
```

- Fees and compensation policy are configuration, not code constants.
- A job records the `pricing_version` used at acceptance; later changes apply only to new jobs.
- Compensation, if offered, is funded from a separate reserve and covers only the published failure
  classes; the default is no compensation for owner input, owner key loss, Account fault, or
  infrastructure incidents.
- Fee and policy changes are prospective only. Historical charges and promised payouts are not
  retroactively rewritten.

### Governance

- Before SNS: the operating company's board/operator changes `ServiceConfig` and canister code.
- After SNS: code upgrades, fee/policy changes, and treasury spending go through SNS proposals.
  Customer contracts and fiat remain with the company.
- SNS treasury is not used for customer refunds or customer credits.

## Consequences

- A non-ICP developer can pay with a card and never touch cycles.
- The company, not the DAO, is the legal counterparty and bears refund/chargeback/tax obligations.
- Customer money and protocol treasury are structurally separate.
- Managed hosting has a clear controller and a documented migration to self-custody.
- The on-chain surface stays small: a credit ledger, a controller mode, and a service config; fiat
  complexity stays off-chain.

## Trade-offs

- The company is a centralized operator for managed deployments. That is the product, not an
  accident; it must be disclosed.
- The foundation adds cost and intercompany agreements; it is deferred until a token/SNS makes it
  necessary.
- Prepaid credits are a liability and require deferred-revenue accounting.
- Controller migration from Managed to SelfCustody needs a tested handover, not just a flag.
- The fee/reserve policy is deliberately changeable, which creates a trust obligation to announce
  changes and avoid retroactive application.

## Migration

- Pre-SNS: fresh state. The credit ledger, controller mode, and `ServiceConfig` can be added to
  Provision/Account without a compatibility decoder.
- Post-SNS: a new stable map or field is added by canister upgrade. Empty new regions need no data
  migration; existing deployments keep their current mode and zero credit until funded.
- No customer fiat data is ever written into Provision; only the resulting cycles credit is.
- Legal/tax review must precede the first fiat payment.

## Design Documentation Impact

- ADR 0035: link the operating-company/credit boundary from the Provision issuance section.
- ADR 0068: link Managed/SelfCustody controller modes from Account/router issuance.
- `design/storage/stable-memory-inventory.md`: add the credit ledger and service config when they
  exist.
- Add an operations/billing document for refunds, chargebacks, deferred revenue, and processor
  webhooks before fiat goes live.
- `design/adr/README.md`: index this ADR.

## Open Questions

- Exact Japanese entity form and jurisdiction; legal/tax review.
- Payment processor and merchant-of-record choice.
- Refund window and whether paid credits expire.
- `Operator` principal: multisig, operator canister, or both.
- Foundation jurisdiction (Cayman vs Swiss) and timing relative to the SNS swap.
- Whether managed customers can export and migrate to self-custody without operator consent.
