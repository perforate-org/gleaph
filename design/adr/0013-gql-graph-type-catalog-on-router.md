# 0013. GQL graph type catalog on router (`gleaph-graph-catalog`)

Date: 2026-06-13  
Status: accepted  
Last revised: 2026-06-13  
Anchor timestamp: 2026-06-13 14:17:35 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-13 | Proposed: mount `gleaph-graph-catalog` on router stable memory; DDL ingress; planner schema bridge. |
| 2026-06-13 | **`binding_map` keys use `GraphId`** (align with ADR 0011); crate refactor + federation name prerequisite for `CREATE GRAPH`. |
| 2026-06-13 | **`type_map` string keys are interim** — [`GraphTypeId` name catalog planned in ADR 0014](0014-graph-type-id-catalog-on-router.md). |
| 2026-06-13 | S0–S3 implemented; PocketIC e2e in `router_graph_type_catalog`. |
| 2026-06-13 | **Accepted** (S0–S3). |
| 2026-06-13 | Design sync: [`type_map` → `GraphTypeId`](0014-graph-type-id-catalog-on-router.md) landed; glossary, RBAC, layers docs updated. |

## Context

ISO/IEC 39075 defines a **catalog** of property graphs and **graph types** (node/edge declarations,
labels, directedness). Gleaph parses these constructs in `gleaph-gql` and implements persistence
logic in the standalone crate **`gleaph-graph-catalog`** (`GraphCatalog`):

| Map | DDL | Key (implemented) | Payload |
|-----|-----|-------------------|---------|
| `type_map` | `CREATE GRAPH TYPE` / `DROP GRAPH TYPE` | **`GraphTypeId`** ([ADR 0014](0014-graph-type-id-catalog-on-router.md)) | [`StorableGraphTypeDefinition::V1`] |
| `binding_map` | `CREATE GRAPH` / `DROP GRAPH` | **`GraphId`** | [`GraphSchemaBinding::V2`] (`TypeRef` stores `GraphTypeId` raw; legacy V1 string `TypeRef` → migration error) |

[`GraphCatalog::try_property_schema_for_graph_id`] resolves a federation **`GraphId`** to
[`GraphTypePropertySchema`] for planning and validation. GQL and Candid still use property graph
**names** at the API boundary; router interns once via `ROUTER_GRAPH_CATALOG` ([ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md)).
Graph **type** names from catalog DDL intern via `ROUTER_GRAPH_TYPE_CATALOG` ([ADR 0014](0014-graph-type-id-catalog-on-router.md)).

**Implemented (2026-06-13):** `GraphCatalog` is mounted on router regions **21–22**; catalog DDL
runs on `gql_execute*`; schema is injected at plan + validate when a binding exists for the
focused **`GraphId`** ([layers.md](../gql/layers.md)).

### Router already has two different “graph catalogs”

Do not conflate them with this ADR:

| Name in code/docs | Stable regions | Role |
|-------------------|----------------|------|
| **`ROUTER_GRAPH_CATALOG`** | 14–15 (`ROUTER_GRAPH_BY_NAME` / `_BY_ID`) | Federation **name ↔ `GraphId`** ([ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md)) |
| **`ROUTER_GRAPHS`** | 1 | Federation **registry** (`GraphRegistryEntry`: canister, owner, `is_home`, …) |
| **`GraphCatalog` (this ADR)** | **21–22** (implemented) | GQL **property graph schema** (types + bindings); type keys **`GraphTypeId`** after [ADR 0014](0014-graph-type-id-catalog-on-router.md) |
| **`ROUTER_GRAPH_TYPE_CATALOG`** | **23–24** ([ADR 0014](0014-graph-type-id-catalog-on-router.md)) | Graph **type** name ↔ **`GraphTypeId`** |

Federation registration (`admin_register_graph`) and GQL catalog DDL (`CREATE GRAPH`) remain
**separate operations**, but **`binding_map` rows are keyed by the same `GraphId`** as
`ROUTER_GRAPHS`, index catalog, and prepared plans. Schema DDL requires the property graph name to
already exist in `ROUTER_GRAPH_CATALOG` (see §2).

### Prerequisites (met)

- [ADR 0006](0006-pre-federation-foundation.md) — router owns resolution catalogs
- [ADR 0007](0007-stable-memory-layout.md) — repack gate for new router `MemoryId`s
- [ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md) — program-based graph resolution;
  property graph **names** remain the GQL/session surface; `GraphId` is internal federation identity
- `gleaph-graph-catalog` — DDL apply, schema resolve, **V1** type-definition + **V2** binding rkyv records; **`binding_map` keyed by `GraphId`**; **`type_map` keyed by `GraphTypeId`** ([ADR 0014](0014-graph-type-id-catalog-on-router.md))

### Non-goals (this ADR)

- `CREATE GRAPH … LIKE`, `COPY OF`, `AS COPY OF` (crate returns `Unsupported` — stays deferred)
- `CREATE SCHEMA` persistence (ignored by crate today)
- Separate **catalog canister** or graph-shard copy of graph types
- Auto-provisioning router label/property **id catalogs** from graph type DDL (labels in schema
  must still exist in `ROUTER_*_LABEL_CATALOG` / property catalog for DML — follow-up)
- Graph-shard enforcement of graph type constraints on DML (v1: router validate + plan only)
- Production migration from pre-0013 snapshots (dev discard per [refactoring-roadmap.md](../architecture/refactoring-roadmap.md))
- Cross-call catalog state without DDL in the invoking program (same stateless boundary as ADR 0011)

---

## Problem

| Issue | Impact |
|-------|--------|
| **Schema not persisted** | `CREATE GRAPH TYPE` / `CREATE GRAPH` in `gql_execute*` have no effect across calls |
| **Planning blind** | `NoSchema` — no graph-type-aware binding inference or constraint checks at plan time |
| **Split ownership risk** | Without a router SSOT, future per-shard schema would diverge (same class of problem as pre-[0008](0008-edge-payload-profile-router-ssot.md) edge payload profiles) |
| **String keys in binding map** | Inconsistent with ADR 0011 `GraphId` partition for indexes, prepared plans, idempotency |
| **Dead crate** | `gleaph-graph-catalog` only unit tests + canbench; product path undefined |

---

## Decision

### 1. Router owns `GraphCatalog` (SSOT for GQL graph types)

Mount [`GraphCatalog`] on the **router canister** in two new stable regions:

| MemoryId | Symbol | Map | Class |
|--------|--------|-----|-------|
| 21 | `ROUTER_GRAPH_TYPE_DEFINITIONS` | `type_map` | catalog |
| 22 | `ROUTER_GRAPH_SCHEMA_BINDINGS` | `binding_map` (`GraphId` → binding) | catalog |

Thread-local: one `GraphCatalog<Memory, Memory>` initialized from regions 21–22 (same pattern as
index catalog maps).

**Wire format:** `StorableGraphTypeDefinition::V1` for definitions;
[`GraphSchemaBinding::V2`] for bindings (`TypeRef(u32)` = `GraphTypeId` raw). Legacy
[`GraphSchemaBinding::V1`] string `TypeRef` rows fail on read (dev discard / migration hook).

**Stable repack:** [ADR 0007](0007-stable-memory-layout.md) gates added router regions **21–22** (this ADR)
and **23–24** ([ADR 0014](0014-graph-type-id-catalog-on-router.md)); router **33** regions total (0–32).
Dev snapshot discard acceptable pre-production. `ROUTER_STABLE_LAYOUT`, `stable-memory-inventory.md`,
and layout tests updated in implementation patches.

### 2. `binding_map` keys are `GraphId`; `type_map` keys are `GraphTypeId`

Align property graph schema rows with [ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md)
catalog partitioning. Graph **type** name interning is [ADR 0014](0014-graph-type-id-catalog-on-router.md).

| Map | Stable key | Storable | Rationale |
|-----|------------|----------|-----------|
| `type_map` | **`GraphTypeId`** | [`GraphTypeId`] fixed-width | Named graph types are a **global catalog namespace**; intern at DDL ingress |
| `binding_map` | **`GraphId`** | [`GraphId`] fixed-width (`graph-kernel` `Storable`) | One schema binding per **registered logical graph**; same id as `ROUTER_GRAPHS`, `ROUTER_NAMED_INDEXES`, `ROUTER_PREPARED_PLANS` |

**Crate API:** `try_property_schema_for_graph_id(graph_id: GraphId)`. DDL ingress resolves graph
type **names** through `GraphTypeLookup` / `ROUTER_GRAPH_TYPE_CATALOG` and property graph **names**
through `GraphNameLookup` / `ROUTER_GRAPH_CATALOG`.

#### 2.1 `CREATE GRAPH` / `DROP GRAPH` prerequisite

Catalog DDL resolves the property graph **name** from the statement through
`ROUTER_GRAPH_CATALOG` → `GraphId`:

```text
CREATE GRAPH g …  →  lookup_graph_id("g")?  →  binding_map.insert(graph_id, binding)
DROP GRAPH g      →  lookup_graph_id("g")?  →  binding_map.remove(&graph_id)
```

| Outcome | Behavior |
|---------|----------|
| Name **not** in `ROUTER_GRAPH_CATALOG` | Fail with **`CatalogError::GraphNotRegistered(name)`** (new variant) — schema cannot exist without a federation graph id |
| Name registered, `CREATE GRAPH g ANY` | Remove binding row for that `GraphId` (open graph) |
| Name registered, inline / `TYPED` | Insert/replace [`GraphSchemaBinding::V2`] at `GraphId` |

**Order of operations (v1):**

1. `admin_register_graph` — allocates `GraphId`, federation row, name catalog entry ([ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md))
2. `CREATE GRAPH TYPE` — optional shared type definition in `type_map`
3. `CREATE GRAPH g …` — writes schema binding at **`GraphId` for `g`**

This ADR does **not** auto-register federation canisters from catalog DDL alone.

#### 2.2 Lookup at plan / validate time

Use **resolved `GraphId`** from ingress graph context ([ADR 0011](0011-gql-graph-resolution-and-catalog-scoping.md)),
not the property graph name string:

```text
resolve_graph_context / USE dispatch GraphId
  → try_property_schema_for_graph_id(graph_id)
  → Option<GraphTypePropertySchema>
```

Open graphs: no binding row for that `GraphId` → `None` → planner keeps [`NoSchema`] behavior.

**Remote `USE GRAPH`:** load schema by **focused segment `GraphId`** (same as per-segment stats).

#### 2.3 `DROP GRAPH TYPE` cascade

When a named graph type is removed from `type_map`, scan **`binding_map` by `GraphId`** and delete
rows whose `GraphSchemaBindingV2::TypeRef` matches the dropped **`GraphTypeId`** (integer compare;
see [ADR 0014](0014-graph-type-id-catalog-on-router.md)).

### 3. Catalog DDL ingress on router (mirror index DDL pattern)

Apply catalog statements from GQL programs on the router **`gql_execute*`** path, analogous to
[ADR 0009](0009-edge-property-index-and-index-ddl.md) index DDL:

| Statement | Action |
|-----------|--------|
| `CREATE GRAPH TYPE` | `GraphCatalog::apply_create_graph_type` |
| `CREATE GRAPH` | Resolve name → `GraphId`; `apply_create_graph` at **`GraphId`** |
| `DROP GRAPH TYPE` | `apply_drop_graph_type` (cascades `TYPED` bindings by scanning **`GraphId`** rows) |
| `DROP GRAPH` | Resolve name → `GraphId`; remove binding row |
| Other statements in same block | Unchanged — non-catalog ops follow existing classify/dispatch |

**Mixed blocks:** catalog statements run against stable catalog; DML/query statements in the same
transaction follow existing router flow (catalog apply order: extract catalog statements or delegate
to `apply_statement_block` for catalog-only prefix — exact split is implementation detail; must be
documented in [layers.md](../gql/layers.md) when implemented).

**Authorization:** GQL graph type catalog DDL (`CREATE`/`DROP GRAPH TYPE`, `CREATE`/`DROP GRAPH`)
requires **Write** or higher via `authorize_adhoc_gql` (`has_catalog_modification` from
[`classify_program`](../gql/layers.md#program-modification-security-input)). Stricter than index
DDL (Admin or Manager + `PREPARE_REGISTER`). See [rbac-and-prepared.md](../security/rbac-and-prepared.md).

### 4. Planner and validator schema bridge

Replace router `NoSchema` with a router-resolved schema provider:

```text
effective GraphId (ingress) → try_property_schema_for_graph_id
                            → Option<GraphTypePropertySchema>
                            → PropertySchema for build_block_plan_with_schema / validate_with_seed
```

| Case | Behavior |
|------|----------|
| Binding present, valid definition | Pass `GraphTypePropertySchema` (or adapter) into planner + validator seed path |
| No binding (`ANY`, unset graph) | `NoSchema` (current behavior) |
| Invalid stored definition | Fail ingress with `CatalogError::InvalidDefinition` → router `InvalidArgument` |
| `TYPED` reference to missing type | `GraphTypeNotFound` |
| Property graph name not in federation catalog | `GraphNotRegistered` |

**Scope:** router ingress only in v1. Graph shards continue to execute plan blobs; they do not read
`GraphCatalog`.

**Remote `USE GRAPH`:** resolve schema by **focused `GraphId`** per dispatch segment (align with
stats load per `GraphId` in ADR 0011 U2).

### 5. Crate boundary unchanged

- **`gleaph-graph-catalog`** remains a pure library: DDL, maps, rkyv codecs, unit tests; **`GraphId`
  binding keys** and `try_property_schema_for_graph_id` live here (no IC deps — `GraphId` from
  `gleaph-graph-kernel`).
- **Router** owns thread-local stable memory, RBAC, and GQL ingress wiring (`facade/stable/`).
- **Do not** move federation registry into the catalog crate.
- **Do not** add Gleaph/IC-specific rules into `gleaph-gql` / `gleaph-gql-planner`.

---

## Consequences

### Positive

- Single SSOT for GQL graph types on the same canister that already owns index DDL and graph name
  resolution
- Reuses tested `GraphCatalog` + V1 stable records; minimal new persistence design
- Enables graph-type-aware planning and validation without exposing catalog DDL on graph shards
- **`GraphId`-keyed bindings** match index catalog, prepared plans, and federation registry
- Clear naming distinction: federation catalog vs graph **type** catalog

### Trade-offs

- Router stable layout repack (+2 regions for 0013; +2 more for [ADR 0014](0014-graph-type-id-catalog-on-router.md))
- **`CREATE GRAPH` requires prior federation registration** (name → `GraphId` in `ROUTER_GRAPH_CATALOG`)
- Label/property **names** in graph types are not automatically inserted into router id catalogs

---

## Alternatives considered

| Alternative | Verdict |
|-------------|---------|
| **Store `GraphCatalog` on graph shard** | Rejected — same split-SSOT problem as pre-0008; schema is graph-wide logical metadata |
| **Separate catalog canister** | Rejected — extra hop; router already runs DDL for indexes and owns graph context |
| **String keys in `binding_map`** | Rejected — inconsistent with ADR 0011; rename-safe federation identity is `GraphId` |
| **Auto-intern `GraphId` on `CREATE GRAPH` without federation row** | Rejected — orphan ids without shard registry; use explicit `admin_register_graph` first |
| **Merge into `ROUTER_GRAPH_CATALOG`** | Rejected — different value type (BidirectionalCatalog vs rkyv bindings) |
| **Embed schema in `GraphRegistryEntry`** | Rejected — bloats federation rows; mixes admin registration with GQL catalog DDL |

---

## Implementation phases

| Phase | Scope | Status |
|-------|--------|--------|
| **S0a** | Refactor `gleaph-graph-catalog`: `binding_map` **`GraphId` keys**; `try_property_schema_for_graph_id`; `GraphNotRegistered`; update unit tests | **Implemented** |
| **S0b** | Router MemoryId 21–22; thread-local `GraphCatalog`; layout registry + inventory | **Implemented** |
| **S1** | Catalog DDL on `gql_execute*`; name→`GraphId` via `ROUTER_GRAPH_CATALOG`; map `CatalogError` → `RouterError` | **Implemented** |
| **S2** | Inject resolved schema at **plan + validate** by **`GraphId`** (replace `NoSchema` when binding exists) | **Implemented** |
| **S3** | PocketIC e2e: register graph → `CREATE GRAPH TYPE` + `TYPED` binding → query | **Implemented** (`router_graph_type_catalog`) |

---

## Migration

1. Land **S0a** (crate `GraphId` keys) then **S0b–S1** behind router upgrade (dev reinstall / snapshot discard).
2. Document in [layers.md](../gql/layers.md): schema lookup by **`GraphId`** after graph resolution.
3. Update [stable-memory-inventory.md](../storage/stable-memory-inventory.md) when S0 lands.
4. No Candid breaking change for query clients; catalog mutation remains GQL text in `gql_execute*`.

---

## Design documentation impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0013 | **Implemented** |
| [storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md) | Regions 21–22; **`GraphId` binding keys**; 23–24 via [ADR 0014](0014-graph-type-id-catalog-on-router.md) | **Implemented** |
| [gql/layers.md](../gql/layers.md) | Schema resolution step; catalog DDL | **Implemented** |
| [security/rbac-and-prepared.md](../security/rbac-and-prepared.md) | Catalog DDL authorization | **Implemented** |
| [glossary.md](../glossary.md) | Distinguish federation catalog vs graph type catalog | **Implemented** |

---

[`GraphTypeDefinition`]: ../../crates/gql/src/ast/graph_type.rs
[`GraphSchemaBinding::V1`]: ../../crates/graph-catalog/src/lib.rs
[`GraphSchemaBinding::V2`]: ../../crates/graph-catalog/src/lib.rs
[`GraphTypeId`]: ../../crates/graph-kernel/src/entry/graph_type_id.rs
[`GraphCatalog::try_property_schema_for_graph_id`]: ../../crates/graph-catalog/src/lib.rs
[`GraphTypePropertySchema`]: ../../crates/gql/src/type_check/graph_type_schema.rs
[`NoSchema`]: ../../crates/gql/src/type_check/schema.rs
[`GraphCatalog`]: ../../crates/graph-catalog/src/lib.rs
[`object_name_key`]: ../../crates/graph-catalog/src/lib.rs
[`GraphId`]: ../../crates/graph-kernel/src/entry/graph.rs
[`StorableGraphTypeDefinition::V1`]: ../../crates/graph-catalog/src/lib.rs
