# Account canister and per-developer Router issuance

## Purpose

Define the **Account** canister and the end-to-end flow from developer registration to
per-developer Router issuance and CLI resolution.

## Status

**Partially implemented.** The Account canister exists. The **Provision artifact-catalog issuance**
described below (`LogicalResource::Router` via `accept_envelope`) is **planned**, not implemented.
Per the **lazy Router issuance** design (amended in
[ADR 0068](../adr/0068-account-canister-and-per-developer-router-issuance.md), the single source of
truth), Router issuance is triggered **on demand** by the first operation that needs a Router, and
`gleaph deploy` is removed from the CLI surface. This document is an overview only; do not restate
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

### Lazy Router issuance (replaces `gleaph deploy`)

Router issuance is **on demand**, driven by the first operation that needs a Router. There is no
`gleaph deploy` step; a fresh developer workflow is:

1. Ensure a local IC network is running and the platform canisters (Account / Provision) exist;
   `gleaph network start` deploys them and by default **auto-registers** the caller's Personal
   account (a flag disables auto-registration).
2. Any command that needs a Router (`migration apply`, `load`, `prepared`, `codegen`, GQL) resolves
   the Router id: cache → `Account.resolve_router` → auto-issue via
   `Account.authorize_router_issuance` → Provisioner `accept_envelope` with
   `LogicalResource::Router` → `Account.register_router` → cache.
3. The issued Router carries `provision_canister: Some(provision)`, so graph/shard/index/vector
   provisioning (ADR 0070 / ADR 0071) flows through Provision, not a manual management-canister
   install.

The **Provision artifact-catalog issuance** path (`LogicalResource::Router` and `accept_envelope`)
is the lazy issuance mechanism; the CLI no longer installs canisters directly via the management
canister.

### Network resolution (delegation to icp-cli)

`gleaph cli` depends on `icp-cli`. Network resolution is **delegated, not parsed**:

- If an `icp.yaml` is present, its **`networks:`** definitions are reused as connection targets by
  default; `icp network status --json` provides the authoritative `api_url` / `gateway_url`.
- Gleaph does **not** parse `icp.yaml`'s schema (no version-following fragility).
- `icp.yaml` `environments:` are not adopted; a Gleaph environment is owned by `gleaph.toml`.
- When there is no `icp.yaml`, `gleaph network start` starts an **icp-cli managed local network**
  behind the scenes.

## CLI configuration

See [ADR 0062 Amendment](../adr/0062-gleaph-toml-project-configuration.md) (planned) for the
`gleaph.toml` / identity / environment / `.gleaph/` details. Summary:

- `account` is not stored; the caller principal resolves it at runtime.
- `identity` is a **name** (keyring default), delegated to icp-cli when an `icp.yaml` is present.
- `environment` vs `network` are separated (`-e` / `-n`).
- `.gleaph/data/mappings/<env>.ids.json` (committed, platform-fixed ids) and
  `.gleaph/cache/account/<env>.router.json` (gitignored, per-user Router id).
- `GLEAPH_CANISTER` is removed.

## Authentication: `gleaph login` / `gleaph signup`

Account access is gated by the caller's **principal**; the RBAC is principal-based and
authentication-method-agnostic. Three paths converge on "resolve a principal":

| Path                     | Who          | Principal origin                                      |
| ------------------------ | ------------ | ----------------------------------------------------- |
| Web UI login             | end user     | Internet Identity app-specific principal (`gleaph.com`) |
| `gleaph login`           | CLI + browser| Internet Identity delegation via `icp identity link web` |
| Local PEM/keyring        | local dev    | principal derived from a secret key                    |

The web UI is served at `gleaph.com`. `/login/` hosts the sign-in entry (redirecting to Internet
Identity / `id.ai`), and `/account/` hosts the authenticated account-management screen (profile,
members, Routers). The principal is derived from the bare domain `gleaph.com` (no path), so
`/login/` and `/account/` share the same principal and Account.

`gleaph login` and `gleaph signup` are **separate commands**, sharing a common auth layer:

- **`gleaph login`** — authentication only. Resolves the caller's principal (delegating the
  browser/II flow to `icp identity link web`, or reading a local PEM/keyring identity) and stores
  it as the active session. Read-only and idempotent.
- **`gleaph signup`** — registration. Runs the login principal resolution, then calls
  `Account.create_account` to create a Personal account for that principal. One-time; creates
  state.
- **Router issuance is lazy** — requires an already-registered account (auto-created by
  `gleaph network start` by default, or by `signup`); the first operation needing a Router calls
  `Account.authorize_router_issuance` to issue it.

The web UI and `gleaph login` share the same II app principal, so they access the same Account.
A local PEM identity is a different principal and maps to a different Account (or is added to an
Org).

## Related documents

- [ADR 0068](../adr/0068-account-canister-and-per-developer-router-issuance.md) — account model (SSOT).
- [ADR 0035](../adr/0035-provision-canister-and-issuance-protocol.md) — issuance protocol (Amendment planned for Account trust subject).
- [ADR 0054](../adr/0054-provisioned-logical-graph-topology-and-resource-activation.md) — bootstrap resource selection and topology.
- [ADR 0062](../adr/0062-gleaph-toml-project-configuration.md) — `gleaph.toml` (Amendment planned).
