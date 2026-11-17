# 0127. Provision deployment record and service config

Date: 2026-11-17
Status: accepted (contract). Types, MemoryIds 17/18, first-issuance record creation, Managed operator
snapshot, SelfCustody owner, the `[Provision, deployment controller]` set, store-level checked
credit/debit, the governance-only `admin_set_service_config` update, and the per-job
`service_config_version` snapshot are implemented. The cycles funding rail (attached cycles or
billing grant), fee settlement, and the `take_ownership` transition remain unimplemented; no fiat
rail is included.

Related: [0126](0126-operating-entity-managed-hosting-and-fiat-billing.md) (operating entity and
fiat boundary), [0035](0035-provision-canister-and-issuance-protocol.md) (jobs and singleton Router
issuance), [0068](0068-account-canister-and-per-developer-router-issuance.md) (Account).

## Context

ADR 0126 separates the customer contract (operating company) from the protocol. The on-chain part
that must exist before the SNS is small:

- a durable deployment setting: who owns it, whether it is managed, and who operates it;
- a durable cycles credit per deployment;
- a versioned service configuration for fees and compensation policy.

These are deployment-scoped facts, not job-scoped facts. A replacement Router, a second resource, or
a retry must see the same mode, operator, owner and credit. They cannot live only in
`ProvisionJobRecord`.

## Decision

Add two Provision-owned contracts and no fiat code.

### `ControllerMode`

```rust
pub enum ControllerMode {
    Managed,
    SelfCustody,
}
```

- `Managed` is the product default for fiat customers; the request must explicitly ask for it.
  Direct ICP customers default to `SelfCustody`.
- `Managed`: the provisioned canister controllers are `[Provision, operator]`, where `operator` is
  the platform operator snapshotted from `ServiceConfig` at issuance.
- `SelfCustody`: controllers are `[Provision, owner]`, where `owner` is the deployment owner.
- The SNS/DAO is never a controller of an individual deployment.

### `DeploymentRecord` (MemoryId 17)

Key: `DeploymentId`.

```rust
pub struct DeploymentRecord {
    pub owner: Principal,
    pub controller_mode: ControllerMode,
    pub operator: Option<Principal>,
    pub credit_cycles: u128,
}
```

Invariants:

1. `owner` is non-anonymous and is the `authorized_caller` of the first accepted issuance.
2. `Managed` requires `operator = Some(non-anonymous)`; `SelfCustody` requires `operator = None`.
3. `operator` is snapshotted from `ServiceConfig` at first issuance. A later `ServiceConfig` change
   does not rewrite an existing deployment's controller; changing it requires the explicit
   controller-mode operation.
4. `credit_cycles` never underflows. A debit is checked; an insufficient balance rejects the
   admission before any resource mutation.
5. A missing record means "not configured". The first accepted issuance creates it. A missing
   record is not treated as a zero-credit record for an existing job.
6. `owner` changes only through a deliberate Account/Relay operation; a job cannot silently replace
   it.

### `ServiceConfig` (MemoryId 18, `StableCell`)

```rust
pub struct ServiceConfig {
    pub version: u32,
    pub platform_fee_bps: u16,
    pub reserve_bps: u16,
    pub compensation_cap_cycles: u128,
    pub covered_failure_classes: u32,
    pub operator: Option<Principal>,
}
```

Initial value: `version = 1`, all rates zero, no compensation classes, `operator = None`.

- `platform_fee_bps + reserve_bps <= 10_000`.
- A `Managed` request is rejected while `operator = None`.
- `ServiceConfig` is changed only by the governance principal before the SNS and by SNS proposals
  afterwards. The exported `admin_set_service_config` requires a strictly increasing `version`.
- A job records the `version` it accepted under (`service_config_version`); changes apply only to
  new jobs.
- Fees are not charged in this slice; the fields are contract placeholders so the SNS does not need
  a layout change to introduce them.

### Request additions

`ProvisionRequest` gains:

```rust
pub controller_mode: ControllerMode,
```

- A `Managed` request must have `ServiceConfig.operator = Some`; otherwise reject.
- A `SelfCustody` request uses `authorized_caller` as the owner and `[Provision, owner]` as the
  controllers.
- No `operator` field is accepted from the caller: the platform operator is service policy, not a
  per-request choice.

## Write Paths

- First accepted issuance: create `DeploymentRecord` from `(authorized_caller, controller_mode,
  ServiceConfig.operator)`, with zero credit unless cycles are attached.
- Credit top-up: a billing principal (or attached cycles) adds to `credit_cycles`; idempotent and
  audited. This slice defines the field only; the funding path is the next slice.
- Debit: reserve the admission cost before side effects, then settle after the terminal state.
- Controller change (later slice): `take_ownership` sets `SelfCustody`, clears `operator`, and adds
  the owner controller in one operation. `Managed` from `SelfCustody` is not offered implicitly; it
  would require an explicit service contract and controller handover.

## Data Cost

- `DeploymentRecord`: one row per deployment, about 80 bytes (owner 29 + mode 1 + operator 29 +
  credit 16 + encoding overhead).
- `ServiceConfig`: one cell, under 100 bytes.
- No per-job duplication of owner/mode/operator.

## Migration

- Pre-SNS: fresh state; MemoryIds 17 and 18 are initialized empty.
- Post-SNS: a new region is added by upgrade. Empty regions need no data migration. Existing
  deployments without a record are treated as "not configured"; the first operation that needs the
  record must create it, and the implementation must not infer credit from absence.
- No fiat customer data is written on-chain; only the cycles credit is.

## Validation

- Store tests: create defaults, Managed requires operator, SelfCustody operator is None, credit
  checked add/sub, record survives reopen, missing record is not zero credit.
- Provision tests: first issuance creates the record; Managed with no operator rejects; controller
  list uses `[Provision, operator]` / `[Provision, owner]`.
- Workspace and wasm checks.
