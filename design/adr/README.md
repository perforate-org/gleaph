# Architecture Decision Records (ADR)

## Purpose

Capture **significant, hard-to-reverse** decisions with context and consequences. Design docs explain steady-state; ADRs explain *why* we chose it.

## When to write an ADR

- Federation placement authority on router
- Plan blob as execution IR (vs re-parse GQL on graph)
- `PlanRow` dense layout + indexed merge
- Prepared query security model
- Breaking wire format changes

Skip ADRs for routine features, bug fixes, or choices already obvious from code.

## Format

Use numbered files: `NNNN-short-title.md`

```markdown
# NNNN. Title

Date: YYYY-MM-DD
Status: proposed | accepted | deprecated | superseded by NNNN
Last revised: YYYY-MM-DD

## Context
## Decision
## Consequences
## Alternatives considered
```

## Index

| ADR | Title | Status |
|-----|-------|--------|
| [0001](0001-labeled-segment-slide.md) | Labeled edge physical layer uses PMA leaf segment slide | accepted |
| [0002](0002-federated-row-batch-merge.md) | Federated row-batch merge on router (`rows_blob`) | accepted |
| [0003](0003-federated-aggregate-merge.md) | Federated aggregate merge and index fast path | accepted |
| [0004](0004-label-index.md) | Label index: sieve + telemetry; vertex export only when needed | accepted |
| [0005](0005-vertex-identity.md) | Vertex/edge identity: global physical keys and encoded wire ids | accepted |
| [0006](0006-pre-federation-foundation.md) | Pre-federation foundation: ShardId, catalogs, MemoryId, placement | accepted |
| [0007](0007-stable-memory-layout.md) | Stable-memory layout policy and measured consolidation | accepted |
| [0008](0008-edge-inline-value-profile-router-ssot.md) | Edge payload profile schema: router SSOT; retire graph `EDGE_PAYLOAD_PROFILES` | accepted |
| [0009](0009-edge-property-index-and-index-ddl.md) | Edge property index on graph-index; mixed intersection; opt-in `CREATE INDEX` / `DROP INDEX` DDL | accepted |
| [0010](0010-index-sharding-extensibility.md) | Index sharding: defer split strategy; stable posting wire; router index resolution | accepted |
| [0011](0011-gql-graph-resolution-and-catalog-scoping.md) | GQL graph resolution; `BidirectionalCatalog` for graph/index names; migrate stable keys to `GraphId` / `IndexNameId` | accepted |
| [0012](0012-edge-index-direction-in-ddl.md) | Edge index direction in `CREATE INDEX FOR` (GQL patterns); wire label in graph-index keys; planner subset rule | accepted |
| [0013](0013-gql-graph-type-catalog-on-router.md) | Mount `gleaph-graph-catalog` on router; `GraphId`-keyed schema bindings; GQL catalog DDL; planner schema bridge | accepted |
| [0014](0014-graph-type-id-catalog-on-router.md) | Graph type name ↔ `GraphTypeId` catalog on router; migrate `type_map` and `TypeRef` off string keys (0013 follow-up) | accepted |
| [0015](0015-label-stats-projection-log.md) | Label stats projection log and graph mutation journal | proposed |
| [0016](0016-overflow-log-tombstones-and-src-fields.md) | Overflow log tombstones and `src` field layout review | accepted |
| [0017](0017-graph-vertex-existence-ssot.md) | Vertex existence SSOT on graph shard; remove router placement registry | accepted |
| [0018](0018-graph-scoped-label-property-catalogs.md) | Graph-scoped `PropertyId` / label catalogs per `GraphId`; supersede 0011 global vocabulary policy | accepted |
| [0019](0019-graph-local-shard-id-and-index-clusters.md) | Graph-local dense `ShardId`; per-graph index cluster; commit `GROUP_SIZE` routing | accepted |
| [0020](0020-deferred-maintenance-timer-drain.md) | Timer-driven (`ic-cdk-timers`) adaptive drain of the deferred LARA maintenance queue; event-driven re-arm | accepted |
| [0021](0021-resumable-supernode-detach-delete.md) | Resumable super-node `DETACH DELETE` via tombstone-first + phased incident-edge purge on the maintenance timer; gated read-time neighbor visibility | proposed |
| [0022](0022-degree-driven-hub-edge-storage.md) | Fix labeled overflow-log read-window underflow (`CollectAllocationOverflow`) for buckets past leaf-0's cap now; degree-driven hub edge storage (dedicated span / B-tree) deferred | accepted |
| [0023](0023-federated-index-consistency-upgrade-compaction.md) | Federated index/store consistency across upgrade & compaction: remove shard index registry; router-sourced ephemeral catalog; precise emit; failure-only durable repair journal | accepted |
| [0024](0024-mutation-journal-completion-vs-index-flush.md) | Mutation journal completion vs deferred index flush | implemented |
| [0025](0025-client-mutation-journal-retention-sweep.md) | Client-mutation idempotency journal retention, compaction, and GC | implemented |
| [0026](0026-reverse-adjacency-differential-repair.md) | Reverse-adjacency differential repair (`rebuild_reverse_adjacency`): per-diverged-key reconcile; reject full clear-and-rebuild cascade | implemented |
| [0027](0027-graph-mutation-journal-retention.md) | Graph mutation journal time-based retention + amortized write-path GC; reject ack-through-seq eviction | implemented |
| [0028](0028-per-graph-tenancy-metadata-reads.md) | Per-graph tenancy on router metadata reads: owner/admins ACL + Admin superuser + own-shard allow; NotFound non-disclosure; reject anonymous owner/admin at registration | accepted |
| [0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) | Shard-local atomicity and asynchronous cross-canister consistency | accepted |
| [0030](0030-cross-shard-uniqueness-tcc-reservation.md) | Cross-shard uniqueness via Router-coordinated TCC reservation | accepted (partially implemented) |
| [0031](0031-vertex-embedding-store-and-derived-vector-index.md) | Vertex embedding store and derived vector index canister | accepted (planned) |
| [0032](0032-vector-index-slab-page-store.md) | Vector index slab page store | implemented |
| [0033](0033-vector-rebuild-state-read-memoization.md) | Vector rebuild candidate pool storage and rebuild-state read cost: reject storage-layout change; adopt transient heap memoization | accepted (implementation deferred) |
| [0034](0034-gleaph-gql-extension-syntax.md) | Gleaph GQL extension syntax surface: SEARCH, INLINE, IC values, and operational procedures | accepted (syntax design; implementation staged by feature) |
| [0035](0035-provision-canister-and-issuance-protocol.md) | Provision canister and issuance protocol | Partially Implemented (Slice 3 ingress foundation + Slice 4 callable canister endpoints + Slice 5 Router outbound accept_envelope send + Slice 6 Router ack catalog commit + owner-identity locks + invocation-owned rollback + Slice 7 durable bootstrap authority region and post-init admin install) |
| [0036](0036-versioned-wasm-artifact-catalog.md) | Versioned WASM artifact catalog | Partially Implemented (Slice 8a: artifact catalog regions + publish/upload/status; Slice 8b: release manifest + active-release pointer + activation; Slice 8c: install transfer + artifact audit log + PocketIC; fully implemented modulo external archive / HTTP outcall) |
| [0037](0037-provisioned-canister-lifecycle-and-controllers.md) | Provisioned canister lifecycle and controllers | proposed |
| [0038](0038-provisioning-authorization-and-cycles-funding.md) | Provisioning authorization and cycles funding | proposed |
| [0039](0039-production-stable-memory-evolution-and-upgrade-safety.md) | Production stable-memory evolution and canister upgrade safety | proposed |
| [0040](0040-gql-ast-formatter-and-social-demo-wasm-integration.md) | GQL AST formatter and social-demo WASM integration | Implemented |
| [0093](0093-router-mutation-preflight-call-coalescing.md) | Router mutation preflight call coalescing | Implemented |
| [0094](0094-synchronous-graph-index-flush.md) | Synchronous graph-to-index posting flush for wire DML | implemented |

When adding an ADR, link it from the relevant design doc and update this table.

## Design documents

| Document | Purpose |
|----------|---------|
| [social-demo-config](../storage/social-demo-config.md) | Per-file YAML configuration for the social-demo sample graph and its emitted artifacts. Updated by Plan 0064 to document RETURN columns and runtime semantic-vector loading; updated by Plan 0065 to document native BOOL `is_public`. |

When adding a design document, link it from the relevant ADR and update this table.
