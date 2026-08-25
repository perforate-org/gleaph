# 0080. JIT metadata elevation: time-boxed control-plane grants replace the admin bypass

Date: 2026-08-25
Status: implemented
Last revised: 2026-08-25

## Context

[ADR 0028] shipped a global-admin superuser arm in `caller_may_access_graph` so operations,
migration, and tooling could resolve graph topology and schema across tenants. [ADR 0074]
Phase 1 kept that arm under an explicit temporary contract: control/metadata reads only —
never the data plane — and logged as elevated access. Plan 0291 added the dormant
`expires_at` field to grant rows precisely so this phase would not be a destructive schema
change.

Three forces now make the standing bypass the wrong end state:

1. **Governance**: Gleaph's administrative authority may evolve toward DAO governance.
   Accountability then rests on verifiable grant chains, and "admins can implicitly read any
   tenant's topology" is exactly the standing-privilege pattern privileged-access practice
   treats as an audit failure (zero-standing-privilege / just-in-time elevation consensus).
2. **The substrate is complete**: grant rows, expiry-aware evaluation, introspection, the
   PUBLIC pseudo-subject, and caps administration all landed with Plans 0286–0300. Elevation
   can be expressed with existing concepts instead of a parallel mechanism.
3. **Sovereignty positioning**: break-glass must remain inside the system as evidence-
   producing workflow (request → approval → window → review), not as an implicit trust
   relationship or an external PAM product.

## Decision

### 1. New authorization resources and operation

```text
Resource.GraphMetadata(graph_id)   // topology, schema dictionary, shard registry of one graph
Resource.ControlPlane              // cross-graph operational scope (sweeps, fleet tooling)
Operation.ReadMetadata
```

`GraphMetadata` reads are metadata-plane only: they never satisfy data-plane requirements,
and data-plane requirements never satisfy them. The two planes share storage and grammar but
not coverage semantics.

### 2. The bypass arm is deleted; grants are the only elevation

`caller_may_access_graph` loses its global-admin arm. Metadata visibility becomes:

```text
owner | admins            (unchanged; tenant themselves)
| own registered shard    (unchanged; federation)
| holds unexpired ReadMetadata grant for that graph
| holds unexpired ControlPlane-scope grant
```

NotFound non-disclosure for strangers is preserved unchanged ([ADR 0028]); caps holders
without a metadata grant now receive the same treatment as anyone else. Deleted in the same
patch that ships the replacement — no window where neither exists.

### 3. The elevation loop (five stages, each leaves evidence)

```text
request : elevate_request{requester, scope: GraphMetadata(g)|ControlPlane,
          justification (incident reference required non-empty), window}
approve : a second principal holding MANAGE_AUTHORIZATION approves; requester ≠ approver;
          approval recorded on the row
issue   : ReadMetadata grant row written with expires_at = now + window
use     : every authorized metadata evaluation during the window resolves through the row
expire  : automatic at expires_at (existing expired-rows-as-absent semantics); reversion
          needs no human action
review  : the expired row remains introspectable until GC; post-use review is a documented
          operator duty pending the audit-log ADR's retention contract
```

Self-elevation without approval exists only through the explicit `EMERGENCY_ELEVATE`
admin-cap bit: it writes the same row shape with approver = requester flagged emergency and
is surfaced in introspection as such. Silent bypass paths do not exist. Windows come from a
constrained set (1h/4h/24h/7d) to keep the friction real.

### 4. Evidence model: the grant row is the record

No separate audit store in this slice. An elevation row carries requester, approver,
justification, scope, window, and emergency flag — introspection lists active and recently
expired elevations to authorized viewers. The dedicated append-only audit stream, retention,
and tamper-evidence belong to the audit-log ADR, which inherits this row shape as its source
events.

### 5. Grammar and surfaces

Metadata grants ride the existing GRANT/REVOKE statement family behind the same feature
gate, plus two Candid control endpoints (`elevate_request`, `elevate_approve`) gated on
`MANAGE_AUTHORIZATION`. Introspection (`list_graph_grants` family) gains the new resource
kinds. Data-plane statements are untouched.

## Consequences

Positive:

- Invariant 1 becomes exception-free: no path from administrative authority to content
  visibility exists anywhere in the system.
- Every cross-tenant metadata read traces to a request/approval pair with a bounded window.
- DAO evolution swaps only the approver implementation (second caps holder → k-of-n
  resolution); the representation, endpoints, and evidence chain are unchanged.
- Frequency is measurable: elevation rows are countable per operator/graph/period, enabling
  the "a policy bypassed weekly needs revision" feedback loop.

Trade-offs accepted:

- Operators lose always-on cross-tenant visibility; recurring maintenance requires recurring
  (or pre-authorized long-window) grants. Fleet sweeps use ControlPlane windows.
- Incident response has one more step when no second approver is reachable — mitigated, not
  eliminated, by `EMERGENCY_ELEVATE`.
- Expired-row retention interacts with the future audit-log GC; until then rows accumulate
  bounded by elevation frequency (expected low).

## Alternatives considered

- **Keep the bypass permanently, document + log**: rejected — standing implicit elevation is
  the specific pattern privileged-access practice fails audits for, and it caps DAO
  evolution.
- **External PAM/orchestration issuing grants**: rejected — sovereignty; the loop must live
  in-system where its evidence is tamper-bounded by the ledger itself.
- **Session-scoped elevation tokens separate from grant rows**: rejected — two mechanisms to
  revoke/introspect/expire; the grant row already carries every needed field.
- **Do nothing until DAO**: rejected — the bypass is load-bearing today; retrofitting the
  approval loop after operators build workflows on implicit access is harder than shipping
  the loop while usage is small.

## Migration

Pre-production destructive switch, single patch:

- Delete the admin arm; ship resources, gates, endpoints, introspection, tests together.
- Bootstrap seeds designated operators with `MANAGE_AUTHORIZATION`; initial ControlPlane
  windows are granted through the loop itself, not bootstrap (bootstrap keeps granting caps
  only).
- Existing suites asserting admin metadata access are rewritten to run inside approved
  windows; the ingress adversarial walk gains deny-before-elevation cases.
- `design/security/rbac-and-prepared.md` tenancy section updated in-patch.

## Design documentation impact

- Closes [ADR 0074] §1b Phase 2 item "JIT completion"; §1b tables amended to point here.
- Amends [ADR 0028]: the superuser arm is superseded by time-boxed grants; NotFound
  non-disclosure and own-shard arms stand.
- Feeds the audit-log ADR: elevation rows defined as source events; retention/GC contract
  owed there.

## Later slices

None within this ADR. Deferred beyond it: k-of-n governance approver; EMERGENCY_ELEVATE
alerting integration; audit stream with tamper evidence.

[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0074]: 0074-data-plane-authorization-core.md
