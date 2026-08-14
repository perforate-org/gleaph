# Account canister and per-developer Router issuance

## Purpose

Define the **Account** canister and the end-to-end flow from developer registration to
per-developer Router issuance and CLI resolution.

## Status

**Planned.** Not implemented. The design decisions are fixed in
[ADR 0068](../adr/0068-account-canister-and-per-developer-router-issuance.md), which is the
**single source of truth** for the account model. This document is an overview only; do not restate
the ADR's contracts here.

## Non-goals

- Service-scope top-level administration (a separate concern).
- Governance (SNS) account kind — reserved as an extension point in ADR 0068.
- Router-internal RBAC (`Role::Executor..Admin`), owned by `crates/auth` / Router.
- Graph topology and tenancy (`owner`/`admins`), owned by the issued Router.

## Overview

A developer (or organization) owns a **Router** (and its graphs/shard/index canisters) within a
multi-developer service. The account is the ownership boundary. The model is an `enum`
(`Personal`, `Org`; governance reserved), the account maps 1:1 to a `deployment_id`, and the
first Router is issued via a **bootstrap trust handover**: Account is the issuance authority only
for the first Router, after which the issued Router owns the deployment binding under
[ADR 0035](../adr/0035-provision-canister-and-issuance-protocol.md).

**Detailed contract:** [ADR 0068](../adr/0068-account-canister-and-per-developer-router-issuance.md).

## Platform vs user layers

Two layers consume different tools; an end user never touches `icp.yaml` or the IC subnet.

| Layer      | Who                          | Uses                               | Deploys                                         |
| ---------- | ---------------------------- | ---------------------------------- | ----------------------------------------------- |
| Platform   | Gleaph operator              | `icp.yaml` + `icp-cli`             | Provision, Account, reference Router/Graph code |
| User       | Gleaph developer (end user)  | `gleaph` CLI only                  | their own Router / Graph / index canisters      |

User canisters are issued by **Provision** from the artifact catalog
([ADR 0036](../adr/0036-versioned-wasm-artifact-catalog.md)). The user does not build wasm, manage
subnets, or run `icp deploy`.

### `gleaph deploy` (user workflow)

`gleaph deploy` (no flags) defaults to the **`local`** environment:

1. Ensure a local IC network (start it behind the scenes if absent).
2. Ensure the platform canisters (Account / Provision) exist; provision them if not.
3. Issue the user's Router + graph + first shard through Provision
   (Account `authorize_router_issuance` → Provision).
4. Generate/update `.gleaph/data/mappings/local.ids.json`.
5. Set up the initial graph / schema as needed.

### Network resolution (delegation to icp-cli)

`gleaph cli` depends on `icp-cli`. Network resolution is **delegated, not parsed**:

- If an `icp.yaml` is present, its **`networks:`** definitions are reused as connection targets by
  default; `icp network status --json` provides the authoritative `api_url` / `gateway_url`.
- Gleaph does **not** parse `icp.yaml`'s schema (no version-following fragility).
- `icp.yaml` `environments:` are not adopted; a Gleaph environment is owned by `gleaph.toml`.
- When there is no `icp.yaml`, `gleaph deploy` starts an **icp-cli managed local network** behind
  the scenes.

## CLI configuration

See [ADR 0062 Amendment](../adr/0062-gleaph-toml-project-configuration.md) (planned) for the
`gleaph.toml` / identity / environment / `.gleaph/` details. Summary:

- `account` is not stored; the caller principal resolves it at runtime.
- `identity` is a **name** (keyring default), delegated to icp-cli when an `icp.yaml` is present.
- `environment` vs `network` are separated (`-e` / `-n`).
- `.gleaph/data/mappings/<env>.ids.json` (committed, platform-fixed ids) and
  `.gleaph/cache/account/<env>.router.json` (gitignored, per-user Router id).
- `GLEAPH_CANISTER` is removed.

## Related documents

- [ADR 0068](../adr/0068-account-canister-and-per-developer-router-issuance.md) — account model (SSOT).
- [ADR 0035](../adr/0035-provision-canister-and-issuance-protocol.md) — issuance protocol (Amendment planned for Account trust subject).
- [ADR 0054](../adr/0054-provisioned-logical-graph-topology-and-resource-activation.md) — bootstrap resource selection and topology.
- [ADR 0062](../adr/0062-gleaph-toml-project-configuration.md) — `gleaph.toml` (Amendment planned).
