# 0014. Graph type name catalog on router (`GraphTypeId`)

Date: 2026-06-13  
Status: accepted  
Last revised: 2026-06-13  
Anchor timestamp: 2026-06-13 14:04:54 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-13 | Proposed: `BidirectionalCatalog` for GQL graph **type** names → `GraphTypeId`; migrate `type_map` keys and `TypeRef` bindings off string keys ([ADR 0013](0013-gql-graph-type-catalog-on-router.md) follow-up). |
| 2026-06-17 | Removed pre-production legacy string `TypeRef` and dual binding envelopes; kept `::V1` version envelopes for post-production evolution. |

## Context

[ADR 0013](0013-gql-graph-type-catalog-on-router.md) mounts [`GraphCatalog`] on the router with:

| Map | Stable region | Key (0013) | Payload |
|-----|---------------|------------|---------|
| `type_map` | 30 `ROUTER_GRAPH_TYPE_DEFINITIONS` | [`object_name_key`] (`String`) | [`StorableGraphTypeDefinition`] |
| `binding_map` | 31 `ROUTER_GRAPH_SCHEMA_BINDINGS` | **`GraphId`** | [`GraphSchemaBinding`] |

Property graph bindings already use federation **`GraphId`** ([ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md)).
Named graph **types** previously used unbounded string keys in `type_map` and string `TypeRef` bindings
(0013 interim). **0014** retargets both to [`GraphTypeId`].

That asymmetry was intentional in 0013 (ship schema SSOT first). It now blocks:

| Gap | Why it matters |
|-----|----------------|
| **Stable key width** | String BTree keys in `type_map` are unbounded; every `TYPED` binding stores a duplicate type name string |
| **Cascade cost** | `DROP GRAPH TYPE` scans all `GraphId` bindings comparing `TypeRef` **strings** |
| **Rename / admin** | No stable identity for a graph type separate from its GQL surface name |
| **Pattern drift** | Graph names, index names, labels, and properties already intern to ids on the router; graph type names are the outlier |

### Do not conflate with in-schema “type keys”

GQL **graph type definitions** contain node/edge declarations (`Person`, `KNOWS`, …). Those are
**schema content** inside rkyv [`GraphTypeDefinition`], not catalog partition keys. This ADR covers
only the **named graph type object** from `CREATE GRAPH TYPE gt { … }` — the `gt` in
`CREATE GRAPH g TYPED gt`.

Label/property **id catalogs** (`VertexLabelId`, `PropertyId`) are **graph-scoped** per `GraphId`
([ADR 0018](0018-graph-scoped-label-property-catalogs.md)) and remain separate from graph-type
identity; 0013’s non-goal of auto-inserting schema labels into those catalogs was superseded by
0018 **V5** (optional auto-intern on `CREATE GRAPH` / `CREATE GRAPH TYPED`).

### Prerequisites

- [ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md) — `BidirectionalCatalog` pattern on router
- [ADR 0013](0013-gql-graph-type-catalog-on-router.md) — `GraphCatalog` on regions **21–22**;
  `binding_map` keyed by **`GraphId`**; catalog DDL ingress (S0–S2 landed)
- `gleaph-graph-kernel` — `CatalogId`, `BidirectionalCatalog`, `GraphId`, `IndexNameId`

---

## Problem

| Issue | Impact |
|-------|--------|
| **String keys in `type_map`** | Unbounded stable keys; harder layout invariants; inconsistent with ADR 0011 id partitioning |
| **String `TypeRef` in bindings** | Redundant storage; string compare on every cascade scan |
| **No graph-type identity** | Cannot rename a type or reference it in admin APIs without rewriting bindings |
| **Two-part lookup** | Resolve `TYPED gt` → string key → `type_map` get; id catalog collapses to id → definition |

---

## Decision

### 1. Introduce `GraphTypeId` (router-global catalog identity)

Add **`GraphTypeId(u32)`** in `gleaph-graph-kernel` (same storage width as [`GraphId`], **separate
type and id space**):

| Rule | Value |
|------|-------|
| Reserved | **`0`** never assigned |
| Allocation | [`SparseFromOnePolicy`] (lowest free id), same as [`GraphId`] catalog |
| Scope | **Router-global** — not scoped per property graph (unlike [`IndexNameId`]) |
| API surface | GQL / Candid still use **graph type names**; router interns at catalog DDL ingress |

Implement `CatalogId` for `GraphTypeId` and wire into [`BidirectionalCatalog`].

### 2. New stable regions: graph type **name** catalog

Mount a second bidirectional catalog on the router (**+2 `MemoryId`s**):

| MemoryId | Symbol | Map | Class |
|--------|--------|-----|-------|
| 23 | `ROUTER_GRAPH_TYPE_BY_NAME` | [`object_name_key`] → **`GraphTypeId`** | catalog |
| 24 | `ROUTER_GRAPH_TYPE_BY_ID` | **`GraphTypeId`** → name string | catalog |

Thread-local: **`ROUTER_GRAPH_TYPE_CATALOG`** (`BidirectionalCatalog<GraphTypeId, …>`), initialized
from regions 23–24 (mirror [`ROUTER_GRAPH_CATALOG`] at 14–15).

**Stable repack:** router layout registry is **33** regions (0–32) per [ADR 0007](0007-stable-memory-layout.md). Dev snapshot discard acceptable pre-production.

Existing regions **21–22** stay the [`GraphCatalog`] payload maps; only **key types and wire
variants** change (§3).

### 3. Retarget `GraphCatalog` maps to `GraphTypeId`

| Map | Region | Key (0014) | Payload |
|-----|--------|------------|---------|
| `type_map` | 21 | **`GraphTypeId`** | [`StorableGraphTypeDefinition::V1`] |
| `binding_map` | 22 | **`GraphId`** | [`GraphSchemaBinding::V1`] |

#### 3.1 `GraphSchemaBinding` wire

Versioned rkyv envelope **`GraphSchemaBinding::V1`**:

| Variant | Payload |
|---------|---------|
| Inline | `Inline(GraphTypeDefinition)` |
| Typed reference | **`TypeRef(u32)`** — `GraphTypeId::raw()` |

**Write path:** always emit **`GraphSchemaBinding::V1`** with the variants above.

#### 3.2 DDL apply (ingress)

Extend [ADR 0013](0013-gql-graph-type-catalog-on-router.md) §3 catalog DDL:

```text
CREATE GRAPH TYPE gt { … }
  → type_id = ROUTER_GRAPH_TYPE_CATALOG.get_or_insert(object_name_key(gt))
  → type_map.insert(type_id, definition)

CREATE GRAPH g TYPED gt
  → type_id = lookup GraphTypeId by name (fail GraphTypeNotFound if missing)
  → binding_map.insert(graph_id, TypeRef(type_id))

DROP GRAPH TYPE gt
  → type_id = lookup by name
  → type_map.remove(type_id)
  → scan binding_map: remove rows where TypeRef(type_id)
  → ROUTER_GRAPH_TYPE_CATALOG.remove(type_id)  // frees name for reuse
```

| Statement | Name catalog | `type_map` | `binding_map` |
|-----------|--------------|------------|---------------|
| `CREATE GRAPH TYPE` (new) | `get_or_insert` | insert at id | — |
| `CREATE GRAPH TYPE OR REPLACE` | existing id | replace definition | `TypeRef` rows unchanged |
| `DROP GRAPH TYPE` | remove id + name | remove | cascade by **`GraphTypeId`** |
| `CREATE GRAPH … TYPED` | lookup id | — | `TypeRef(id)` |

**`IF NOT EXISTS`:** if name already mapped, no-op without touching definition unless `OR REPLACE`.

#### 3.3 Schema resolution

[`GraphCatalog::try_property_schema_for_graph_id`] unchanged at the API; internally:

```text
binding_map[graph_id]
  → Inline(def)  → decode definition
  → TypeRef(id)  → type_map[id]   // no string hop
```

### 4. Crate and router boundaries

| Layer | Owns |
|-------|------|
| **`gleaph-graph-kernel`** | `GraphTypeId`, `CatalogId` impl, stable layout symbols 23–24 |
| **`gleaph-graph-catalog`** | `type_map` keyed by `GraphTypeId`; V2 binding codec; DDL apply takes **`GraphTypeLookup`** trait (mirror [`GraphNameLookup`]) |
| **Router `facade/stable/`** | `ROUTER_GRAPH_TYPE_CATALOG` thread-local; **`RouterGraphTypeLookup`** for catalog DDL; intern at ingress |

**Do not** merge `ROUTER_GRAPH_TYPE_CATALOG` into `ROUTER_GRAPH_CATALOG` — different namespaces
(property graph vs graph type).

**Do not** add IC deps to `gleaph-graph-catalog`; lookup traits stay injectable.

### 5. Future enabled (non-goals for v1 implementation)

- GQL `RENAME GRAPH TYPE` (not in ISO surface today) — would update name catalog only
- Admin `list_graph_types` / Candid introspection by `GraphTypeId`
- Embedding `GraphTypeId` in prepared-plan metadata (optional follow-up)

---

## Consequences

### Positive

- **Consistent id partitioning** with ADR 0011 graph/index catalogs
- Fixed-width `type_map` keys; smaller `TypeRef` payloads
- **`DROP GRAPH TYPE` cascade** compares `GraphTypeId` instead of strings
- Stable identity for graph types without coupling to property graph `GraphId`
- Enables rename and admin tooling later without rebinding every `GraphId`

### Trade-offs

- **Second router repack** (+2 regions) after 0013
- **No pre-production legacy wire** — string `TypeRef` and dual `V1`/`V2` binding envelopes removed; version envelopes retained for future `V2+`
- Slightly more ingress work (`get_or_insert` on every new graph type name)
- Debug dumps show numeric ids — name catalog required for human-readable admin

---

## Alternatives considered

| Alternative | Verdict |
|-------------|---------|
| **Keep string `type_map` keys (0013 status quo)** | Rejected for long term — asymmetry with `GraphId` bindings; deferred only to land 0013 first |
| **Intern at ingress only; keep string `TypeRef` in bindings** | Rejected — half migration; cascade and storage still string-heavy |
| **Reuse `GraphId` for graph types** | Rejected — conflates federation property graph identity with GQL catalog type namespace |
| **Graph-scoped graph type ids (per `GraphId`)** | Rejected — GQL graph types are catalog-global; `TYPED` references type names, not graph ids |
| **Merge into `ROUTER_GRAPH_CATALOG`** | Rejected — same reason as 0013 § alternatives |
| **Store definitions inside bidirectional catalog value** | Rejected — mix lookup catalog with large rkyv blobs; keep 0013 two-map split |

---

## Implementation phases

| Phase | Scope | Status |
|-------|--------|--------|
| **T0** | `GraphTypeId` in `graph-kernel`; `CatalogId`; layout registry regions 23–24 | **Implemented** |
| **T1** | Router `ROUTER_GRAPH_TYPE_CATALOG` + init; `GraphTypeLookup` in catalog crate | **Implemented** |
| **T2** | `type_map` keys → `GraphTypeId`; `GraphSchemaBinding::V1` with `TypeRef(u32)`; update unit tests + canbench | **Implemented** |
| **T3** | Version envelopes only; update `ROUTER_STABLE_LAYOUT` count **33** (0–32) | **Implemented** (dev discard; no string `TypeRef` legacy) |
| **T4** | PocketIC regression: `CREATE GRAPH TYPE` + `TYPED` + drop cascade | **Implemented** (`router_graph_type_catalog.rs`) |

**Sequencing:** complete [ADR 0013](0013-gql-graph-type-catalog-on-router.md) **S3** e2e before T0
(validate string-key path end-to-end, then migrate).

---

## Migration

1. **Dev / pre-production:** reinstall router canister; discard snapshots (ADR 0007 policy).
2. **If retaining snapshots:** one-shot upgrade hook:
   - Walk legacy `type_map` string keys → `get_or_insert` each name → re-insert at `GraphTypeId`
   - Rewrite `binding_map` `TypeRef(String)` → `TypeRef(GraphTypeId)` via name catalog
   - Clear legacy string-keyed btree or repack region 21
3. Update [stable-memory-inventory.md](../storage/stable-memory-inventory.md) and layout tests when T1 lands.
4. No Candid breaking change for GQL clients; graph type names remain in DDL text.

---

## Relationship to ADR 0013

| Topic | ADR 0013 | ADR 0014 |
|-------|----------|----------|
| Mount `GraphCatalog` on router | ✓ regions 21–22 | unchanged regions; **key / wire change** |
| `binding_map` keyed by `GraphId` | ✓ | unchanged |
| `type_map` keyed by name string | ✓ (interim) | **superseded** → `GraphTypeId` |
| `TypeRef` stores type name string | ✓ (V1) | **superseded** → `GraphTypeId` (V2) |
| Catalog DDL ingress | ✓ | extended with name intern |
| Planner schema bridge | ✓ | unchanged API |

When 0014 is **accepted**, amend 0013 §2 trade-off (“future GraphTypeNameId”) to point here.

---

## Design documentation impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0014 | **This patch** |
| [storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md) | Regions 23–24; router count 33 (0–32) | **Implemented** |
| [gql/layers.md](../gql/layers.md) | Note graph type name intern at catalog DDL | **Implemented** |
| [0013-gql-graph-type-catalog-on-router.md](0013-gql-graph-type-catalog-on-router.md) | Cross-link; interim string keys | **This patch** |

---

[`GraphCatalog`]: ../../crates/graph-catalog/src/lib.rs
[`GraphCatalog::try_property_schema_for_graph_id`]: ../../crates/graph-catalog/src/lib.rs
[`GraphSchemaBinding::V1`]: ../../crates/graph-catalog/src/lib.rs
[`GraphTypeDefinition`]: ../../crates/gql/src/ast/graph_type.rs
[`GraphId`]: ../../crates/graph-kernel/src/entry/graph.rs
[`GraphNameLookup`]: ../../crates/graph-catalog/src/lib.rs
[`IndexNameId`]: ../../crates/graph-kernel/src/entry/index_name.rs
[`object_name_key`]: ../../crates/graph-catalog/src/lib.rs
[`ROUTER_GRAPH_CATALOG`]: ../../crates/router/src/facade/stable.rs
[`SparseFromOnePolicy`]: ../../crates/graph-kernel/src/bidirectional_catalog.rs
[`StorableGraphTypeDefinition::V1`]: ../../crates/graph-catalog/src/lib.rs
[`BidirectionalCatalog`]: ../../crates/graph-kernel/src/bidirectional_catalog.rs
