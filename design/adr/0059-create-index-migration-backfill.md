# 0059. CREATE INDEX migration backfill lifecycle

Date: 2026-08-03
Status: accepted
Last revised: 2026-08-20
Anchor timestamp: 2026-08-22 18:49:58 UTC +0000
Implementation status: Partially implemented. The versioned artifact/wire, Router durable
lifecycle and migration ledger, Active-only planner gate, Graph canonical export scope,
graph-index build worker/state, and the production Router cross-canister driver with seal/drain
composition are implemented. Graph label gain/loss admission now uses the same exact
label-scoped Building/Sealing fence as property DML, with six focused Graph regressions (including
the public mutation-id-0 wrapper boundary). Focused
PocketIC E2E and upgrade validation are not yet run and pre-release artifacts/ledgers are not yet
regenerated.

## Context

ADR 0058 allows one or more additive catalog statements per migration but deliberately keeps
`CREATE INDEX` as exactly one statement per artifact, because its catalog registration and
historical posting backfill are separate operations. Registering an index before its existing
canonical values have converged can expose false-negative index reads. The current backfill
endpoints also do not provide one lifecycle that covers vertex values, edge sidecar values, and
edge `INLINE` values together.

Multi-index consolidation keeps `CREATE INDEX` outside the GQL statement grammar: the vendor
index DDL (`gleaph_index_ddl`) gains `NEXT` chaining so one migration payload may carry several
`CREATE INDEX` statements that share one graph selector and one migration record. Each statement
builds through the same per-index lifecycle, driven sequentially in payload order.

The ownership boundaries are already clear: Router owns the index catalog, graph shards own
canonical vertex/edge/property storage, and graph-index owns posting storage and posting reads.
This ADR adds an online build protocol at those boundaries. It does not move canonical storage
into graph-index or make Router a posting scanner.

## Problem

A migration must create an index over live data without a long graph-wide write pause, while
preserving all of the following:

- an unambiguous graph target and an immutable execution identity;
- a durable, resumable build that cannot publish a partial posting set;
- one export contract for vertex properties, edge sidecars, and edge `INLINE` values;
- protection against stale Router catalog snapshots and old build workers;
- a short, bounded seal rather than full quiescence; and
- planner behavior that never treats a non-converged generation as an index.

## Existing architecture assessment

The Router currently records named index definitions and supplies an ephemeral indexed-property
catalog to Graph operations (ADR 0023). Graph DML and maintenance already have durable outbox and
repair paths, while graph-index owns the posting map. Those concepts can absorb an index build if
the build state and generation are explicit. A second canonical scan service, a Router-side copy
of Graph storage, or an independent posting registry would weaken those ownership boundaries.

The implementation now has a Router-owned durable lifecycle, a single Graph canonical export API
covering vertex, sidecar, and exact `INLINE` projections, a graph-index-owned bounded pull worker,
a migration selector/resolved-target record, and the production Router driver that composes the
live Graph scope/seal controls with graph-index register/advance/seal/cleanup. The production
driver is wired into the control plane (`apply_schema_migration`); the remaining gap is focused
PocketIC E2E and upgrade validation, which is the pre-release gate before production deployment.

## Decision

### Scope

This ADR defines the first migration-driven `CREATE INDEX` only. Rebuild, replacement of an
existing logical index, and drop/recreate workflows require a later ADR; they are not represented
by an extra state or a second active generation here. A logical index/name/property conflict with
an existing catalog entry is a `Preparing` preflight rejection before effects.

### Migration target and execution identity

The planned index-migration artifact is a breaking pre-release extension of the migration package.
It contains one or more literal `CREATE INDEX` statements chained with `NEXT` and an optional
top-level `graph` selector that applies to the whole payload:

```toml
format_version = 1
id = "000002_person_age_index"
description = "Build the Person.age index"
parent = "000001_init_graph"
# graph = "social"       # omitted means the Default selector
```

The omitted selector is the canonical `Default` variant. An explicit value is the canonical
`Named(name)` variant. The checksum extends ADR 0058's framed checksum with that selector variant
and its UTF-8 name before the raw `up.gql` bytes; omission is therefore not interchangeable with
an explicit graph name. Human description and filesystem paths remain outside the checksum.

Router resolves the selector exactly once in `Preparing` and persists the selector, resolved
`GraphId`, and resolved graph name in the pending/terminal migration record. A missing, ambiguous,
or otherwise unresolvable `Default` selector fails before any catalog, `PhysicalIndexId`, cursor, outbox,
or posting effect. Retries use the persisted `GraphId`; they do not resolve a mutable name again.

Every statement in the payload resolves against that single graph and the same topology snapshot.
The pending ledger state addresses one build pointer per statement in payload order:
`PendingIndex { pending: Vec<PendingIndexBuild> }` with each `PendingIndexBuild` carrying the
`index_name_id` and `PhysicalIndexId` of one catalog lifecycle row. The per-index lifecycle rows
remain the durable owners of each build; the migration record only addresses them.

The index migration lane is one parent-ordered linear child at a time. Router rejects a second
pending index migration (including a fork or a different payload) until the existing one is fully
`Active` or its `Aborting` cleanup is complete. Index migration identity is the artifact id,
parent, checksum, selector, target `GraphId`, and every logical index identity and `PhysicalIndexId`
in the payload.

### Target and build generation

`Preparing` records the resolved `GraphId`, immutable logical index identity (`IndexId`/name and
property scope), the target shard set and topology epoch, the Router catalog epoch, and one
monotonically allocated `PhysicalIndexId`. `PhysicalIndexId` is both the build generation and the
posting namespace; it is never reused. Router metadata may retain the logical `IndexId`/name, but
posting and outbox keys carry only `PhysicalIndexId`, not a second `(IndexId, build_generation)`
pair. An abandoned physical namespace cannot write into a later build.

### Lifecycle

The durable lifecycle is:

| State       | Owner and invariant                                                                                                                                                                                                                                                                            | Allowed transition                                                       |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `Preparing` | Router authenticates the caller, validates the migration chain and index definition, resolves the selector, captures the target shard/topology epoch, allocates the `PhysicalIndexId`, and validates catalog conflicts. No planner or posting read may use the namespace.                      | `Building`, or `Aborting` on a recorded failure.                         |
| `Building`  | Graph-index pulls canonical pages from each target Graph, writes `PhysicalIndexId` postings, and durably records the touched-subject set and continuation. Concurrent Graph writes are represented by exact namespace-tagged outbox operations.                                                | `Sealing`, or `Aborting`.                                                |
| `Sealing`   | Router advances the catalog epoch as a stale-request fence. A short target-namespace seal rejects affected-index DML retryably before dispatch or canonical mutation, while reads and unaffected writes continue; it drains already-admitted exact outbox work through its recorded watermark. | `Active`, or `Aborting`.                                                 |
| `Active`    | Router atomically publishes the `PhysicalIndexId` pointer and planner capability after the build and outbox watermark prove convergence. Only this state is exposed as an index to planning and query routing.                                                                                 | A future explicit rebuild or drop; never an implicit partial activation. |
| `Aborting`  | The failure reason, `PhysicalIndexId`, and cleanup cursor are durable. The namespace is never planner-visible; this first-create migration leaves no logical index active. Cleanup and retries are idempotent until the namespace is removed and the migration can terminate.                  | Cleanup completion; a later explicit migration may then start.           |

### Online build and ownership

1. In `Preparing`, Router writes only the pending lifecycle/catalog metadata after all pure
   validation succeeds. It does not scan Graph storage. An existing logical index/name/property
   conflict is rejected here; this ADR has no replacement path.
2. In `Building`, graph-index owns the pull loop. For each target Graph shard it calls one
   Graph-owned opaque-cursor export API. The API is the only path graph-index may use to read
   canonical values; graph-index never opens Graph stable memory or reconstructs storage keys.
3. The export API returns canonical indexable records for vertex properties, edge property-store
   sidecars, and edge `INLINE` properties. The cursor token hides the storage layout and can
   represent a full scan or a resume point. Graph owns cursor validation, ordering, source
   precedence, and upgrade-stable encoding.
4. Every pending-index DML/outbox record is an exact operation. After full validation, one
   synchronous graph-index message atomically persists the canonical subject in the
   `PhysicalIndexId` touched-subject set **first** and applies the exact namespace-scoped posting
   mutation; commit and rollback cover both. The Graph outbox retains the operation until ack, and a
   lost response retries both idempotently.
5. During `Building`, the base-seed callback serializes the touched check with its candidate write
   and writes a pulled record only when the subject is not touched. If a DML race wins first, the
   seed skips the subject; if the seed wins first, the later touched-first exact mutation overwrites
   it. No newer mutation can be overwritten and no second canonical scan is needed.
6. In `Sealing`, Router increments the catalog epoch and rejects old `PhysicalIndexId`/epoch
   requests.
   A short per-index seal fence rejects affected-index DML retryably before dispatch or canonical
   mutation; reads and unaffected writes continue. Graph-index drains only already-admitted exact
   outbox work through the recorded watermark and reports a converged namespace. After `Active`,
   Router or the caller exact-replays a rejected DML with the
   same idempotency envelope. This is not full graph quiescence: ordinary reads continue, and the
   canonical Graph remains the source of truth.
7. Router publishes the `PhysicalIndexId` pointer and `Active` state only after the convergence
   proof is durable. Planner statistics and index seeds include only `Active` namespaces;
   `Preparing`, `Building`, `Sealing`, and `Aborting` always fall back to a non-index plan.
   A Router trap after remote activation but before the durable convergence persist is resumable:
   re-driving the same exact seal envelope treats an already-`Active` scope under the same frozen
   identity and lifecycle epoch as an exact replay, so the drive converges without re-scanning and
   `Active` remains deliberately non-abortable and non-removable.

### Graph label-transition admission

Graph remains the canonical owner of vertex labels and properties. For a label gain or loss, the
Graph label coordinator resolves every affected property from the vertex's canonical label set and
selects only the exact `(label_id, property_id)` namespaces supplied by the Router catalog. It
preflights the complete transition before any label, row, pending-queue, or outbox write. A
`Building` namespace receives one exact touched-first `BuildDml` envelope under the caller's
mutation identity; an `Active` namespace receives the ordinary property posting; a `Sealing`
namespace rejects the label mutation before canonical state changes. Label postings remain owned by
the ordinary label-pending path. This boundary is covered by the Graph gain/loss and delete
single-emission regressions; cross-canister lifecycle and upgrade/reopen proof remain the pending
pre-release gate.

### Catalog epoch and stale-request fence

Every Graph export, graph-index posting batch, outbox drain, and lifecycle transition carries the
expected `catalog_epoch`, target `GraphId`, logical index identity, and `PhysicalIndexId`. Router
advances the epoch at preparation and sealing. Graph-index and Graph reject a mismatched epoch or
namespace without changing stable state. A worker that receives a stale error must reread the durable
lifecycle; it may resume the recorded `PhysicalIndexId` or enter `Aborting`, but it may not allocate
a replacement `PhysicalIndexId` implicitly.

### Multi-index migrations

A payload with several `CREATE INDEX` statements is driven as a sequence of independent sub-builds
in statement order. `Preparing` parses and preflights every statement (name conflicts, definitions,
inline projections) before the first write, then co-writes one catalog lifecycle row per statement
plus one pending ledger record in a single synchronous message. Each apply round advances the
first non-`Active` sub-build by one bounded step; the response progress carries the advancing
sub-build's phase and target counts plus its ordinal (`active_index`) and the payload's total
(`total_indexes`). The migration reaches `Applied` only when every sub-build is `Active`.

Failure is partial by construction: `Active` is deliberately non-abortable and non-removable, so a
terminal failure of sub-build `k` leaves sub-builds `1..k-1` `Active`, drops the failed build's
catalog row, and removes the not-yet-started `Preparing` rows so no orphaned lifecycle state
remains. The migration record becomes `Failed`. As in the single-index contract, index names stay
interned after a failure, so a follow-up migration must use fresh index names; the failed migration
remains the linear chain parent.

### Failure, retry, upgrade, and new-shard rules

- Selector, authorization, checksum, parser, catalog-conflict, target-shard, and pending-lock
  failures are preflight failures. In particular, an unresolved `Default` selector has no partial
  effect.
- A failed multi-index migration leaves earlier `Active` sub-builds in place and releases the
  remaining lifecycle rows; see [Multi-index migrations](#multi-index-migrations).
- A lost response or transient inter-canister error replays the same page, outbox batch, or state
  transition. The durable cursor advances only after the `PhysicalIndexId` write and touched-set
  update; namespace-scoped upsert/removal is idempotent. An ambiguous failure is not treated as a
  new migration id or physical namespace.
- Router, Graph, and graph-index upgrades reopen the durable lifecycle, cursor, touched set,
  `PhysicalIndexId` metadata, and outbox. A non-`Active` namespace is fail-closed to the planner until
  its recorded convergence proof is restored. Volatile worker state is disposable.
- The target shard set and topology epoch are fixed in `Preparing`. Adding or removing a shard
  while the migration is pending invalidates that `PhysicalIndexId` and moves it to `Aborting`; the
  operator must retry as a new linear child after capturing the new topology. No shard is silently
  appended to a completed build.
- A graph with an `Active` index rejects any new-shard registration or removal before the topology
  epoch changes. A separately specified topology/index rebuild protocol must make the new topology
  complete before registration is allowed; ADR 0059 provides no implicit prepare-plus-topology
  change, full scan, or compatibility fallback.
- If the first build aborts, the graph uses the existing non-index plan. Aborting cleanup is
  bounded and resumable; a new migration cannot start while its cleanup cursor remains. Replacement
  and rebuild behavior are intentionally out of scope for this first-create ADR.

### Complexity and durable footprint

Let `S` be the target Graph-shard count, `K` the graph-index target count, `N_s` the canonical
indexable records on target shard `s`, `B` the bounded export page, `T` the deduplicated
touched-subject count, and `U` the exact outbox work at seal. Graph-index makes

```text
sum_s ceil(N_s / B) + ceil(U / B)
```

bounded Graph↔graph-index page/batch calls. Router fanout and lifecycle control are `O(S + K)` per
transition, not a constant-cost operation hidden behind one Router call. Each request and response
is capped by the page/item byte bound; Router never carries a full canonical dataset. Stable state
is `O(N + T + U)` for `PhysicalIndexId` postings, touched subjects, and durable outbox work, plus
`O(S)` lifecycle/cursor metadata for durable progress per target Graph shard. This first-create path
has no replacement namespace to retain.

### Alternatives considered

| Alternative                                                 | Decision and reason                                                                                                                                                                                                                                                                                                        |
| ----------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Full graph quiescence during the scan                       | Rejected. It makes the consistency proof simple but imposes an unbounded write pause and is not an online index migration; reads need not be paused to reject this option.                                                                                                                                                 |
| Router-driven use of the existing backfill flow             | Rejected. Router currently orchestrates Router→Graph cursor work and Graph→index posting calls; adding that scheduling path to this migration would add orchestration hops without the generation, touched-set, and Active lifecycle. The rejection is about coordination cost, not Router reading Graph storage directly. |
| A new Graph delta log for the build                         | Rejected. It duplicates the existing durable outbox/repair ordering, retention, upgrade, and replay machinery. The `PhysicalIndexId` touched set plus existing outbox is the smaller owner-aligned extension.                                                                                                              |
| Independent shadow index generation and atomic catalog swap | Rejected for this first create. A paired active/pending catalog or second subsystem would double lifecycle and stable-storage ownership; the unpublished `PhysicalIndexId` namespace in the existing graph-index canister provides isolation without a replacement framework.                                              |

### Breaking compatibility boundary

This is a pre-release, breaking design. It keeps `format_version = 1` and replaces the v1
manifest, checksum, wire, and stable shapes in place for the rollout; development snapshots are
discarded. There is no legacy decoder, dual shape, optional-field widening, or version bump solely
for compatibility, and no old shard registry, implicit immediate activation, or fallback from a
failed build to the current `CREATE INDEX` catalog-registration path. Existing pre-release
artifacts and ledgers must be regenerated or reset under that rollout. Existing non-migration
`CREATE INDEX` behavior remains governed by ADR 0009 until a separate change deliberately amends
it; it is not a compatibility implementation of this migration protocol.

## Consequences

Positive:

- Graph and graph-index retain ownership of canonical values, postings, cursors, and repair work.
- A pending `PhysicalIndexId` can build online without planner false negatives or a full graph pause.
- Selector resolution, target identity, epoch fences, and `PhysicalIndexId` records make retries and
  upgrades deterministic.
- One opaque export API prevents duplicate inline/sidecar/vertex scanners and keeps storage layout
  encapsulated. Implemented: the Graph edge-property export enumerates both canonical value
  domains — sidecar `EDGE_PROPERTIES` rows first, then canonical edges carrying indexed inline
  property bytes — under one domain-tagged opaque cursor, so an index on an eligible INLINE
  property converges pre-existing values before activation.

Costs and limits:

- The protocol adds durable lifecycle, `PhysicalIndexId`, touched-set, cursor, and outbox state and a
  bounded short seal fence.
- Build progress and touched-set convergence require additional Graph↔graph-index calls and
  temporary stable bytes; large generations need capacity planning and bounded cleanup.
- Topology changes are deliberately rejected during a pending build and require a new migration.
- Rebuild/replacement of an existing logical index is deliberately deferred to a later ADR; this
  first-create protocol does not retain or swap an older active generation.
- The cross-canister production Router driver is implemented (graph-index build control, Graph
  export-scope lifecycle, and bounded outbox drain) with unit coverage of its exact per-phase call
  ordering against fake downstream clients. The PocketIC E2E proof remains incomplete. Existing unit
  coverage validates fresh/resume/replay, two-shard convergence, exact ambiguous replay, topology
  abort, cleanup-before-failure, the one-pending gate, retryable no-mutation, and Active-only
  publication; those tests are not a deployment claim.

## Migration

The breaking v1 artifact/record replacement, Router lifecycle/epoch fields, graph-index
`PhysicalIndexId` build state, Graph opaque export scopes, durable touched/outbox handling,
Active-only planner projection, bounded cleanup state, and the real Router driver with
seal/drain composition are present. The rollout still requires focused upgrade/PocketIC validation
and regenerated pre-release artifacts/ledgers before advertising production `CREATE INDEX` migration
backfill. No compatibility decoder is part of the work.

## Design documentation impact

ADR 0058 remains the v1 schema-migration contract and now points here for migration index
backfill. ADRs 0009 and 0023 retain their historical index/catalog and repair decisions; their
revision notes point here only for the new migration lifecycle. The property-index and derived-state
documents point to this ADR as the single source of truth for activation gating. The
implementation-gap ledger retains the open P0 backfill and activation gaps and links this ADR.

## References

- [ADR 0009 — Edge property index and index DDL](0009-edge-property-index-and-index-ddl.md)
- [ADR 0023 — Federated index/store consistency](0023-federated-index-consistency-upgrade-compaction.md)
- [ADR 0029 — Shard-local atomicity and cross-canister consistency](0029-shard-local-atomicity-and-cross-canister-consistency.md)
- [ADR 0058 — Versioned additive schema migrations](0058-versioned-additive-schema-migrations.md)
- [Property index design](../index/property-index.md)
- [Derived-state query semantics](../index/derived-state-query-semantics.md)
