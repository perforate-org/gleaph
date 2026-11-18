# 0131. Provision funding integration: quote then attach cycles

Date: 2026-11-18
Status: accepted (2026-11-18). No fiat webhook or payment-provider code.

Related: [0127](0127-provision-deployment-record-and-service-config.md) (contract and charge),
[0128](0128-billing-credit-grants.md) (pre-funded credit), [0129](0129-deployment-pre-registration-for-funded-issuance.md).

## Context

`accept_envelope` enforces prepaid credit. Account and Router called it without attaching cycles or
pre-funding a deployment, so their first issuance would fail with `InsufficientCycles`. The caller
must not re-derive the cost/fee/reserve formula; that formula is owned by Provision.

## Decision

Provision exports a public query:

```candid
quote_admission : (nat32) -> (Result_9) query;
```

`AdmissionQuote` carries `resource_count`, `cost`, `fee`, `reserve`, `total`, and the
`service_config_version` used. Arithmetic failures return `ArithmeticOverflow`.

Account and Router:

1. query `quote_admission` with the number of requested resources,
2. attach `quote.total` cycles to `accept_envelope`,
3. let Provision credit the attached cycles and reserve the admission.

The accept path recomputes the charge from the then-current config. A config change between quote
and accept can therefore reject with `InsufficientCycles`; the caller retries with a fresh quote.
Extra attached cycles remain as deployment credit.

Fiat deployments pre-fund instead: `initialize_deployment` → `grant_credit` → `accept_envelope`.
They do not attach cycles.

## Consequences

- Account/Router do not duplicate the pricing formula.
- First issuance pays exactly the current quote; no unpriced internal path remains.
- Callers need cycles in their own balance. Account/Router funding is an operational concern.
- A config change during the quote/accept window is a retry, not a silent undercharge.

## Validation

Quote tests cover the fee/reserve split and config version. Account/Router compile and existing flow
tests use the sender abstraction; PocketIC end-to-end remains blocked by the unrelated
`Parameter.resolver` gap.
