# Discovered Implementation Gaps

Last updated: 2026-07-03
Anchor timestamp: 2026-07-03 17:52:45 UTC +0000

## Status

**Active tracking document** — this ledger records implementation defects, missing product
capabilities, and contract mismatches discovered while implementing another slice when they cannot
be resolved safely in that slice.

It is not a second roadmap or design source of truth. Each entry names the owning module and links
the active design contract. Once an architectural decision is accepted, the owning design document
or ADR remains authoritative and this ledger points to it.

## Disposition rule

Every material gap discovered during implementation, review, validation, or demo integration must
receive one disposition before the current work is committed:

1. **Fix now** when it is a correctness or security defect, blocks the current contract, has a clear
   owner, and can be repaired without obscuring the current slice.
2. **Prerequisite slice** when it blocks the current work but needs independent implementation,
   review, validation, or commit history.
3. **Record here** when it is real but non-blocking, its design is unresolved, or fixing it would
   expand the current slice materially.
4. **Reject as not a gap** only with evidence that the observed behavior matches an existing active
   contract.

Do not leave a gap only in terminal scrollback, a temporary report, an ignored plan file, or a final
chat summary.

## Entry requirements

Each open entry must state:

- **Observed behavior:** reproducible fact, not a proposed solution;
- **Expected or needed behavior:** the contract or product need that exposes the gap;
- **Owner:** module/domain that owns the violated invariant or missing API surface;
- **Evidence:** test, command, source path, or design section;
- **Impact:** what remains unsafe, impossible, misleading, or inefficient;
- **Next decision:** the smallest question or slice that can resolve it;
- **Status:** `Open`, `Planned`, `In progress`, `Resolved`, or `Not a gap`.

Resolved entries remain in the ledger with the fixing commit and owning test. This prevents the same
defect from being rediscovered without its prior reasoning.

## Open gaps

### GAP-2026-07-04-001 — Prepared execution still requires graph visibility

- **Status:** Open
- **Severity:** P2 product gap; P1 if a public frontend must call Router prepared queries directly
- **Owner:** Router prepared catalog resolution and graph authorization
- **Observed behavior:** `authorize_prepared_execute` permits the default Router `Executor` role,
  including an anonymous caller, but prepared-plan resolution searches only graphs visible to the
  caller. A principal that is not the graph owner or in the graph `admins` set therefore cannot
  resolve the prepared plan. The social demo test must currently add its application caller to the
  graph administrators while leaving its Router role at `Executor`.
- **Expected or needed behavior:** an application should be able to expose an administrator-registered
  read-only prepared query without granting its calling principal graph-administrator membership,
  or the product contract must explicitly require an application backend principal with graph
  visibility.
- **Evidence:** `crates/router/src/rbac.rs::authorize_prepared_execute`,
  `crates/router/src/prepared.rs::resolve_prepared_graph_id`, and Plan 0044's
  `install_single_shard_federation_with_graph_admins` fixture.
- **Impact:** the initial public social demo cannot truthfully claim direct anonymous prepared-query
  execution. The current bounded workaround is a graph-visible application principal with no Router
  ad-hoc `Read` role; anonymous and default-Executor semantics must not be conflated.
- **Next decision:** assess three existing-boundary alternatives before adding an API: application
  backend canister principal with graph visibility; a graph-level read/execute membership distinct
  from administrators; or a prepared-plan public-execution flag whose graph is resolved at
  registration. If the latter two are chosen, write an authorization ADR and adversarial cross-graph
  tests.
- **Implemented workaround (does not close this gap):** `crates/social-demo-gateway` provides an
  application-owned canister with a fixed three-variant scenario enum. The Gateway principal is
  registered as a graph administrator so Router can resolve the prepared plan, but it remains a
  default Router Executor with no ad-hoc `Read` role. Anonymous callers execute the fixed scenarios
  through the Gateway; Router observes the Gateway principal, not the original caller. This is an
  application-layer trusted-deputy pattern, not a product change to Router prepared-query
  authorization.
- **Related contracts:** [security/rbac-and-prepared.md](security/rbac-and-prepared.md),
  [demo/social-graph-rag.md](demo/social-graph-rag.md)

## Resolved gaps

### GAP-2026-07-04-002 — `NEXT INSERT` lost edge endpoint identity

- **Status:** Resolved by commit `27e993ae`
- **Severity:** P1 correctness defect
- **Owner:** GQL block planning and Graph projection/mutation execution
- **Observed behavior:** a `MATCH ... RETURN ... NEXT INSERT (a)-[:L]->(b)` mutation reported
  success, but a later traversal observed disconnected/`NULL` endpoints. Separate seed operations
  could not build a shared-vertex social graph.
- **Resolution:** no-YIELD `NEXT` boundaries now preserve typed graph bindings; already-bound node
  variables are not planned as new vertices; plain-variable projections retain `PlanBinding`
  identity through native and wire execution.
- **Evidence:**
  `gql_run::tests::{block_match_next_insert_edge_keeps_endpoints,wire_block_match_next_insert_edge_keeps_endpoints,block_match_next_insert_edge_shares_source}`.
- **Related contracts:** [gql/plan-format.md](gql/plan-format.md),
  [execution/pipeline.md](execution/pipeline.md)

## Review cadence

- The primary agent checks this ledger before final approval of a meaningful slice.
- A slice that resolves an entry updates its status in the same commit as the fix.
- Open entries should be converted to an implementation plan when their prerequisite arrives or
  when they become the highest-impact blocker.
- If an entry duplicates an existing roadmap or ADR item, replace its detailed proposal with a link
  to that authoritative contract rather than maintaining both descriptions.
