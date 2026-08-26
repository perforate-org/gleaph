# 0084. EXPLAIN AUTHORIZATION: privileged diagnosis of requirement coverage

Date: 2026-08-26
Status: proposed
Last revised: 2026-08-26

## Context

[ADR 0074] §4 fixed the failure contract of data-plane enforcement: an uncovered demand
fails with a uniform `Forbidden` that never names the missing privilege or resource —
existence non-disclosure inherited from [ADR 0028]. That choice protects the
authorization topology from probing, and [ADR 0074]'s Consequences accepted its price
explicitly: support and debugging of "cannot see my own data" incidents require either
temporary explicit grants or "the future `EXPLAIN AUTHORIZATION`".

The need is now demonstrated rather than hypothetical: deny-by-default surprises are the
commonest friction reported while building shared-graph flows (prepared publication,
conditional policies, and now [ADR 0082] relationship gates), and the only diagnosis
paths today are reading code/tests plus owner-side `list_graph_grants` introspection.
The question "why can't caller X run program P on graph G?" has no direct answer anywhere
in the system ([GAP-2026-08-26-002]).

This ADR designs that diagnosis surface. The constraint it must honor is absolute:
**adding explanation must not weaken the uniform-error property of execution**.

## Existing architecture assessment

Preserved as-is:

- **Uniform non-disclosure on every execution path** ([ADR 0074] §4): unchanged by this
  ADR; explanation is additive, never a flag on dispatch.
- **Requirement machinery already exists as enforcement internals**
  (`crates/router/src/authz.rs`): exhaustive `walk_ops`/`walk_op` extraction producing
  `RequirementSet` (per-graph conjunctive demands, alternative groups, unattributed
  residue) and the single `requirements_cover` evaluation seam over
  `StoredGrants` (expiry-aware caller ∪ PUBLIC) plus the ownership root. Diagnosis is
  these two functions running in report mode instead of verdict mode.
- **Prepared records store their static requirement set** ([ADR 0074] slice 3): the
  primary checked artifact, with the live walk retained as fail-closed fallback for
  catalog drift — the same duality serves explanation.
- **Introspection precedent**: `list_graph_grants` shows a graph's grant rows to its
  owner/admins; visibility rules (tenancy ∪ grant-derived ∪ elevation, strangers get
  indistinguishable `NotFound`) are settled in `caller_may_access_graph`.
- **Grammar extension precedent** ([ADR 0034] style): Gleaph-specific statement forms
  land behind the `gleaph` feature gate in the general-purpose crates.

Missing machinery: nothing computes or renders the join between a program's requirement
set and coverage; no statement type is classified as a pure diagnostic.

## Decision

### 1. One statement, two asker modes

```gql
EXPLAIN AUTHORIZATION FOR PREPARED QUERY <name>;              -- self mode (default)
EXPLAIN AUTHORIZATION FOR PREPARED QUERY <name> BY <principal>; -- owner mode
```

- **Self mode** (default, `BY` omitted): the asker explains their own coverage. Answers
  "why was I denied / what covers this query for me".
- **Owner mode** (`BY <principal>`): restricted to tenants (owner/admins) of **every**
  graph the requirement set touches; explains another principal's coverage. Justified:
  the owner already sees every grant row of their graph via `list_graph_grants`, so the
  join discloses no new information class — it automates a derivation they can perform
  by hand.
- Anything else is refused. In particular, **caps confer no explain authority**
  ([ADR 0074] invariant 1): `MANAGE_AUTHORIZATION` holders get no cross-graph,
  cross-principal diagnosis — cross-tenant operational need flows through time-boxed
  `ReadMetadata` elevation exactly as content access does.

### 2. Subject of explanation: prepared queries only in v1

The named prepared record supplies its stored requirement set (live walk as fail-closed
fallback for catalog drift, mirroring enforcement). Explaining **ad-hoc inline programs
is deferred**: accepting arbitrary untrusted text would put parse+plan on the diagnosis
path, contradicting the prepared-first hot-path posture, and the sanctioned route for
shared programs is registration anyway. Deferral recorded in Later Slices.

### 3. Authority is visibility, not privilege

The statement is classified as a **pure diagnostic read**: it never dispatches the
explained program, never mutates state, and spends only walker/render instructions. Its
own authorization is:

- resolve the prepared record; resolve every graph its requirement set touches;
- self mode: asker must hold visibility of every such graph under the settled
  `caller_may_access_graph` arms (tenancy ∪ grant-derived ∪ elevation);
- owner mode: asker must be tenant of every such graph;
- any invisible graph ⇒ the statement fails with the **same indistinguishable
  `NotFound`** used everywhere else — the uniform-honesty rule applies to the
  diagnostic tool itself, so `EXPLAIN` cannot be turned into an existence oracle.

### 4. Coverage semantics mirror enforcement exactly

Report rows are produced by the same evaluation the enforcer would apply, rendered
instead of judged:

- conjunctive demands: each listed with its coverage source;
- alternative groups: rendered as "any-of" with per-arm sources;
- unattributed residue (unlabeled scans, unresolved names): rendered as "requires graph
  tenancy (owner/admin)" — matching the tenancy-only attribution table;
- expired rows are absent, exactly as in enforcement;
- metadata-plane demands (`ReadMetadata`) are covered by the same machinery and included
  when present;
- conditional-policy predicates ([ADR 0075]/[ADR 0082]) **do not appear**: policies
  constrain outputs after coverage and produce absence, never denial — there is nothing
  to explain structurally.

### 5. Redaction rules (the existence-leak boundary)

Coverage sources differ per mode:

- **Self mode** sources can only ever be {asker's own rows, `PUBLIC` rows, tenancy
  root} — another principal's grants never cover anyone else. The report names the
  source class (`your grant` / `PUBLIC grant` / `graph tenancy`) and, for own rows, the
  row identity; it **never names other principals**.
- **Owner mode** renders full row identities: the asker already sees every row of their
  graph, so redaction would be theater.
- Under both modes, requirement items name only resources inside graphs whose visibility
  the asker passed in §3 — enforced by construction because an invisible graph aborts
  before rendering.

This resolves [GAP-2026-08-26-002]'s three questions: (a) self + owner-about-others
only; (b) self-diagnosis may name uncovered resources **on visible graphs only** —
labels of a program on a visible graph are already disclosed by the program/query
itself, so the join leaks nothing beyond what asker-visible inputs contain; (c) full
coverage detail with the source-class redaction above.

### 6. Invariants

1. **Execution paths are byte-for-byte unchanged**: no dispatch flag, no altered error,
   no added log line on the enforcement route.
2. **No new authority**: explain authority derives exclusively from settled visibility
   arms; caps and elevations grant no additional explain reach (elevations help only by
   making a graph visible, as they already do for admission).
3. **Report-only**: the diagnosis path cannot mutate stable state, cannot execute the
   explained program, and is bounded by the ordinary instruction budget of a read.
4. **Uniform honesty extends to the tool**: insufficient visibility ⇒ indistinguishable
   `NotFound`; the diagnostic never becomes an existence oracle.
5. **Fail-closed reuse**: requirement extraction keeps the exhaustive-match discipline;
   a future planner variant fails compilation until the report path knows it.

## Consequences

Positive:

- Deny-by-default becomes operable: owners diagnose collaborator failures, developers
  diagnose their own access, without temporary-grant trial-and-error.
- Zero new server trust surface: two existing functions run in report mode behind
  settled visibility rules; no stable-layout change and no new endpoints.
- Closes [ADR 0074]'s acknowledged diagnosability debt without touching its
  non-disclosure guarantee.

Trade-offs accepted:

- Security-critical code grows a second consumer (report mode) of the walker — mitigated
  by the exhaustive-match compilation discipline and report-only tests proving
  non-mutation/non-execution.
- Self-mode redaction means an asker sees source *classes*, not which concrete row of a
  multi-row union matched first — sufficient for diagnosis, less forensic than owner
  mode.
- Ad-hoc (unregistered) programs are not explainable in v1.

## Alternatives considered

- **Candid endpoint instead of a statement**: rejected — duplicates ingress, skips the
  grammar/classification seam every other surface uses, and fragments the client story;
  the statement form rides existing transport and formatting precedents.
- **Allow `MANAGE_AUTHORIZATION` holders to explain anyone, anywhere**: rejected —
  recreates standing cross-tenant privilege through the diagnostic door ([ADR 0074]
  invariant 1); the honest route for legitimate cross-tenant need is an elevation that
  makes the graph visible.
- **Dry-run execution mode** (execute the plan with writes suppressed): rejected — pays
  real shard instructions to answer a question the requirement set already answers
  statically, and drags execution-path complexity into a diagnostic.
- **Inline ad-hoc programs in v1**: deferred — untrusted parse+plan on the diagnosis
  path conflicts with the prepared-first posture; revisit after demand appears.
- **Uncovered-only minimal reports** (never name covering rows): rejected — the whole
  value is the coverage join; redaction (§5) already bounds disclosure to source classes
  the asker owns or that are public.

## Migration

Additive; **no stable-layout change and no fresh-state requirement**:

- Grammar: statement parsed behind the `gleaph` feature gate; classification marks it
  diagnostic-read (non-DML, non-catalog-modification).
- Router: report-mode entry points wrapping `extract` + `requirements_cover` with the
  §3 authority gate and §5 rendering; prepared-record resolution reused from the
  execution path.
- New PocketIC suite `adr0084_explain_authorization.rs`: owner-explains-collaborator
  (full identities), self-explain redaction matrix (own/PUBLIC/tenancy sources),
  invisible-graph ⇒ indistinguishable `NotFound` (existence-oracle probe),
  execution-path uniform `Forbidden` byte-identical before/after (regression guard for
  invariant 1), alternatives/unattributed rendering, expired-row absence, adversarial
  deny-by-default walk — stating expected counts before running.
- Docs in the landing patch: `design/security/rbac-and-prepared.md` (new EXPLAIN
  section), `design/gql/extension-syntax.md` (statement surface), and
  `design/implementation-gaps.md` GAP-2026-08-26-002 flipped to Resolved with the
  fixing commit.

## Design documentation impact

- Fulfills the reserved hook in [ADR 0074] §4 and its Consequences trade-off; that text
  gains a pointer here on acceptance.
- Extends the statement surface documented in [ADR 0034]-style dialect terms.
- Feeds future operator tooling: `gleaph auth` affordances (deliberately deferred) would
  naturally wrap this statement alongside grant management.
- Deferred: inline-program explanation; k-of-n governance reporting views;
  history/explain joins with the deferred audit stream.

[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0034]: 0034-gleaph-gql-extension-syntax.md
[ADR 0074]: 0074-data-plane-authorization-core.md
[ADR 0075]: 0075-conditional-policies-constant-pushdown.md
[ADR 0082]: 0082-rebac-bounded-exists-traversal.md
