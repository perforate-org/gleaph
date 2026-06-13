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
| [0008](0008-edge-payload-profile-router-ssot.md) | Edge payload profile schema: router SSOT; retire graph `EDGE_PAYLOAD_PROFILES` | accepted |
| [0009](0009-edge-property-index-and-index-ddl.md) | Edge property index on graph-index; mixed intersection; opt-in `CREATE INDEX` / `DROP INDEX` DDL | accepted |
| [0010](0010-index-sharding-extensibility.md) | Index sharding: defer split strategy; stable posting wire; router index resolution | accepted |
| [0011](0011-gql-graph-resolution-and-catalog-scoping.md) | GQL graph resolution; `BidirectionalCatalog` for graph/index names; migrate stable keys to `GraphId` / `IndexNameId` | accepted |
| [0012](0012-edge-index-direction-in-ddl.md) | Edge index direction in `CREATE INDEX FOR` (GQL patterns); wire label in graph-index keys; planner subset rule | proposed |

When adding an ADR, link it from the relevant design doc and update this table.
