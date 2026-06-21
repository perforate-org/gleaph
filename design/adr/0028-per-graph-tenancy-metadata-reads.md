# 0028. Per-graph tenancy enforcement on router metadata reads

Date: 2026-06-21
Status: accepted
Last revised: 2026-06-21

## Context

`GraphRegistryEntry` has carried `owner: Principal` and `admins: BTreeSet<Principal>`
since [ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md), and the per-graph
ACL `caller == owner || admins.contains(caller)` was already enforced on the live query
path (`RouterStore::resolve_graph` / `resolve_home_graph_id` / `list_visible_graph_ids`,
reached through `resolve_graph_context`) and on prepared-plan resolution.

However, ten router `#[query]` endpoints resolved a graph by name through the bare
`resolve_graph_id` (a name→id catalog lookup with **no** ACL) and returned graph-scoped
metadata to **any** caller, including the anonymous principal:

- `resolve_shard`, `list_shards_for_graph` — shard topology (graph/index canister principals)
- `lookup_graph_id` — existence + id of a graph by name
- `indexed_property_catalog` — which properties are indexed
- `lookup_{vertex,edge}_label_id`, `lookup_property_id`,
  `reverse_{vertex,edge}_label_name`, `reverse_property_name` — the schema dictionary

This is cross-tenant disclosure: a non-tenant could enumerate another tenant's topology,
schema, and index layout.

Two facts constrain the fix:

1. **Four of these endpoints are called inter-canister by a graph's own shards.**
   `verify_shard_attachment` (`resolve_shard`, `lookup_graph_id`) and federation/maintenance
   routing (`list_shards_for_graph`, `indexed_property_catalog`) reach the router with the
   **`graph_canister` principal**, which is neither the graph owner/admin nor a global Admin.
   Naively gating on owner/admins would break federation and shard attachment.
2. **Registration trusts caller-supplied `owner`/`admins` verbatim** (it only gates on global
   `require_admin`). Nothing prevented registering a graph with `owner == anonymous`, which
   would make the ACL match every unauthenticated caller and silently world-expose the graph.

The visibility model was deferred pending a product decision between canister-global
`Role::Read` gating and real per-graph tenancy. The decision below is per-graph tenancy.

## Decision

Introduce a single tenancy predicate and route the leaking endpoints through an
authorizing resolver.

- **`caller_may_access_graph(entry, graph_id, caller)`** is true when the caller is:
  1. the graph `owner`, or in `admins`; or
  2. a **global canister `Admin`** (superuser bypass; matches a DB superuser, supports
     operations/migration/tooling); or
  3. the graph's **own registered shard canister** — the `graph_canister` principal keyed in
     `ROUTER_SHARD_BY_GRAPH`, scoped to the same `graph_id`. This keeps federation/index-routing
     inter-canister calls working without giving a shard access to other graphs.

- **`RouterStore::resolve_graph_id_authorized(name, caller)`** applies the predicate and backs
  all ten endpoints (`list_shards_for_graph` now resolves then calls the by-id variant).

- **Non-disclosure: a non-tenant receives `NotFound`, not `Forbidden`**, so it cannot
  distinguish "exists but forbidden" from "does not exist". `resolve_graph`'s prior `Forbidden`
  becomes the same `NotFound` and it gains the Admin bypass via the shared predicate.

- **Default/HOME selection is intentionally NOT changed.** `list_visible_graph_ids` and
  `resolve_home_graph_id` keep their membership-only check (no Admin bypass) because they answer
  "which graph is *mine* / the default", and a superuser bypass there would make an Admin's HOME
  resolution ambiguous across all graphs. The intentionally-public `prepared_execute_*` path is
  unchanged; it already scopes via `list_visible_graph_ids`.

- **Registration hardening:** `validate_registration_principals` rejects the anonymous principal
  as `owner` or in `admins`, **before any state is mutated** (so a rejected register interns no
  name). Admin-driven provisioning is otherwise preserved: an Admin still assigns `owner`/`admins`
  on behalf of a tenant.

## Consequences

- The ten metadata endpoints are now tenant-isolated; anonymous/non-tenant callers get `NotFound`.
- Federation, shard attachment, and maintenance keep working via the shard-canister allow path.
- `resolve_graph` (and its endpoint) is now a non-disclosing `NotFound` for non-tenants and
  accessible to global Admins. This is a deliberate API-contract change from `Forbidden`.
- A graph can no longer be registered with an anonymous owner/admin.
- No new stable region or struct: the fix reuses `GraphRegistryEntry.{owner,admins}` and the
  existing `ROUTER_SHARD_BY_GRAPH` reverse map.

## Alternatives considered

- **Canister-global `Role::Read` gating** instead of per-graph tenancy. Rejected: it authenticates
  but does not *isolate* — any `Read` principal would still see every tenant's metadata — and it
  would break the intentionally-public prepared-execute contract.
- **Return `Forbidden` for non-tenants.** Rejected: it discloses graph existence, defeating the
  cross-tenant non-disclosure goal.
- **Register every shard principal in `admins`.** Rejected: it conflates tenant-facing admins with
  infrastructure identities and bloats the ACL; the existing `ROUTER_SHARD_BY_GRAPH` map already
  models "this canister is a shard of this graph".
- **Force `owner = msg_caller()` at registration.** Rejected for now: it removes the Admin's
  ability to provision a graph on behalf of a tenant; validation against the anonymous principal
  is sufficient to close the world-exposure hole.
