# 0082. ReBAC conditional policies: bounded EXISTS traversal on grant rows

Date: 2026-08-25
Status: implemented
Last revised: 2026-08-26

## Context

[ADR 0075] landed grant-attached conditional policies: an AND-only DSL of
property comparisons over one vertex label, compiled onto grant rows and lowered
by the Router into ordinary plan ops after data-plane enforcement. That DSL
answers "callers see posts whose `visibility = 'public'` or whose `owner =
MSG_CALLER()`" — but only when the deciding fact is a **property on the row
itself**.

The demonstrated product need is **relationship-based** visibility: "a caller
sees a document when a `GRANTED_TO` edge points at an account vertex they
control", "a caller sees a post when it is shared to a group they belong to".
Today the only way to express this is to **denormalize the relationship into a
property** on every row (copy member ids onto each post, maintain a
`visible_to` list). That is write amplification on every membership change,
staleness risk, and it defeats the point of a graph database where the
relationship is the single source of truth. The parser already rejects `EXISTS`
in conditional policies with the explicit message "ReBAC conditions are a later
phase" (`crates/gql/src/parser/statement.rs`), and [ADR 0075] §"Design
documentation impact" opened "ReBAC `EXISTS` policies (bounded traversal)" as a
follow-up.

This ADR decides the shape, the execution position, the budget accounting, and
the cache discipline for that bounded traversal condition.

## Existing architecture assessment

Preserved as-is:

- **Grant-attached predicates, one rule = one row** ([ADR 0075] §1): a
  conditional grant is one row carrying its condition inline; introspection
  prints it; `REVOKE` removes rule and condition together. ReBAC conditions
  extend this shape; standalone `POLICY` objects remain rejected.
- **Router-side lowering after enforcement** ([ADR 0075] §5): policies constrain
  outputs, they add no requirements — requirement extraction never sees
  policy-derived reads. The ordering invariant is documented at
  `crates/router/src/gql.rs` (enforcement strictly precedes lowering).
- **Trust model** ([ADR 0028], [ADR 0074]): shards trust the Router plus
  registered internal callers; index/vector canisters never evaluate policies;
  caller identity does not propagate below the Router. Shards execute filters
  they cannot distinguish from user-authored ones.
- **Batch fan-out dispatch**: the Router sends the same encoded plan blob to
  every live shard of the graph and merges results; it never iterates result
  rows. Traversals within a plan execute shard-locally over each partition.
- **Fingerprint cache discipline** ([ADR 0075] §5 trade-off): per-execution
  lowered shapes cache keyed by the exact fingerprint of the lowering inputs;
  any grant write invalidates the cache.
- **Budget accounting** ([ADR 0075] §5): instruction cost rides normal plan ops
  under existing budgets; no new accounting surface.

Missing machinery (the demonstrated gap):

- `PlanOp` has **no SemiJoin, AntiJoin, Apply, or Exists/correlated-subplan
  variant** (`crates/gql-planner/src/plan.rs`). Nested subplans appear only as
  whole-input operands (`HashJoin`, `CartesianProduct`, `SetOperation`,
  `OptionalMatch`, `InlineProcedureCall`, `UseGraph`); none is parameterized per
  output row.
- The AST already parses `ExprKind::ExistsSubquery` / `ExistsPattern`
  (`crates/gql/src/ast/expr.rs`) and infers them as `Bool`, but the **planner
  never lowers them** and the **shard evaluator has no implementation**
  (`crates/graph`). They would ride opaquely inside a `Filter` expression with
  no executor behind them.
- The closest existing executor mechanism is `OptionalMatch`
  (`crates/graph/src/plan/query/executor.rs`): a left-outer subplan executed
  per input row batch, with **no correlation parameter passing**. It is a
  skeleton, not a semi-join.

## Decision

### 1. A bounded `EXISTS` clause extends the grant-attached predicate DSL

A conditional grant row gains an optional bounded traversal clause (a chain of
**1–2 hops**) alongside its property conjuncts. One rule stays one row; the
clause is part of the same compiled predicate, printed inline by introspection,
and removed by the same `REVOKE`.

```text
GrantRow = { subject, graph, privileges…, predicate?: CompiledPredicate, expires_at? }
CompiledPredicate = { label, conjuncts: [Comparison; 1..=8], chain?: Chain }
Chain = Hop | Hop Hop                       // 1–2 hops, vertex → vertex
Hop = { edge_label, direction, dest_label }
TerminalConjuncts = [Comparison; 1..=8]     // evaluated on the last hop's destination
```

As implemented (2026-08-26): a pure-EXISTS condition carries zero source `conjuncts`
(the flagship grammar form below); the terminal conjunct group is **required** rather
than optional, so every chain names its gate.

The clause is a **semi-join filter**: a row is visible iff at least one matching
chain exists. It never duplicates rows, never projects values, and never turns a
missing match into an error.

### 2. Grammar subset (Phase R1)

```gql
-- direct-grant pattern (1 hop)
GRANT READ ON GRAPH social
  FOR (d:Doc) WHERE EXISTS { (d)-[:GRANTED_TO]->(a:Account) WHERE a.principal_id = MSG_CALLER() }
  TO PRINCIPAL …;

-- organization-membership pattern (2 hops)
GRANT READ ON GRAPH social
  FOR (p:Post) WHERE EXISTS { (p)-[:SHARED_TO]->(g:Group)<-[:MEMBER_OF]-(u) WHERE u.principal_id = MSG_CALLER() }
  TO PRINCIPAL …;
```

- The pattern is a **bounded chain, 1–2 hops, vertex → vertex**:
  `(source)-[:E1]->(mid:Label1)-[:E2]->(dest:Label2)`.
- `source` must be the grant's selector variable (the granted label).
- Each hop carries its own direction; the terminal `dest` is a fresh variable
  bound to a **concrete destination label**.
- `WHERE` on the terminal destination reuses the exact [ADR 0075] comparison DSL
  (`<property> <op> <literal | MSG_CALLER()>`, AND-only, depth ≤ 8), so
  `MSG_CALLER()` resolves to the invoking caller as a literal constant at the
  Router — the same second resolution site as [ADR 0075] §5. Intermediate
  vertices carry a label only; no conjuncts in R1.
- Direction follows the [ADR 0074] §2 rules: omitted direction on a directed
  label means BOTH; undirected labels reject directional modifiers at
  validation time; an undirected-pattern match over a directed label requires
  both directional rows.
- Wildcard labels are not granted in Phase R1; the clause enumerates each edge
  label and each intermediate/terminal label.

### 3. Compilation and validation at GRANT time

`compile_condition` (`crates/router/src/gql_grants.rs`) extends to the chain:

- The clause is accepted only on `MATCH`/`READ` rows over a `VertexLabel`
  resource — the same gate `compile_condition` already enforces for property
  conjuncts ([ADR 0075] §3/§5). Relationship-based conditions on mutation rows
  (`CREATE`/`UPDATE`/`DELETE`) are **deferred**: the demonstrated need is read
  visibility, and mutation conditions would change the write-path requirement
  extraction in addition to the semi-join executor surface.
- Catalog checks at GRANT time, per hop: the edge label exists; the direction
  modifier is valid for the label's directedness; the intermediate/terminal
  label exists; terminal property ids resolve; literal-vs-declared-scalar-type
  compatibility is checked open-world exactly as today.
- Conjunct counts stay within `1..=8` for the source and terminal groups; the
  chain is bounded at **2 hops** (a chain-depth knob exists only as the fixed
  `1..=2` bound, not a configurable depth).

### 4. Stable encoding

`CompiledPredicate` gains a versioned encoding (`V2`) with an optional trailing
`Chain` field; the inner byte encoding adds a version discriminator so a V1
predicate and a V2 predicate are distinguishable at decode time. Pre-production
destructive evolution: fresh state required, old bytes reject, no decode shims.
Grant rows stay on `MemoryId 55`
(`ROUTER_AUTH_GRANTS`); no new stable region. The vocabulary-drop cascade
([ADR 0074] invariant 4) already sweeps graph-scoped rows by graph
(`revoke_all_for_graph`), so compiled edge/intermediate/terminal label ids in
the chain are covered by the same sweep — a dropped id can never be reallocated,
and a stale chain fails closed.

### 5. Semantics

- **Semi-join, not join**: visible iff ≥ 1 matching chain; the executor
  short-circuits at the first match per input row. No row duplication, no
  null-padding, no projection of chain values into results.
- **The relationship is the gate.** The chain traverses edges and reads terminal
  properties that the *evaluating caller* may hold no `TRAVERSE`/`READ_PROPERTY`
  grant for. This is the essence of ReBAC, not an inversion: the policy grants
  visibility *through* a relationship the graph owner defined, so the caller's
  own privilege set must not gate the policy's internal reads. Requirement
  extraction stays blind to the chain. (The *granter* side is trivially
  satisfied in Phase 1 — grant authority is the registry owner, who holds full
  authority over their graph — and is revisited only if grant administration is
  ever delegated.)
- **Filtered rows are absent, never errors** — the authorized-subset contract of
  [ADR 0074] §4.6. Structural-privilege failures remain hard errors; the chain
  is a resource-level filter.
- **Uniform non-disclosure preserved**: an uncovered caller is rejected with the
  same `Forbidden` that never names the missing privilege or resource; a
  covered caller whose rows fail the chain sees empty results, never an error.

### 6. Lowering: a new `PlanOp` executed by shards

The chain lowers into a new `PlanOp` (working name `SemiApply`) that executes
shard-side, modeled on `OptionalMatch`'s per-input-batch subplan execution but
with **semi semantics and correlation on the source binding**:

- The Router substitutes `MSG_CALLER()` in the terminal conjuncts as a literal
  constant, then emits the op with a nested subplan that expands each hop's edge
  label in its direction, filters each intermediate/terminal label, and applies
  the terminal conjuncts. The op keeps a row iff the subplan yields ≥ 1 match,
  short-circuiting per row.
- Shards execute it as ordinary plan machinery; no caller identity, policy
  object, or policy engine crosses the Router boundary. The trust model is
  unchanged.
- The wire change is confined to the **encoded plan blob** (Router → shard),
  which is versioned and fresh-state; no caller-facing wire change.

**Shard-local boundary (inherited, explicit).** A single plan execution is
strictly shard-local today: edges are stored source-local, vertices belong to
exactly one shard, cross-shard `federated_expand` was removed and is a no-op,
and destination property reads are only possible for local destinations. The
chain therefore executes shard-locally exactly like ordinary traversals, and
inherits the same boundary: a chain whose intermediate/terminal vertex lives on
a different shard than the source is not found by the local probe. This is
consistent with current traversal semantics, not a new gap; cross-shard
destinations are out of scope until the cross-shard traversal ADR lands.

Adding the variant requires updating the four exhaustive consumers — this is the
fail-closed safety net working as designed:

1. `authz.rs::walk_op` — an exhaustive arm (a forgotten arm fails compilation,
   not silently bypasses enforcement).
2. `policy_pushdown.rs` — `recurse_nested_subplans` / `decide_site` /
   `hydrate_site` arms for the nested subplan.
3. `executor_contract.rs::first_executor_unsupported_op` — a decision for the
   new shape.
4. `crates/graph/src/plan/query/executor.rs` — the shard-side execution.

**Planner boundary (deliberate).** The new `PlanOp` is a **general semi-join
operator**, not Gleaph-specific authorization logic, so it belongs in the
general-purpose `gleaph-gql-planner` crate alongside `OptionalMatch`/`HashJoin`
— consistent with the rule that the GQL crates stay language-oriented and
Gleaph-specific behavior lives in integration layers. The *policy* semantics
(that this op is an authorization filter) stay entirely Router-side; the planner
gains a reusable relational shape, and the authz walker consumes it like any
other op. This is a bounded, deliberate deviation from the "principally zero
gql-planner change" guidance for the authorization walker: the walker itself
still needs no planner cooperation beyond the exhaustive arm, and the new op is
not authz-specific.

### 7. Budget accounting and fan-out bounding

The chain rides normal plan ops under existing budgets ([ADR 0075] §5): the
semi-join's expansions and terminal filters are ordinary operations charged by
the existing per-operation estimates against `MAX_QUERY_CALL_INSTRUCTIONS`
(`crates/instruction-budget`). No new accounting surface and no second limit
knob — the same discipline the vector deepening loop follows by aliasing
`GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION` instead of introducing a new
constant. Heavy predicates are the graph owner's choice, priced by existing
budgets.

**Reverse-index-driven terminal equality.** When the terminal predicate is an
equality against `MSG_CALLER()`, an active physical index exists on the
terminal property, and **exactly one applicable row covers the label** (the
same single-row precondition `indexed_equality` enforces for the source side),
the chain is driven from the **destination side**: the index resolves the
caller's terminal vertices first, then reverse-expands the chain to derive the
candidate source set — avoiding a per-source scan of high-fan-out edges (e.g. a
large group's membership list). The reverse-seeded candidate set is a candidate
source set that **composes with source-side predicates as residual filters**
(an index-seeded scan still applies its remaining conjuncts), exactly as the
source-side `indexed_equality` path does. Reverse expansion relies on the
reverse adjacency maintained by [ADR 0026]'s differential repair, which is
already available for both hops. This extends the existing source-side
`indexed_equality` pattern ([ADR 0075] §5, `policy_pushdown.rs`) to the
destination side; it is the same index-seed discipline applied in the reverse
direction, not a new mechanism. When more than one row covers the label, or no
index exists, the equality lowers to a residual filter and the instruction
budget bounds the scan.

### 8. Cache discipline

Per-execution lowered shapes cache keyed by the exact fingerprint of the
lowering inputs, extended to include the compiled `Chain` bytes (per-hop edge
label, direction, intermediate/terminal label, terminal conjuncts) plus the
resolved caller bytes. Grant writes invalidate the cache exactly as today
(`facade/auth.rs`). **Data changes require no invalidation**: correctness comes
from executing the lowered plan fresh on each dispatch, so a membership edge
added or removed takes effect on the next execution without any cache action.

### 9. Vector search composition

[ADR 0078] layer 2 (per-candidate visibility tail plan) sees the lowered chain
like any other policy filter: search candidates seed the ordinary tail plan,
and the semi-join filters them exactly as ordinary rows. The vector canister
stays policy-blind; the deepening loop is unchanged. Nothing enters GraphRAG
context except through plan execution, so layer 3 is covered by construction.

### 10. Organization permissions are graph-local relationships

"Organization permission model" is expressed as **graph-local relationship
patterns**: orgs, groups, and memberships are vertices and edges in the graph,
and the `EXISTS` clause references them. The account canister's `Role` enum
(`crates/account/src/types.rs`, [ADR 0068]) is a **separate domain** and is
out of scope: the Router has no dependency on the account canister today, and
this ADR introduces none. Cross-canister membership reads would add a new
dependency direction, make account-canister availability a query-path
precondition, and duplicate relationship knowledge that the graph already owns
— rejected (see Alternatives).

### 11. Invariants

1. **Policy-internal reads never become requirements and never leak values**:
   the chain's traversal and terminal reads are authorization machinery; they
   add no demands to the requirement set and project nothing into results.
2. **The relationship is the gate**: the caller's own privilege set never gates
   the chain's internal reads; visibility is granted *through* the relationship
   the graph owner defined.
3. **One rule = one row preserved**: the chain lives in the same compiled
   predicate; no second identity/lifecycle for the condition.
4. **Shards stay policy-blind**: the chain lowers to ordinary plan ops; no caller
   identity or policy engine crosses the Router boundary.
5. **Uniform non-disclosure preserved** (structural failures hard, resource
   filtering silent).
6. **Catalog monotonicity cascade covers the chain**: compiled edge/
   intermediate/terminal label ids are swept with the graph's vocabulary drop
   and can never be reallocated; stale chains fail closed.
7. **Anonymous invariant unchanged**: `PUBLIC` is the only reachable path for
   anonymous callers; the chain resolves `MSG_CALLER()` to the anonymous
   principal only through a `PUBLIC`-subject row, exactly as [ADR 0075] does
   today.

## Consequences

Positive:

- Relationship-based visibility without denormalizing relationships into
  properties — the graph relationship stays the single source of truth.
- The demonstrated shared-document / group-membership product pattern becomes
  expressible in one grant row.
- Trust model, budget accounting, and cache discipline are unchanged in kind;
  the chain is a bounded extension of the existing lowering seam.
- Vector search and GraphRAG inherit the visibility contract by construction.

Trade-offs accepted:

- A new `PlanOp` variant touches four exhaustive consumers and the encoded plan
  blob — a real but bounded cost, and the exhaustive arms are the safety net.
- Chain depth is bounded at 2: 3+ hop chains (e.g. "member of a group that is
  itself a member of a parent org") require either a longer chain or a
  denormalized membership edge; deferred until repetition pain is demonstrated.
- Semi-join execution consumes shard cycles like any filter; heavy or
  high-fan-out chains are the graph owner's choice, priced by existing budgets,
  with the reverse-index-driven terminal equality as the supernode mitigation.
- The caller's own grants never gate the chain's internal reads — the
  relationship is the gate. This is the essence of ReBAC, not a bypass; the
  granter side is trivially satisfied in Phase 1 (owner authority) and revisited
  only if grant administration is delegated.

## Alternatives considered

- **Router-side probe loop** (modeled on the vector deepening loop): rejected —
  a per-probe shard round-trip does not scale to filtering arbitrary row sets;
  it fits seed-probe shapes (like ANN) but not general semi-join filtering.
- **Shard-side opaque `EXISTS` expression evaluation** (planner emits the
  `ExistsPattern` AST node opaquely into a `Filter`): rejected — the shard
  evaluator would need a correlated-subplan implementation anyway, and an opaque
  expression the shard cannot distinguish from user-authored code weakens the
  auditability of the trust model. Lowering to an explicit `PlanOp` keeps the
  shape inspectable and the exhaustive walker honest.
- **Standalone `POLICY` objects referenced by grants**: rejected again, for the
  same SSOT reason as [ADR 0075] §1 — duplicated rule identity, lifecycle, and
  introspection.
- **Cross-canister org membership reads (Router → Account)**: rejected — new
  dependency direction, account-canister availability becomes a query-path
  precondition, and membership knowledge would be duplicated between the
  account canister and the graph. The graph already owns relationship data;
  org permissions are graph-local.
- **Multi-hop bounded chains in R1**: in scope — the demonstrated
  organization-membership pattern is inherently 2-hop, so R1 ships a bounded
  `1..=2` chain. Chains of 3+ hops are deferred until a real use case demands
  them.
- **Negated `EXISTS` / OR inside the clause**: deferred — anti-semi-join and
  disjunction inside a policy condition are separate decisions with their own
  executor and union-lowering implications.

## Migration

Pre-production destructive evolution consistent with prior slices:

- `CompiledPredicate` encoding bumps to `V2` with the optional `Chain`; fresh
  state required, old bytes reject, no decode shims.
- Parser gate flips from rejecting `EXISTS` in conditional policies to accepting
  the bounded 1–2 hop chain form behind the `gleaph` feature gate; the rejection
  message and its test are replaced.
- New `PlanOp` variant added with the four exhaustive consumers updated
  (authz walker, policy pushdown, executor contract, shard executor).
- New PocketIC suite: chain-match visibility matrix (match / no-match /
  multi-match dedup / first-match short-circuit, 1-hop and 2-hop), GRANT-time
  validation failures (unknown edge/intermediate/terminal label, invalid
  direction, bad property, non-vertex selector, 3+ hop chain), prepared
  re-resolution across two callers, vector tail composition, vocabulary-drop
  sweep of a stale chain, and an adversarial walk of the ingress surface
  asserting deny-by-default plus one success path per handler family.
- `design/security/rbac-and-prepared.md` and `design/gql/extension-syntax.md`
  updated in the same patch that lands the code.

## Design documentation impact

- Extends [ADR 0075]'s conditional-policy DSL with the bounded `EXISTS` clause.
- Extends [ADR 0074]'s data-plane model with relationship-based visibility.
- Feeds the audit-log ADR (follow-up): the chain's policy-internal reads are
  authorization machinery and should be visible in elevation/audit review like
  other policy decisions.
- Follow-ups opened: 3+ hop chains; OR inside the clause; anti-`EXISTS`;
  edge-form selectors (already deferred by [ADR 0075] compilation);
  relationship-based conditions on mutation rows (`CREATE`/`UPDATE`/`DELETE`).

[ADR 0026]: 0026-reverse-adjacency-differential-repair.md
[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0068]: 0068-account-canister-and-per-developer-router-issuance.md
[ADR 0074]: 0074-data-plane-authorization-core.md
[ADR 0075]: 0075-conditional-policies-constant-pushdown.md
[ADR 0078]: 0078-authz-aware-vector-search.md
