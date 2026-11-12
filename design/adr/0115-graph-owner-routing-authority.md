# 0115. Graph-owner routing catalog and profile authority

Date: 2026-11-12
Status: proposed (design; implementation not started)
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 21:40:00 UTC +0900

## Context

ADR 0114 makes routing a per-graph capability: the Router builds a caller-visible context from
the graph type and the prepared-operation catalog, and the per-graph routing profile carries the
abstain threshold and policy version. Today the authority model still reflects the pre-routing
world:

- `prepare` / `drop_prepared` require the Router-global `PREPARE_REGISTER` capability
  (`authorize_prepared_catalog_change`, ADR 0061/0063). A graph owner who can execute and own the
  data cannot register the operation cards that make routing useful.
- `admin_set_general_routing_profile` is controller-only (ADR 0114 item 7). A graph owner cannot
  calibrate and apply their own threshold.
- `admin_general_route_raw`, the calibration diagnostic, is controller-only.
- Prepared names are Router-global (`PreparedPlanKey { name }`, ADR 0063); the target graph is
  resolved once at registration from the hidden GQL source. Execution is name-only, so a
  graph-scoped key would need a client-visible graph argument that ADR 0063 deliberately rejected.

The result is a product gap: graph owners must ask the platform operator to add prepared queries,
change a threshold, or run calibration for their graph.

## Problem

Define a least-privilege authority model that lets graph owners/admins manage the routing surface of
their own graph without widening cross-graph or data-plane authority, while keeping the name-only
execution contract.

## Existing architecture and ownership

- Graph registry (`ROUTER_GRAPHS`) owns owner/admin tenancy; `RouterStore::is_graph_tenant` and
  `caller_may_access_graph` already answer the graph-level question.
- Prepared catalog (`ROUTER_PREPARED_PLANS`, MemoryId 8) is keyed by global name and stores the
  bound `graph_id` on each record.
- Prepared registration resolves the target graph from the hidden query source before storing it;
  name collisions replace in place (ADR 0063).
- Execution publication is a separate grant path (`GRANT EXECUTE ON PREPARED QUERY`, ADR 0074);
  graph tenants have implicit execution authority through `authorize_prepared_execute`.
- The routing profile is per-graph state in `GraphRuntimeConfig` (MemoryId 5).
- The general route never executes queries; it only selects an operation and extracts slots.

## Decision

### 1. Keep Router-global prepared names in this slice

Execution stays name-only (`prepared_query(name, ...)`, ADR 0063). A graph-scoped catalog key would
require either a graph argument on execution (rejected by ADR 0063) or a disconnected name
namespace per graph, so names remain Router-global. Collision pressure is an accepted consequence,
mitigated by the collision rule in §3.

### 2. Graph tenants may author operations bound to their own graph

`prepare` and `drop_prepared` authorize when the caller either:

- holds the global `PREPARE_REGISTER` capability (the existing platform-operator arm), or
- is an owner/admin of the graph that the operation binds to, for **every** operation in the batch.

The bound graph is resolved before any mutation, by the same program-derived path used today.
A batch is all-or-nothing: if any operation resolves to a graph the caller does not administer,
the whole batch is rejected before the first write. Mixed batches with a global
`PREPARE_REGISTER` holder remain allowed.

`get_prepared`/`list_prepared` stay operator surfaces; this ADR does not widen catalog read.

### 3. Name collisions respect existing ownership

Replacing an existing name is allowed when the caller is a tenant of both the existing record's
bound graph and the newly resolved graph, or holds the global capability. Otherwise the
registration returns `Conflict` before any mutation. A tenant can never silently take over another
graph's operation name.

### 4. Graph tenants may read and write their routing profile

`general_routing_profile(graph_name)` is readable by a graph tenant and by a Router controller.
`admin_set_general_routing_profile` is authorized for a graph tenant of that graph or a Router
controller; the existing basis-point/policy validation is unchanged. The write is audit-relevant
configuration, but no new audit ledger is introduced in this slice; the mutation is atomic in
`GraphRuntimeConfig`.

### 5. Graph tenants may run the calibration diagnostic for their graph

`admin_general_route_raw(graph_name, text)` is authorized for a graph tenant of that graph or a
Router controller. It forces the threshold to zero, performs no execution and does not consume
quota. Calibration remains advisory; the caller applies the recommendation through §4.

### 6. Publication and data-plane authority are unchanged

Graph tenancy still implies prepared execution (`authorize_prepared_execute`), and publishing a
prepared operation for non-tenant callers still requires the existing ADR 0074 publication path.
Owning a graph never grants ad-hoc GQL access, Router admin capabilities, or the ability to publish
another graph's operation.

## Consequences

- A graph owner can add/update/drop prepared operations bound to their own graph, read/set the
  routing threshold, and calibrate it without a platform operator.
- Cross-graph mutation still requires the global `PREPARE_REGISTER` capability; mixed-tenant
  batches fail closed.
- Name collisions are a governance/UX problem, not a security hole: replacing another graph's name
  is rejected.
- The graph owner can influence only routing metadata, never execution of a query they cannot
  already execute.
- Existing operator workflows and global capabilities are unchanged.

## Trade-offs

- Global names with owner checks keep the execution wire stable but limit multi-tenant naming
  freedom. A future graph-scoped catalog requires an execution-side graph selector and a new ADR.
- Owner-managed profiles make quality the owner's responsibility; calibration is advisory and
  sample-size caveats remain.
- The ADR deliberately does not add an audit ledger; configuration history is a separate concern
  if required later.

## Alternatives

- **Graph-scoped prepared keys.** Rejected for this slice: execution is name-only; a graph
  argument would duplicate the hidden source's graph binding and break existing clients.
- **A separate owner-only registration API with an explicit graph name.** Rejected: the graph
  would have two sources of truth (hidden source and argument) and the resolution rule would need
  priority semantics.
- **Owner-only names, global operator excluded.** Rejected: platform operators must retain a
  cross-graph repair path.
- **Let owners set the profile without calibration access.** Rejected: calibration would remain
  operator-only and the owner could not tune quality; the raw diagnostic is read-only and scoped
  to their graph.

## Implementation slices

1. Preflight the batch's resolved graph bindings before mutation; add the tenant arm to
   `authorize_prepared_catalog_change` call sites with conflict checks.
2. Add tenant/controller authorization to profile read/write and the raw route; update Router API
   tests and PocketIC E2E for an owner and a non-owner.
3. Update `gleaph prepared apply` / operator docs to explain owner-authored catalogs and the
   collision rule.
4. Add CLI/owner workflow evidence (movie/knowledge owner registers an operation, sets a profile,
   calibrates) and record it in an investigation.
5. Defer graph-scoped names, audit ledger and quotas to a later ADR.

## Review axes

- **Encapsulation:** graph tenancy remains the single graph-level authority source; catalog and
  profile each enforce it at their own boundary.
- **Separation of concerns:** authoring metadata is separate from execution publication; the raw
  calibration diagnostic never executes.
- **Invariants:** resolve-before-mutate; all-or-nothing batches; no cross-graph name takeover;
  profile validation unchanged.
- **Consistency:** prepared record's bound graph remains the single source of truth for ownership.
- **Fitness for purpose:** owners can self-serve routing without widening data-plane authority.

## Open questions

- UX for a tenant who wants a name already used by another graph (prefix convention vs. explicit
  conflict message).
- Whether owner-authored catalog changes need a durable audit event in this slice or a later
  configuration-history ADR.
- Whether calibration reports should be persisted as owner-visible artifacts; this ADR keeps them
  client-side.
- Graph-scoped names and a graph selector on execution remain a future ADR.
