# 0074. Data-plane authorization core: per-graph privileges orthogonal to administrative capabilities

Date: 2026-08-23
Status: accepted
Last revised: 2026-08-25

## Context

Gleaph currently authorizes through a canister-global linear role ladder
(`Executor < Read < Write < Manager < Admin`, `crates/auth`) enforced on the Router
before planning (`classify_program` → `authorize_adhoc_gql` in `crates/router/src/rbac.rs`),
plus a per-graph owner/admins metadata ACL ([ADR 0028]). This model answers one question —
"may this principal run ad-hoc GQL at all / perform catalog DDL" — but has no notion of
**what within a graph** a caller may see or touch.

Gleaph is positioned as a decentralized sovereign DB: multiple untrusted principals share
graphs, graphs carry hidden authorization relationships and tenant-owned data, and the
administrative authority may evolve toward DAO governance. That requires a **data plane**
answer ("may this caller traverse FOLLOWS incoming on graph X") that the ladder cannot
express, while keeping an **administration plane** for platform operations. It also requires
that administrative authority does not implicitly read tenant data — under DAO governance,
accountability shifts from trusted individuals to verifiable grant chains.

A prior external proposal sketched a fine-grained RBAC (`Principal → RoleBinding → Role →
Privilege`, conditional policies, ReBAC, VetKeys deferral). After review against the live
architecture it was revised in discussion into the decision below; this ADR supersedes that
proposal wherever they conflict.

## Existing architecture assessment

Preserved as-is:

- Router owns authorization; graph shards trust only the Router and registered internal
  callers (anonymous-principal invariant, `guard_router_canister` /
  `assert_router_caller`). No shard-side policy evaluation is introduced.
- The durable-authority pattern: privileged bindings are seeded via init args
  (`bootstrap_bindings`) into stable memory and validated by
  `crates/auth::validate_bootstrap_principals` (anonymous rejection).
- ADR 0028 per-graph tenancy predicate and its NotFound non-disclosure rule.
- Prepared-query registration gating and versioned prepared-plan records (ADR 0053/0061).

Replaced destructively (pre-production; fresh state required, no compatibility path):

- The five-value `Role` ladder and its rank ordering.
- `authorize_adhoc_gql` role gating as the sole pre-plan admission step (its *write-detection
  agreement* with the planner is kept; the mechanism becomes plan-time privilege checking).
- `grant_role`'s five-role surface.

## Decision

### 1. Two orthogonal dimensions replace the ladder

```text
Subject capabilities =
    admin_caps : global administrative bitset (platform/federation/prepared/index DDL,
                 grant administration bootstrap)
  | per-graph : Privilege grants bound directly to principals (data plane)
  | PUBLIC    : virtual pseudo-subject carrying explicitly granted baseline privileges
Default      : empty everywhere → deny
```

- `admin_caps` succeeds the useful residue of the ladder: federation/shard registration,
  index DDL, prepared-query registration (folding today's `ManagerCapability` bits),
  graph-type catalog DDL, and future grant-administration operations. It is seeded via the
  existing `bootstrap_bindings` init path.
- The data plane binds `(principal | PUBLIC) × graph × privilege` rows directly. Named roles
  (`CREATE ROLE` / role inheritance) are **deferred until repetition pain is demonstrated**;
  direct grants are the single source of truth. Preset bundles ("make Alice a reader of this
  graph"), if added later, are syntax sugar lowered to expanded grant rows at parse time and
  are never stored as roles.
- The old ladder collapses: Executor ≙ empty set (prepared execution now requires an explicit
  or PUBLIC `EXECUTE PreparedQuery` grant); Read ≙ "holds READ-family grants somewhere";
  Write ≙ "holds write-family grants"; Manager/Admin caps fold into `admin_caps`.

### 1b. Ladder disposition

Role-by-role destination and enforcement-point cleanup (normative for the migration slice):

| Role | Current authority | Destination |
|---|---|---|
| Executor | Prepared execution only (default for unknown principals) | Deleted as a concept; default becomes empty/deny. Prepared execution requires an explicit `EXECUTE PreparedQuery` grant to the caller or to `PUBLIC` |
| Read | Read-only ad-hoc GQL | Deleted; derivable from holding READ-family grants. Plan-time checking replaces the role gate |
| Write | + data modification, graph-type catalog DDL, `CALL` | Data modification = per-graph CREATE/UPDATE/DELETE grants. Catalog DDL = `admin_caps(MANAGE_CATALOG)`. `CALL` = `admin_caps(CALL_PROCEDURE)` until procedures become catalog objects (below) |
| Manager | Same as Write + capability bits (`PREPARE_REGISTER`, `INDEX_CREATE`, `INDEX_DROP`) | Bits absorbed into `admin_caps`; the rank itself disappears |
| Admin | Full + `grant_role`, federation, backfill | `admin_caps` full set; **no implicit data-plane reads** (invariant 1) |

Enforcement points:

| Location | Disposition |
|---|---|
| `crates/auth` `Role` enum, `rank()`, `satisfies_at_least`, `FromStr` | Removed; replaced by `AdminCaps` bitflags and grant storage |
| `rbac.rs::authorize_adhoc_gql` | Removed; replaced by plan-time privilege checking wired into plan construction |
| `rbac.rs::authorize_index_ddl` / `authorize_prepared_catalog_change` | Body changes from rank check to caps check; call sites unchanged |
| `rbac.rs::authorize_prepared_execute` | Executor default removed; caller-grants ∪ PUBLIC-grants check |
| `control.rs::grant_role` | Reshaped to caps administration (breaking; SDK regenerated) |
| init args `bootstrap_bindings` | Mechanism unchanged; value semantics change from Role to caps |

**Metadata bypass disposition (ADR 0028 interaction).**

- *Phase 1*: the global-Admin arm of `caller_may_access_graph` remains, scoped explicitly to
  control/metadata reads only — never the data plane — and logged as an elevated access.
  Grant storage includes a dormant `expires_at` field from day one so later time-boxing is
  not a destructive schema change.
- *Phase 2* (**landed 2026-08-25** via [ADR 0080]): the implicit bypass arm is deleted. Operator
  metadata access is now time-boxed grants over resource scopes `GraphMetadata(graph_id)` /
  `ControlPlane` with operation `ReadMetadata`, issued through a JIT loop: justified request →
  approval by a second distinct caps holder (later k-of-n governance; grant representation
  unchanged) → evidence-complete issuance (the grant row IS the record) → elevated use inside
  the window → automatic expiry at `expires_at` → post-use review over the retained rows.
  Self-elevation without approval exists only as an explicit `EMERGENCY_ELEVATE` cap whose rows
  are flagged emergency in introspection; silent bypass paths do not exist. Expired-grant GC is
  designed together with the audit-log retention contract.

**`CALL` gating.** Named `CALL` stays conservative under `admin_caps(CALL_PROCEDURE)` in
Phase 1: today's only procedures are synchronous platform operations
(`GLEAPH.FINALIZE_*`, `GLEAPH.DRAIN_DEFERRED_MAINTENANCE`) executed on the mutation path,
no procedure catalog exists yet to attach data-plane grants to, and a caps requirement
preserves today's semantics (unknown principals cannot run procedures). Because `PUBLIC`
holds data-plane grants only, public programs containing `CALL` fail statically —
administrative surface cannot be published. When stored-procedure bundles become registered
catalog objects (future ADR; the execution-model work precedes it), `(EXECUTE,
Procedure(id))` becomes a data-plane resource and the caps bit narrows to a management
capability (`MANAGE_PROCEDURE`), mirroring the `PREPARE_REGISTER` vs `EXECUTE PreparedQuery`
split.

**Public prepared queries (caller-bounded publication).** There is no `visibility=public`
flag. Publishing is an ordinary grant:

```gql
GRANT EXECUTE ON PREPARED QUERY <query-name> TO PUBLIC;
```

gated by (1) grant authority (registry owner of the query's resolved graph or
`PREPARE_REGISTER` caps holder) and (2) invariant 7 below. Revocation is symmetric
(`REVOKE ... FROM PUBLIC`); runtime merging remains caller ∪ PUBLIC under SECURITY INVOKER.

### 2. Privilege set (Phase 1)

```text
Privilege {
    operation   : MATCH | TRAVERSE | READ | CREATE | UPDATE | DELETE
    resource    : VertexLabel(label_id) | EdgeLabel(label_id)
                | VertexProperty(label_id, property_id)          // READ_PROPERTY only
    direction?  : OUTGOING | INCOMING                              // EdgeLabel TRAVERSE only
}
```

- **Direction is part of the privilege**, expressed as logical graph semantics
  (`OUTGOING = source → target`), independent of physical LARA forward/reverse orientation
  (ADR 0048/0050). An undirected-pattern match over a directed label requires both
  directional privileges and otherwise fails authorization.
- Undirected edge labels reject directional modifiers at grant-validation time; omitted
  direction on a directed label means BOTH.
- **TRAVERSE ≠ READ ≠ READ_PROPERTY.** An edge may contribute to reachability without being
  readable; property-level read grants are enumerated per label. A query that explicitly
  projects an unauthorized property fails; unauthorized properties are never substituted
  with NULL.
- Wildcard label traversal is not granted in Phase 1; grants enumerate labels.

### 3. Invariants

1. **No implicit data access from authority**: effective-privilege expansion never unions
   `admin_caps` into data-plane reads/writes. Administrative capability reaches data only
   through an explicit grant chain rooted at the graph issuer. Break-glass support access, if
   added later, is a time-boxed explicit binding with mandatory audit events.
2. **Anonymous can never hold a stored privileged row** (existing `crates/auth` invariant);
   `PUBLIC` is a virtual subject resolved at evaluation time, never a persisted principal.
3. **Ownership is the implicit root of data-plane authority** (amended 2026-08-24):
   `GraphRegistryEntry.owner` remains the identity anchor, but ownership is **never
   materialized as grant rows**. The owner's full authority over their graph is evaluated at
   enforcement time directly from the registry (the ownership coverage arm of privilege
   evaluation). Literal seeding of `{issuer → full data-plane grants}` at graph creation —
   the original wording of this invariant — is rejected: label/property vocabularies are
   dynamic, so seeded rows would force wildcard rows (ungrantable in Phase 1) or write
   amplification on every catalog change, and would duplicate ownership knowledge into a
   second independently mutable record (SSOT violation). Because no row expresses it,
   introspection must surface this implicit authority explicitly: grant listings synthesize
   an implicit-root entry for the owner instead of presenting an apparently empty list.
4. **Catalog monotonicity**: vertex-label, edge-label, and property IDs are never reused
   after DROP. Dropping a label/property cascades invalidation of grants referencing it.
   Grants reference IDs, resolved at grant validation time.
5. **Admission is not authorization**: compute/cost admission lives behind a single seam
   owned by the account/provision layer (future billing ADR). Privilege evaluation never
   consults balances or payment state.
6. **Sovereignty boundary honesty**: RBAC separates authority *within* the system only. The
   controller plane (wasm upgrade) remains physically absolute until cryptographic enforcement
   (VetKeys track); this ADR must not be cited as tenant confidentiality against controllers.
7. **PUBLIC never exceeds its publisher**: creating an `EXECUTE PreparedQuery → PUBLIC`
   grant requires the granter to hold every privilege in the query's statically extracted
   requirement set. The PUBLIC baseline is therefore bounded by what the publishing
   principal could reach themselves, closing privilege escalation through republication.

### 4. Enforcement point and failure semantics

- Authorization moves to **plan time on the Router**: every referenced element class,
  direction, operation, and projected property in the logical plan must be covered by the
  caller's effective privileges (grants ∪ PUBLIC ∪ ownership-derived). The classification
  pipeline still runs first to map program shape onto required operations.
- Prepared queries statically extract their required privilege set into the prepared record
  (destructively redefined V1; fresh state required) and execute **SECURITY INVOKER**: caller
  grants merged with PUBLIC grants at invocation. `SECURITY DEFINER` is rejected for now.
- Failure returns a uniform generic authorization error that does **not** name the missing
  privilege or resource (existence non-disclosure aligned with [ADR 0028]); diagnosis is the
  job of the now-implemented privileged-only
  [EXPLAIN AUTHORIZATION](0084-explain-authorization-diagnosis.md) ([ADR 0084]), which leaves
  this failure contract byte-for-byte unchanged.
- Conditional policies (owner/visibility predicates, `MSG_CALLER()` resolution, constant
  pushdown) are out of scope here — separate follow-up ADR. Structural-privilege failures
  remain hard errors; only resource-level policies may filter result sets.

### 5. Grammar subset (Phase 1)

GQL-standard-shaped statements, parsed in the general-purpose GQL crates; Gleaph-specific
literals (`PRINCIPAL`) and functions stay integration-layer concerns per ADR 0034 style:

```gql
GRANT <privilege> ON GRAPH <graph> <resource-selector> TO <subject>;
GRANT EXECUTE ON PREPARED QUERY <query-name> TO <subject>;   -- subject may be PUBLIC
REVOKE ... FROM <subject>;
<subject> ::= PRINCIPAL <literal> | PUBLIC
```

Grant/revoke on a graph requires being that graph's registry owner (Phase 1; delegation of
grant administration comes later).

### 6. Stable layout

The Router replaces the `crates/auth` principal→role stable map with principal→`admin_caps`
and new grant collections, each on its own `MemoryId` per [ADR 0007] allocation policy
(exact MemoryIds assigned in the implementation slice). Versioned record encodings; fresh
state on deploy; no migration shims.

## Consequences

Positive:

- Multi-tenant sharing of one graph becomes expressible (per-label, per-direction, per-property).
- Every data access traces to an issuer-rooted grant chain — auditable under DAO governance.
- Hidden authorization edges work: traverse-without-read prevents reverse enumeration.
- The public prepared-query product flow survives default-deny via PUBLIC grants instead of
  a permissive default role.
- Admission/billing evolution (cycles prepaid ledgers, fiat rails) never touches the
  privilege model.

Trade-offs accepted:

- Two-plane configuration burden for simple deployments (mitigated later by preset sugar).
- No admin bypass: support/debugging of "cannot see my own data" incidents requires either
  temporary explicit grants or
  [EXPLAIN AUTHORIZATION](0084-explain-authorization-diagnosis.md) (implemented, [ADR 0084]).
- Uniform error messages cost diagnosability by design.
- Direct grant rows scale linearly with principals×resources; named roles deferred, not free.

## Migration

Destructive, pre-production clean replacement:

- `crates/auth`: remove `Role` ladder and rank logic; introduce `AdminCaps` bitflags and
  grant storage; keep `validate_bootstrap_principals` semantics.
- `crates/router/src/rbac.rs`: `authorize_adhoc_gql` replaced by plan-time privilege
  checking wired into plan construction; DDL gates move onto `admin_caps`.
- CLI/SDK surfaces for `grant_role` change shape (breaking; SDK regenerated).
- PocketIC E2E authorization suites rewritten for grants/PUBLIC; adversarial tests must walk
  the ingress surface asserting deny-by-default plus one success path per handler family.
- `design/security/rbac-and-prepared.md` rewritten in the same patch that lands the code.

## Phases

- **Phase 1 (this ADR)**: admin_caps, direct per-graph grants, PUBLIC, directions,
  plan-time enforcement, prepared static extraction, `CALL` under the `CALL_PROCEDURE` cap,
  caller-bounded PUBLIC publication of prepared queries, grammar subset, destructive
  migration. Grant rows carry a dormant `expires_at` field from day one.
- **Phase 2**: conditional resource policies; router-resolved `MSG_CALLER()` constants pushed
  into plans/index scans; authorization-aware vector search; JIT completion — delete the
  remaining ADR 0028 Admin metadata bypass in favor of time-boxed
  `GraphMetadata`/`ControlPlane` grants issued through the approval loop (§1b).
- **Deferred ADRs**: ReBAC/policy traversal (delivered by [ADR 0082]); internal-caller
  allowlist contract write-up (documents current guards); ~~audit log (bounded-append
  contract and expired-grant GC)~~ expired-grant GC delivered by [ADR 0083] in the grant
  store, with the bounded-append audit-log contract itself still deferred until DAO
  governance is designed; admission/billing; stored-procedure bundles and
  canister-calling procedures (execution model first; then `EXECUTE Procedure(id)`
  downgrades the caps gate).

## Alternatives considered

- **Keep ladder + ADR 0028 ACL, add nothing**: rejected — cannot express shared-graph
  visibility, which is the demonstrated product need.
- **Original proposal's `Principal → RoleBinding → Role → Privilege` with named roles and
  Tenant scopes**: rejected for Phase 1 — roles/Tenants duplicate ownership knowledge
  (SSOT violation vs registry.owner) before any demonstrated need; direct grants preserve
  the same expressiveness with fewer concepts.
- **Explicit DENY precedence**: deferred — breaks additive composition and cacheability;
  ALLOW-only + default deny covers current use cases.
- **Shard-side capability validation for router-bug containment**: rejected — shards trust
  the Router plus registered internal callers; introducing signed contexts invades every wire
  protocol without a demonstrated threat. Revisit only if the trust model changes.
- **Canister-global fine-grained privileges without graph scoping**: rejected — grants must
  be graph-scoped because labels/properties are graph-local catalogs (ADR 0018).

## Design documentation impact

- Supersedes the role-hierarchy and default-role sections of
  `design/security/rbac-and-prepared.md` upon acceptance; that document is rewritten when
  Phase 1 lands.
- Follow-up ADRs to open: conditional policies & pushdown; admission/billing seam; audit log;
  ReBAC. Each links back here.

[ADR 0007]: 0007-stable-memory-layout.md
[ADR 0018]: 0018-graph-scoped-label-property-catalogs.md
[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0034]: 0034-gleaph-gql-extension-syntax.md
[ADR 0048]: 0048-lara-counterpart-resolution.md
[ADR 0050]: 0050-lara-traverse-read-api.md
[ADR 0082]: 0082-rebac-bounded-exists-traversal.md
[ADR 0083]: 0083-authorization-audit-log.md
