# 0068. Account canister and per-developer Router issuance

Date: 2026-08-14
Status: proposed
Last revised: 2026-08-22 19:59:13 UTC +0000
Anchor timestamp: 2026-08-22 19:59:13 UTC +0000

## Context

Gleaph currently has no "account" concept. Authentication is Router-internal RBAC
(`crates/auth`, `Role::Executor..Admin`) granted via `grant_role`. The provisioning model
([ADR 0035](0035-provision-canister-and-issuance-protocol.md),
[ADR 0054](0054-provisioned-logical-graph-topology-and-resource-activation.md)) is
"one service-wide Provision issues canisters; Router owns graph identity, tenancy, routing".
There is no notion of a developer owning their own Router.

A developer (or organization) should own a **Router** (and its graphs/shard/index canisters)
within a multi-developer service. The initial deployment shape is **one Router per developer**,
but the data model must not hard-code "one" so that multiple Routers per account can be added
later (e.g. for subnet-split or per-project environments).

## Decision

Introduce a dedicated **Account** canister that owns developer identity/registration, the
account↔Router mapping, and issuance approval. It does not own graph topology, graph tenancy, or
routing catalogs — those remain owned by the issued Router.

### Account model

An account is the ownership boundary for Routers. It is an enum because each variant has a
genuinely different authorization shape and payload. **The enum variant is the discriminator**; a
separate `AccountKind` field is rejected (it would force nullable `Option` fields on each variant).

```rust
enum Account {
    Personal {
        principal: Principal,                     // account_id == principal
        routers: Map<router_id, RouterEntry>,
    },
    Org {
        account_id: String,                       // generated id, independent of any owner
        members: Map<Principal, Role>,            // owner >= admin >= member RBAC
        routers: Map<router_id, RouterEntry>,
    },
    // TODO(governance): future third kind where an SNS governance canister is the sole
    // authority (proposal-driven control). Not implemented now; reserve as an extension point.
}
```

| Aspect              | Personal                                       | Org                                             |
| ------------------- | ---------------------------------------------- | ----------------------------------------------- |
| `account_id`        | `== owner principal`                            | generated id, independent of owner              |
| Owner/members       | single owner only (no member RBAC)             | `members: Map<Principal, Role>`                 |
| Roles               | none (owner == principal)                       | `owner >= admin >= member`                      |
| Multiple people     | not supported (invite to create an Org)         | supported                                       |
| Governance (SNS)    | not applicable                                 | reserved extension point                        |

Org roles are **three levels** (`owner >= admin >= member`). Account management (membership,
Router issuance approval) is deliberately separate from Router-internal RBAC, which governs
data-plane operations. The two models are not merged.

| Role   | Account-scope authority                                        |
| ------ | -------------------------------------------------------------- |
| owner  | membership management, Router issuance approval, account delete |
| admin  | Router issuance approval (add a Router)                        |
| member | read-only (`list_routers`, `resolve_router`), graph usage      |

Routers are stored per account as `routers: Map<router_id, RouterEntry>` (1:N). The initial
deployment uses a single Router, but the **structure is 1:N** so additional Routers can be added
without a storage or key change.

### Account id and deployment scope

An **account maps 1:1 to an ADR 0035/0054 `deployment_id`** — the issuance and trust-binding
scope. `deployment_id` is derived from `account_id` (Personal principal or Org generated id, see
the table above); it is not a separate concept the user configures. During the bootstrap handover
Account holds the deployment trust binding; after Router issuance the binding is handed to the
Router.

### Bootstrap trust handover

Because no Router principal exists before the first issuance, **Account acts as the deployment's
issuance authority for the first Router only**; after issuance the trust is handed over to the
issued Router.

```text
# Initial issuance: Account holds the issuance trust
developer ──▶ Account: authorize_router_issuance(id, "default")
Account ──▶ Provision: accept_envelope (Account is the deployment trust subject)
Provision ──▶ Account: issuance result callback → Account: register_router(id, "default", canister)

# After issuance: trust handed over to the issued Router
Router ──▶ Provision: subsequent graph issuance and versionless completion (ADR 0035)
```

This keeps the **Account boundary intact**: Account does not own graph topology. It is the issuance
authority only during the bootstrap handover; once the Router exists, Router owns Graph admission,
topology reconciliation, and the Router-to-Provision completion call. Delivery of the initial
`ProvisionResult` back to Account remains outside the current Graph-only completion slice.

### Account canister API

Account owns only identity/registration, the account↔Router mapping, and issuance approval.

| Method                    | Caller                 | Notes                                          |
| ------------------------- | ---------------------- | ---------------------------------------------- |
| `create_account(name)`    | any (self-register)    | caller becomes sole owner of a Personal account |
| `create_org_account(name)`| any (self-register)    | creates an Org account; caller is the first owner |
| `get_account(id)`         | `has_admin_rights(Account(id))` | account info                          |
| `delete_account(id)`      | owner (Org) / self (Personal) |                                               |
| `add_member(id, p, role)` | owner (Org)            |                                                |
| `remove_member(id, p)`    | owner (Org)            |                                                |
| `set_role(id, p, role)`   | owner (Org)            |                                                |
| `list_routers(id)`        | any member             | CLI / dashboard                                |
| `resolve_router(id, router_id)` | any member       | CLI resolution                                 |
| `register_router(id, router_id, canister)` | owner/admin | called after Provision issuance |
| `unregister_router(id, router_id)` | owner          |                                                |
| `resolve_my_accounts()`   | caller (self)         | caller principal → its accounts                |
| `authorize_router_issuance(id, router_id)` | owner/admin | first-Router bootstrap only |

`resolve_router` on a Router that is not yet issued returns "not issued"; the CLI then
**auto-issues** the Router on demand (see [Lazy Router issuance](#lazy-router-issuance)) instead of
requiring a separate deploy step.

**Creation limits.** A Personal account is unique per principal (a direct consequence of
`account_id == principal`); this is an invariant, not a policy knob. Org creation is **not
rate-limited or count-limited in the initial implementation** — spam / Sybil / resource-abuse
limits are deferred to the cycle-funding design
([ADR 0038](0038-provisioning-authorization-and-cycles-funding.md)).

### CLI resolution (no `account` in `gleaph.toml`)

`gleaph.toml` carries no account identifier. The CLI resolves the account from the caller
principal at runtime:

```text
CLI (principal) ──▶ Account: resolve_my_accounts()
  0 accounts  → "not registered"
  1 account   → use it
  >1 accounts → user selects (--account / --router)
CLI ──▶ resolve_router(id, router_id) → Router canister id
```

### Lazy Router issuance

Router issuance is **on demand, driven by the first operation that needs a Router**. There is no
`gleaph deploy` step and no explicit "issue Router" command: any command that must reach a Router
(`migration apply`, `load`, `prepared`, `codegen`, or GQL) triggers issuance automatically the first
time a Router is needed, then caches the result.

```text
CLI needs a Router for operation O
  cache (`.gleaph/cache/account/<env>.router.json`) hit?      → use cached id
  else Account.resolve_router(id, router_id) resolves?        → cache + use id
  else (not issued) → auto-issue:
    CLI ──▶ Account.authorize_router_issuance(id, "default", provision)
    Account ──▶ Provision: accept_envelope (LogicalResource::Router)
    Provision ──▶ Account: issuance result → Router canister id
    CLI ──▶ Account.register_router(id, "default", canister)   → cache + use id
CLI proceeds with O against the Router
```

The issued Router is installed with `RouterInitArgs { provision_canister: Some(provision), .. }`,
so it participates in the ADR 0035 deployment-binding handover and can later issue graph
shard/index/vector canisters itself. The Router init args already carry `provision_canister`, so the
Router never falls back to a dev-mode manual-install path.

#### Provisioner / Account / Router / CLI division of labor

| Component | Owns | Never does |
| --------- | ---- | ---------- |
| Provisioner | artifact catalog + create/install state machine; the **only** issuer | does not decide *whether* a Router should exist |
| Account | identity, account↔Router mapping, first-Router issuance **authorization** (bootstrap trust subject) | never installs/creates a canister |
| CLI | Router-id resolution + cache; on-demand issuance trigger; `network`/`signup` | no management-canister installs |
| Router | post-issuance deployment binding (ADR 0035 ack), graph topology, subsequent shard/index/vector issuance to Provisioner | not involved in its own issuance |

The Account requests a **`LogicalResource::Router`** (not a graph shard) when it drives the first
issuance. `authorize_router_issuance` is the bootstrap handover for the first Router only; once the
Router exists, ADR 0035's normal issuer (the Router principal) governs all further issuance, exactly
as in the [Bootstrap trust handover](#bootstrap-trust-handover) section.

#### `network` auto-registration

`gleaph network start` deploys Account and Provision and, by default, **auto-registers** the
caller's Personal account (one `create_account` for the caller principal) so a fresh developer can
run `migration`/`code` without a separate `signup`. A `--no-auto-register` flag disables this for
platform-only provisioning. Auto-registration only creates the account; it never issues a Router.

## Ownership and invariants

| Invariant | Enforcer |
| --------- | -------- |
| Account owns identity, account↔Router mapping, and issuance approval; no graph topology or tenancy. | Account API + storage boundary |
| The enum variant is the discriminator; no separate `AccountKind`. | Account type definition |
| Personal accounts are single-owner and unique per principal; multiple people require an Org. | Account `create_account` / `add_member` |
| Router structure is 1:N; the initial single Router is not hard-coded as "one". | `routers: Map<router_id, RouterEntry>` |
| The caller's principal is the center of access control; `account` is not committed in `gleaph.toml`. | CLI resolution + Account RBAC |
| Account is the issuance authority only during the bootstrap handover; the Router owns the deployment binding afterward. | trust handover + ADR 0035 ack model |
| Router issuance is on demand (lazy), triggered by the first operation needing a Router; there is no `gleaph deploy`. | CLI lazy-resolution + cache + Account `authorize_router_issuance` |
| The first-Router issuance requests a `LogicalResource::Router`, not a graph shard. | Account `authorize_router_issuance` request construction |

## Alternatives considered

- **Account id = principal for all accounts.** Rejected: an account is an organization with
  multiple members and owner changes; binding the id to a single principal breaks on owner
  rotation. Personal accounts (single owner) use the principal; Org accounts use a generated id.
- **Account owns graph topology.** Rejected: it would duplicate the Router's canonical graph and
  routing catalogs (ADR 0035/0054).
- **Account is the permanent issuance authority.** Rejected: it would make Account a second
  topology registry and break the ADR 0035 "Router owns the deployment binding" contract. Trust is
  handed over to the Router after the first issuance.
- **A separate `AccountKind` field.** Rejected: the enum variant is the discriminator; a `kind`
  field would force nullable `Option` fields on each variant.

## Consequences

- A developer can own a Router (and its graphs/shard/index canisters) within a multi-developer
  service, with a lightweight Personal account or a multi-member Org account.
- Provision needs two additions to accept Account as a bootstrap trust subject and to deliver the
  first-issuance result to Account (see ADR 0035 amendment).
- The CLI resolves the account from the caller principal at runtime; `gleaph.toml` carries no
  account identifier.
- Router issuance is lazy: the first operation that needs a Router triggers it, and `gleaph deploy`
  is removed from the CLI surface. A fresh developer runs `gleaph network start` (which
  auto-registers the account by default), then any data-plane/DDL command.
- Governance (SNS) control is reserved as a future third account kind; service-scope admin is a
  separate concern.

## Implementation status

**Lazy-issuance design adopted and core protocol implemented (2026-08-21); the end-to-end
provisioned Router path remains to be validated in a PocketIC runtime.** The Account canister and
the account↔Router mapping exist. The following are implemented:

- `LogicalResource::Router` (variant + fixed 5-byte stable encoding tag `3`) in `gleaph-graph-kernel`.
- `LogicalResource::Router` → `CanisterKind::Router` mapping in the Provisioner (the kind and its
  manifest already existed); hand-written `provision.did` updated.
- Account's `authorize_router_issuance` requests a `LogicalResource::Router`, not a `GraphShard(0)`,
  with the correct `RouterInitArgs` install args.
- `gleaph deploy` removed from the CLI surface; Router resolution auto-issues on demand in
  `resolve_router_from_account` (cache → `Account.resolve_router` → `authorize_router_issuance` →
  `register_router` → cache).
- `gleaph network start` auto-registers the caller's Personal account unless `--no-auto-register`.

The **end-to-end** provisioned Router issuance (a live Release/artifact install through
`accept_envelope` producing a running Router that responds to queries) is validated in
`crates/pocket-ic-tests/tests/adr0068_router_issuance.rs`: the test publishes the real Router
artifact, activates a release, drives `authorize_router_issuance`, and asserts the issued Router
answers `whoami`. The design contract is fixed in
[`design/architecture/account-and-provisioning.md`](../architecture/account-and-provisioning.md).

## Cross-links

- [ADR 0035](0035-provision-canister-and-issuance-protocol.md) — issuance protocol and Provision ownership (to be amended for Account trust subject + first-issuance callback).
- [ADR 0054](0054-provisioned-logical-graph-topology-and-resource-activation.md) — bootstrap resource selection and topology.
- [ADR 0062](0062-gleaph-toml-project-configuration.md) — `gleaph.toml` (to be amended: account removal, environment/network split, identity as name).
- [ADR 0028](0028-per-graph-tenancy-metadata-reads.md) — graph tenancy (owned by Router, not Account).
- [ADR 0038](0038-provisioning-authorization-and-cycles-funding.md) — Org creation limits and resource admission.
