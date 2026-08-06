# 0063. Prepared query global names and GQL-delegated graph resolution

This ADR makes prepared operation names Router-global and delegates the target graph
resolution to the hidden GQL source. It supersedes the execution-side resolution rule of
ADR 0061 §5 (visibility-scan by caller) while keeping the registration rule (program-derived
graph, ADR 0061 §4) as the single point where a prepared operation binds to a graph.

Date: 2026-08-05
Status: accepted
Last revised: 2026-08-05
Anchor timestamp: 2026-08-05 23:40:24 UTC +0000

## Context

Prepared operations are invoked by name only: `prepared_query(name, params, sort, read_mode)`
and `prepared_mutate(name, params, client_mutation_key)`. The GQL source is hidden from
callers (ADR 0053, 0061) — the caller sees the operation name, never the query text, so the
target graph is an implementation detail of a prepared operation.

Today the graph is determined **twice, by two different mechanisms**:

| Phase                                   | Mechanism                                                                                           | Result                                                                    |
| --------------------------------------- | --------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| Registration (ADR 0061 §4)              | Program-derived: `resolve_graph_context` (session commands / `USE GRAPH` / HOME fallback, ADR 0011) | Plan bound to a `GraphId`; stored under `PreparedPlanKey(graph_id, name)` |
| Execution (`resolve_prepared_graph_id`) | Caller-visibility scan: `list_visible_graph_ids` (owner/admin only)                                 | Zero matches → `NotFound`; more than one → ambiguity error (ADR 0061 §5)  |

The execution-side scan is redundant — registration already bound the graph — and it is the
effective access gate: anonymous callers (e.g. a browser) see no graph and cannot resolve any
prepared operation. This contradicts the authorization layer's own intent, where
`authorize_prepared_execute` computes the caller's effective role (default `Executor` for every
principal, including anonymous) and never rejects.

The prepared catalog already stores the query text as the durable source of truth and rebuilds
plans in the Router heap after an upgrade (`PreparedPlanRecordV1 { query, metadata }`), so the
graph binding can move out of the catalog key without adding storage machinery.

Access control — public-graph flags, per-graph policies, caller roles — is explicitly out of
scope for this ADR and deferred to a future access-control design. In this slice, prepared
execution is uniformly public.

## Decision

### 1. Prepared operation names are Router-global

The catalog key becomes the operation name alone: `PreparedPlanKey` drops `graph_id`, and the
record gains the bound `graph_id` (resolved at registration, §2). No secondary index is added;
the name key is the sole lookup path.

**Name-collision avoidance is the Gleaph operator's responsibility.** The batch `prepare`
keeps its replace-in-place idempotent upsert semantics; registering an existing name replaces
the record regardless of the previously bound graph. No conflict detection is added.

### 2. Graph resolution is delegated to GQL, once, at registration

The bound graph is derived from the hidden query text at registration via the existing
program-derived resolution (ADR 0011 `resolve_graph_context` plus the `USE GRAPH` dispatch
analysis, ADR 0061 §4). Execution does **not** re-resolve the graph from the caller.

On a cache miss, the plan is rebuilt from the hidden source against the record's bound graph;
the existing text↔graph invariant check stays and fails closed on mismatch (a hidden source
whose `USE GRAPH` disagrees with the bound graph is rejected).

### 3. Execution and read-back resolve by name; no caller gate in this slice

`prepared_query`, `prepared_mutate`, `get_prepared`, and `drop_prepared` drop
`resolve_prepared_graph_id` and look the operation up by name. The ambiguity error disappears
(a prepared name is single-graph by construction). `drop_prepared` remains privileged
(`authorize_prepared_catalog_change`); the read surfaces carry no caller gate — all prepared
execution is public until the future access-control design lands.

### 4. Candid signatures are unchanged — no optional graph argument

`prepared_query` / `prepared_mutate` keep their current signatures. An optional `graph_name`
argument is rejected (Alternatives §1): the hidden query text is the single source of truth
for the graph, and an argument would duplicate it and require priority rules.

### 5. No graph-name exposure

Because the GQL source is hidden, the bound graph is never surfaced to the caller. The graph
appears only in the operator-facing registration surface and the record.

### 6. Breaking change, in place: no migration, no version bump, minimal storage delta

The catalog key layout changes without a versioned record transition and without migration
code: existing stable records are invalidated on upgrade and must be re-registered by
re-running `gleaph prepared apply` (or the deploy script). This is accepted per project stage
(pre-release, single deployment path). Storage delta is one `GraphId` field on the record and
the removal of `graph_id` from the key.

**Upgrade mechanism:** there is no live deployment to migrate — the platform is pre-release —
so the breaking change takes effect with the next reinstall and re-registration
(`gleaph prepared apply` / the deploy script). This is consistent with ADR 0007's
development-only wipe policy and its explicit non-goal of bumping version numbers solely for
dev-only layout changes; no stable-memory region is reallocated, so ADR 0007's region table is
unaffected.

## Consequences

### Implementation surface

| Entrypoint                           | Today                                          | After                                                                         |
| ------------------------------------ | ---------------------------------------------- | ----------------------------------------------------------------------------- |
| `prepared_query` / `prepared_mutate` | no-op authorize + visibility scan              | name lookup → record bound graph → execute; no gate                           |
| `get_prepared`                       | no-op authorize + visibility scan              | name lookup; no gate                                                          |
| `drop_prepared`                      | privileged authorize + visibility scan         | name lookup; privileged gate unchanged                                        |
| `list_prepared`                      | graph filter by key `graph_id` (HOME default)  | graph filter by record bound `graph_id`; operator surface otherwise unchanged |
| `prepare` (batch)                    | program-derived graph → key `(graph_id, name)` | program-derived graph → record `graph_id`; key `(name)`                       |

### Positive

- Single source of truth for the target graph: the hidden GQL text, resolved once at
  registration.
- `prepared_query` / `prepared_mutate` become genuinely graph-independent; the
  owner/admin visibility wall no longer gates prepared execution.
- The ambiguity error (same name across visible graphs) is removed by construction.
- Prepared operations are portable: the same name + source re-registers cleanly on any Router.

### Negative

- Global names can collide across tenants or operators with no protection; the operator is
  responsible for avoiding collisions.
- Upgrading invalidates existing registrations (there are no live deployments today); prepared
  operations are re-applied via `gleaph prepared apply`.
- `list_prepared`'s optional graph filter now filters by the record's bound graph.
- The heap-cache key follows the new name-only shape; it is rebuilt on upgrade like today.
- The CLI `prepared status` / `apply` wire surface is unchanged (name-only); re-running
  `apply` repopulates the catalog after an upgrade.

### Deferred (future ADR)

- Access control: public-graph concept, per-graph policies, caller roles, and the
  re-introduction of a caller gate on read surfaces.
- Vector-search read consistency: prepared `SEARCH` lowering stays at `ReadMode::Eventual`
  (existing limitation, ADR 0034 path); unchanged by this ADR.

## Alternatives considered

### 1. Optional `graph_name` argument on `prepared_query` — rejected

Adding an optional graph argument duplicates the graph's source of truth across three places
(registration key, call argument, hidden query text) and requires priority rules (does the
argument override the hidden `USE GRAPH`?). It also re-introduces logical-graph dependence in
the API, which is the property this ADR removes. This is the "future optional graph selector"
ADR 0061 §5 hinted at; it is rejected here in favor of name-global resolution.

### 2. Keep per-graph keys; drop the visibility filter (minimum change) — rejected

The smallest diff would keep `PreparedPlanKey(graph_id, name)` and change only
`resolve_prepared_graph_id` to scan every graph instead of the caller's visible graphs. No key
change, no upgrade wipe, and the lookup cost stays O(#graphs). It is rejected because it keeps
the ambiguity machinery (same name across graphs still fails with `InvalidArgument`), which
contradicts the operator-responsibility collision model, and because it preserves a catalog
whose keys cannot express the actual (global) name semantics — the name meaning would live
implicitly in the scan rather than in the key.

### 3. Keep per-graph keys; resolve execution graph from caller session/HOME — rejected

Plan lookup requires the graph before the plan (and therefore the hidden source) is available
(chicken-and-egg), and the hidden source may legitimately name a different graph than the
caller's home graph. The visibility scan this replaces is the same pattern in different
clothing.

### 4. Public-graph registry flag (open anonymous access per graph) — deferred

Orthogonal to resolution: this ADR removes the resolution wall entirely (all public) rather
than introducing a per-graph escape hatch. The flag belongs to the future access-control
design, where per-graph policy can be re-introduced on top of the bound-graph record.

## References

- [ADR 0011 — GQL graph resolution and catalog scoping](0011-gql-graph-resolution-and-catalog-scoping.md)
- [ADR 0053 — Prepared-query code generation and client-runtime boundary](0053-prepared-query-codegen-and-client-runtime-boundary.md)
- [ADR 0061 — Prepared-query CLI registration and batch catalog API](0061-prepared-cli-registration-and-batch-catalog-api.md)
- `crates/router/src/prepared.rs` (`resolve_prepared_graph_id`, `prepared_run_unchecked`, `prepare_cache_for_execution`)
- `crates/router/src/facade/stable/prepared_catalog.rs` (`PreparedPlanKey`, `PreparedPlanRecordV1`)
- `crates/router/src/rbac.rs` (`authorize_prepared_execute`), `crates/router/src/facade/store/registry.rs` (`list_visible_graph_ids`)
