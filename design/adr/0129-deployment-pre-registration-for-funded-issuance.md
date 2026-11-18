# 0129. Pre-register a deployment before funded issuance

Date: 2026-11-18
Status: accepted (implemented).

Related: [0126](0126-operating-entity-managed-hosting-and-fiat-billing.md),
[0127](0127-provision-deployment-record-and-service-config.md),
[0128](0128-billing-credit-grants.md).

## Context

`grant_credit` requires a `DeploymentRecord`, but the record was created by the first
`accept_envelope`; a paid admission also requires credit. Fiat therefore had a circular dependency:

1. no record -> `grant_credit` returns `UnknownDeployment`;
2. no grant -> the first paid admission cannot reserve its charge.

Attached cycles let the company pay for the first job, but that is a workaround, not the fiat model.

## Decision

Add `initialize_deployment`:

```candid
type InitializeDeploymentArgs = record {
  deployment_id : principal;
  owner : principal;
  controller_mode : ControllerMode;
};
initialize_deployment : (InitializeDeploymentArgs) -> (Result_8);
```

- Creates a `DeploymentRecord` with zero credit.
- Idempotent only for an exact `(owner, controller_mode, operator)` match; any divergence returns
  `OwnerMismatch`, `ControllerModeMismatch`, or `OperatorMismatch`.
- `Managed` requires `ServiceConfig.operator`; a missing operator is `MissingOperator`.
- Anonymous owner is rejected.
- Authorization: the owner itself, a `Relay` grant holder, or an `OwnDeployment` issuer naming its
  own deployment principal.
- No credit is granted and no controller is changed. The caller then uses `grant_credit` and the
  first `accept_envelope` reserves against that credit.

The record is the same MemoryId 17 row; no new stable region is introduced.

## Consequences

- The fiat order becomes initialize -> grant -> accept.
- A pre-registered record with zero credit is visible as "configured but unfunded"; a paid
  admission fails with `InsufficientCycles` until the grant lands.
- Relay can initialize for an owner, matching its existing trust boundary; it cannot change an
  existing record's owner or mode.
- Tests cover owner, Relay and unauthorized callers, idempotency, owner/mode conflicts, Managed
  operator requirement, and grant-after-initialize.
