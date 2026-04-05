# Intra-graph sharding (one logical graph, many canisters)

This document describes **intra-graph sharding**: how a **single logical property graph** can **grow** as a **union of shards** hosted on multiple Internet Computer canisters. That is a **storage and runtime** concern. It is **not** the same thing as **graph-unit federation** (explicit `USE GRAPH` across distinct logical graphs); see [`FEDERATION.md`](./FEDERATION.md).

## Two distinct concepts

| | **Intra-graph sharding** (this document) | **Graph-unit federation** ([`FEDERATION.md`](./FEDERATION.md)) |
|---|------------------------------------------|----------------------------------------------------------------|
| **Unit of abstraction** | One **logical** graph | Multiple **logical** graphs (names in the registry) |
| **What the user sees** | Exactly **that one graph** | Explicit switch / delegation via `USE GRAPH` |
| **GQL surface** | **No** language feature to pick a shard canister or stitch shards. The query addresses **the graph as a whole**; **there is no need** for the author to control placement. | `USE GRAPH`, routed sub-plans, `execute_routed_query_batch`, etc. |
| **Inter-canister work** | May happen **under** the executor/kernel when traversing or resolving data; opaque to the query text. | Explicit remote plan execution and ACL rules for **caller vs subject**. |

**Principle:** Sharding lets one graph’s **physical** footprint span canisters while its **conceptual** identity stays **one graph**. Graph-unit federation is orthogonal: it composes **different** graphs that remain distinct at the language level.

## Vocabulary (use consistently)

- **Federation** — **graph-unit** only: multiple **logical** graphs, `USE GRAPH`, routed sub-plans, registry by name. Never call this **sharding** (in the intra-graph sense).
- **Sharding** — **intra-graph** only: one **logical** graph, many canisters (**shards**), no author-visible graph switch. Never call this **federation**.

## When to add a shard canister (sharding topology)

Sharding topology asks: **under what circumstances should we introduce another canister into the *same* logical graph?** This is a **topology and operations** decision, not something query authors trigger per pattern.

**Recommended criteria (any one can be sufficient; often several apply):**

1. **Hard limits** — A shard approaches **stable memory**, per-message **cycle** ceilings, or unacceptable **hydration / maintenance** time. A new shard canister offloads **physical** capacity while the graph stays logically one.
2. **Partition boundary** — A subgraph is a natural **cut** (e.g. only reached via cross-shard edges you control), so cross-shard chatter and failure domains stay predictable.
3. **Isolation** — **Blast radius**, upgrade windows, or **governance** require a separate canister even though data is still **one** logical graph.
4. **Frontier growth** — New regions of the graph are **anchored** on a new shard (e.g. bulk import, new service area) without rebalancing the entire existing canister.

**What should *not* drive sharding:** Wanting to query **another logical graph** by name, or ad hoc “call that canister for this subquery.” That is **federation**, not sharding. Adding shard canisters should be **relatively rare** compared to vertex/edge growth—shards are **partitions**, not per-hop switches.

## Remote labels and properties (metadata across shards)

Question: how does code on **shard A** obtain **label and property** information for vertices and edges whose **authoritative** record lives on **shard B**?

**Recommended view:**

1. **Single source of truth on the owning shard** — Labels and properties for a record stored on `B` are **authoritative on `B`**. Shard `A` may hold **stubs**, **caches**, or **opaque routing keys**, but must not pretend to be the sole authority for `B`’s payload unless a replication policy says otherwise.
2. **How to obtain metadata (implementation paths; composable):**
   - **Lazy fetch on continuation** — When the executor resolves a cross-shard hop, it **pulls** the remote node/edge (or a **projected** subset) from `B` for the operators at hand. Minimal upfront agreement; planner may be **conservative** when remote schema is unknown until a catalog exists.
   - **Replicated logical catalog** — A **graph-wide** label / key / constraint catalog (and optional stats) **replicated** to every shard canister of the same logical graph so planning and pushdown can reason without per-query discovery. Schema changes use a defined **sync or version** story.
   - **Small introspection surface** — Optional canister API (e.g. “describe shard” or “resolve ref”) for runtime or **operator** use, **cached** aggressively—not a general end-user GQL feature.
3. **GQL stays agnostic** — Authors do not write “import remote schema.” Any catalog replication or lazy read is **below** the query language; the graph remains **one** graph semantically.

**Tradeoff:** Lazy fetch keeps shard canisters loosely coupled but can surprise the planner with **cold** metadata unless stubs or catalog supply hints. A replicated catalog adds **coordination** but enables better plans and clearer compatibility between shards.

## Goals

- Represent cross-canister adjacency inside the **same** logical graph (compact meta + `ShardCanisterDirectory`, asymmetric `EdgeMeta`, etc.—**code names** retain `peer_*` today).
- Preserve **single-graph semantics** at the GQL boundary: authors write patterns against **one** graph; routing to the correct shard is **not** a GQL control lever.
- Keep the hot read path (expand / neighborhood) able to carry **internal** handoff metadata (`expand_hops_with_shard_meta`, optional `hop_aux_bytes_for_edge`).

## Non-goals (for intra-graph sharding)

- Exposing shard choice or routing **in GQL** (no `USE GRAPH`-style syntax for “the same graph”).
- Collapsing this into graph-unit federation: sharding is **not** “call another logical graph,” it is **one** graph stored in parts.
- Relying on end users to wire `USE GRAPH` to stitch what is **logically** already one graph (that wiring, when it appears in implementations, belongs below the language, not in author-facing control flow).

## Current implementation (inventory)

| Layer | What exists |
|--------|----------------|
| **Stable storage** | `RegionKind::ShardCanisterDirectory` (`SCD1`), dense slot → `Principal`; hydrate validates live cross-shard slots (`validate_shard_canister_slots`). |
| **Adjacency** | Forward meta may be `EdgeMeta::new_shard_canister(slot, …)`; reverse meta stays a normal label id (asymmetric pair). |
| **Bridge / overlay** | `insert_edge_with_shard_canister_dst` / `bootstrap_edge_with_shard_canister_dst`: local `src` → local stub `dst`, registers principal in directory, stores cross-shard meta on forward surface. |
| **Kernel API** | `GraphRead::expand_hops_with_shard_meta` fills `ExpansionHop::shard_canister_principal` (raw principal bytes). Default impl: no remote principal on the hop. |
| **GQL (optional / auxiliary)** | Demand-driven `{edge}__hop_aux`: planner sets `hop_aux_binding`; executor binds `Value::Bytes` or `Null`. Treat as **observability or advanced** use, **not** as the primary way **sharding** is surfaced to authors—normal single-graph queries do not need it. |

### Undirected edges (logical flag + hot meta)

Logical undirected edges (GQL `~`) set [`EdgeRecord::undirected`](../graph-kernel/src/records.rs) and the **undirected bit** on **both** forward and reverse [`EdgeMeta`](../graph-pma/src/low_level/edge.rs) payloads for that `EdgeId`, including cross-shard pairs (shard slot on forward, label id on reverse). Pattern expansion filters on this flag: pure `->` / `<-` patterns skip undirected-only rows; `Undirected` and mixed directions combine directed and undirected semantics in the PMA overlay read path (`graph_read_impl`).

## Concepts

### Stub destination vertex

A cross-shard edge’s `dst` is a **local** `NodeId` (labels/properties as chosen by the deployment). It may represent a vertex whose authoritative record lives on another shard; **global** identity across canisters is defined by **runtime conventions**, not by GQL syntax.

### What `e__hop_aux` means today

- **Bytes**: raw Internet Computer principal bytes for the **remote shard** canister associated with that hop, when the store marks the forward hop as cross-shard (`EdgeMeta::is_shard_canister`).
- **Null**: purely local hop, or store without cross-shard resolution for that edge.

Typed `Principal` in the GQL surface (if desired) remains an extension concern. Ordinary single-graph queries do not depend on this column.

## Design decisions (locked)

1. **Shard hint on the hop** — principal (via directory slot) is associated with the expansion hop for efficient runtime routing, not only with ad hoc node properties.
2. **Directory is authoritative for “which canister”** — edge meta holds a **slot**, not an inline principal, to keep adjacency entries small and stable across compactions. The slot lives in the **16-bit** `EdgeMeta` payload when the cross-shard flag is set (`crates/graph-pma` `EdgeMeta`).
3. **Single-graph cross-shard continuation is not `USE GRAPH`** — for one logical graph, following a cross-shard edge to another canister is an **executor/kernel** responsibility. **`USE GRAPH` is reserved for graph-unit federation** ([`FEDERATION.md`](./FEDERATION.md)), i.e. distinct logical graphs.

## Open design choices (to resolve next)

### A. Remote vertex / shard routing key

For **transparent** continuation on the same logical graph, the runtime needs a stable **internal** reference to the remote endpoint. Conventions (stub properties, packed hop payload, etc.) are **implementation** details—they do not become a separate GQL “control plane” for sharding. Pair with [Remote labels and properties](#remote-labels-and-properties-metadata-across-shards): routing key tells *where*; catalog/lazy fetch tells *what schema* applies on that shard.

### B. Relation to graph registry

**Graph-unit** federation uses `GraphRegistryResolver` (logical name → `canister_id`) together with `USE GRAPH`. **Intra-graph** sharding uses **one** logical graph name; shard choice must be resolved **inside** that graph’s implementation (directory + meta + future routing policy), not by authors switching graphs in GQL.

### C. Reverse direction semantics

Reverse meta has no cross-shard marker today: traversing **into** the home shard from a stub behaves like a normal local edge. Symmetric metadata or policies for “back-edges” across shards are **sharding** concerns, not `USE GRAPH` concerns.

## Phased roadmap

| Phase | Scope | Status |
|-------|--------|--------|
| **P0** | Directory + asymmetric meta + expand / optional `hop_aux` surfacing | **Done** (see inventory). |
| **P1** | **Internal** remote/shard routing key convention + tests (no requirement on GQL authors) | **Next** |
| **P2** | Executor/kernel **transparent** cross-shard steps for the **same** logical graph (still one graph in the language) | Planned |
| **P3** | Planner support only if needed for **correctness or cost** of single-graph plans—not for exposing shard control in GQL | Exploratory |

Graph-unit federation roadmaps (`USE GRAPH` pushdown, routed batching, etc.) stay in [`FEDERATION.md`](./FEDERATION.md) and related code; they are **not** substitutes for intra-graph sharding.

## Security note

Any inter-canister read still enforces **that canister’s** ACL and caller rules. That is independent of whether the hop was triggered by **intra-graph** routing or by an explicit **graph-unit** `USE GRAPH` call. Surfacing `e__hop_aux` in a result does not grant rights on a remote shard.

## Graph type catalog vs graph registry

- **Graph registry** ([`crates/graph-registry`](../graph-registry)) maps a **logical graph name** to **which canister** serves that graph (`USE GRAPH`, federation routing). It does **not** store `CREATE GRAPH TYPE` definitions.
- **Graph type catalog** ([`crates/graph/src/catalog.rs`](./src/catalog.rs)) holds **DDL** results: named graph types and optional **typed** `CREATE GRAPH` bindings. The planner resolves [`PropertySchema`](../../gql/src/type_check/graph_type_schema.rs) from the active graph (`ExecutionContext::selected_graph`) for DML direction and related checks.
- **`COPY OF` / `LIKE`** on graph types or graphs are **not** implemented in the catalog yet; applying them returns a clear error.
- **Persistence:** the canister serializes the catalog into [`GleaphServiceSnapshot::graph_catalog_blob`](./src/service.rs) as **`ic_stable_structures::StableBTreeMap` wire bytes** (prefix keys `t:` / `b:`), restored on upgrade alongside ACL and prepared statements.

## Related code

- `crates/graph-pma/src/low_level/shard_canister.rs` — directory encoding (`ShardCanisterDirectory`)
- `crates/graph-pma/src/integration/bridge_bootstrap.rs` — `insert_edge_with_shard_canister_dst`
- `crates/graph-kernel/src/traits.rs` — `expand_hops_with_shard_meta`, `hop_aux_bytes_for_edge`
- `crates/gql-planner/src/planner.rs` — `hop_aux_binding_for_edge_if_referenced`
- `crates/gql-executor/src/graph_ops.rs` — `insert_hop_aux_binding`
