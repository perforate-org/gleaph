# Knowledge Graph Demo (GraphRAG search slice)

Anchor timestamp: 2026-08-24 04:20:00 UTC +0000

## Status

**Planned** — This document describes intended behavior for a new demo that replaces the social
comparison demo (`demo/social`, `design/demo/social-graph-rag.md`). The change is architectural:
the demo surface moves from a microblog feed comparison to a knowledge-graph visualization that
shows Gleaph's graph-native search and traversal strengths, with the Graph Explorer as the primary
surface.

## Why replace the social demo

The social demo frames Gleaph as a relational/vector *comparison*. Its six scenarios are read-only
feed queries, vector search, and one four-hop path. Those are all things a relational or vector DB
can also do; the demo does not foreground Gleaph's differentiators:

- **Variable-length path matching** (`-[e]->{1,5}`), which a normalized RDB answers only with
  increasingly heavy joins (see the topic-path scenario note).
- **Shortest-path search** (`MATCH ANY SHORTEST`), a graph-native read.
- **Composing structure, vector similarity, and access control in one query plan** (the README's
  "one query plan" direction), which is Gleaph's long-term positioning.

All three are implemented in Gleaph (verified in code: path quantifiers in the parser
`crates/gql/src/parser/pattern.rs`, `VarLenSpec` lowering in the planner
`crates/gql-planner/src/planner/match_plan/path/filters.rs`, `ANY SHORTEST` planning in
`crates/gql-planner/tests/planner_tests.rs`, and `SEARCH ... VECTOR INDEX` execution in the graph
kernel). The new demo surfaces them.

## Demo graph: `knowledge`

A small technical-knowledge graph that exercises variable-length paths, shortest paths, vector
search, and access constraints in realistic, readable queries.

### Graph type

```gql
CREATE GRAPH TYPE KnowledgeGraph {
  NODE Document { title STRING NOT NULL, body STRING NOT NULL, is_public BOOL NOT NULL },
  NODE Concept  { name STRING NOT NULL, definition STRING NOT NULL },
  NODE Person   { name STRING NOT NULL, role STRING NOT NULL },
  NODE Team     { name STRING NOT NULL },

  DIRECTED EDGE Cites    LABEL CITES       CONNECTING (Document -> Document),
  DIRECTED EDGE About    LABEL ABOUT       CONNECTING (Document -> Concept),
  DIRECTED EDGE Relates  LABEL RELATED_TO  CONNECTING (Concept -> Concept),
  DIRECTED EDGE Authored LABEL AUTHORED_BY CONNECTING (Person -> Document),
  DIRECTED EDGE Owns     LABEL OWNS        CONNECTING (Person -> Document),
  DIRECTED EDGE BelongsTo LABEL BELONGS_TO CONNECTING (Person -> Team),
  DIRECTED EDGE RoutedVia LABEL ROUTED_VIA CONNECTING (Person -> Document)
}
```

- `CITES`, `ABOUT`, `RELATED_TO`, `AUTHORED_BY` give the search/traversal material.
- `OWNS` and `ROUTED_VIA` model access: a caller may read a `Document` if it is public or owned
  or reachable through a bounded `ROUTED_VIA` path.
- `BELONGS_TO` groups people into teams so a scenario can constrain vector search to a team.

### Vector index

One vector index over `Document` embeddings (canonical with the graph data, as in the social
demo). `SEARCH` over it is constrained by graph patterns and access rules in the same query. It is
declared declaratively in migration `000003_knowledge_vector_index` (`CREATE VECTOR INDEX
document_embedding FOR (d:Document) ON d.embedding ...`), which provisions the vector canister
(provisioned mode) or registers a targetless definition (dev mode) per ADR 0071. In dev mode the
demo's `setup_vector_index` admin sequence remains the operational fallback for attaching a target.

### Embedding ingestion path (implemented, B案)

The demo feeds embeddings through a dedicated CLI step, `gleaph embed ingest`, decided 2026-08-23:
after `gleaph load` completes, the command re-reads `seeds/vertices.jsonl` for the `source_id`
order, pages the load job's chunk receipts to reconstruct `source_id → encoded vertex id`
(receipts are the ground truth because Router commits budget-fitting chunk prefixes), and pushes
payload-fitted batches through Router's existing admin batch API (`ingest_vertex_embeddings`,
ADR 0064 §6 two-call flow). This keeps the durable bulk-load wire free of embedding bytes — the
vector canister remains the sole owner (ADR 0064 §6), and no canister or `format_version`ed wire
type changes. The rejected alternative — carrying embeddings inside the bulk-load artifact and
dispatching them inside the durable lifecycle — would touch Router chunk-commit paths, size hints,
and recovery/GC interactions; it is deferred and would need its own ADR if load+embed atomicity
ever becomes a real requirement. Deterministic seed vectors live in
`seeds/embeddings.jsonl` with their generator (`scripts/gen-embeddings.mjs`).

## Demo scenarios (prepared queries)

1. **Variable-length reach** — from a starting Concept, which Concepts are reachable in 1..N
   `RELATED_TO` hops, with hop depth. Surfaces the path-quantifier feature a relational baseline
   cannot express. (Implemented direction reads *incoming* edges to the anchor: the demo seeds
   author `RELATED_TO` edges toward `graph-databases`, making it a pure sink, so an outgoing
   pattern correctly returns 0 rows — verified during plan 0296 bring-up.)
2. **Shortest path** — `MATCH ANY SHORTEST` from one Concept/Document to another. Surfaces
   graph-native shortest path.
3. **Graph-constrained semantic search** — `SEARCH` a Document embedding restricted by an
   `OWNS`/`BELONGS_TO` access pattern: "documents about a query that this team can read". Surfaces
   structure + vector + access in one query.
4. **Citation reach** — Documents reachable from a source Document through `CITES` up to depth N,
   with the relationship trail returned as edge identities (the `topic-path` prototype).
   Deliberate exception to the element-id projection guidance: the page renders the trail from
   the returned ids, so this op keeps its edge `ELEMENT_ID` projection even though the CLI
   warns that edge element ids are not stable identifiers and may change over time
   (`design/security/rbac-and-prepared.md`, *Element-id projection guidance*).

### Parameter surface (implemented state)

Three ops are literal-bound with no parameters. Scenario 3 declares one required parameter
`$query` (`Value::Bytes`, 768 little-endian f32s), per `prepared/team-readable-documents.gql`;
the demo page supplies it in-page via the shared deterministic embedding module
(`scripts/embedding-core.mjs`, also consumed by `scripts/gen-embeddings.mjs`), so no user param
input UI exists and the top-1 row is fixed by construction to the seeded document (`d4`,
"Vector search in graphs"). `gleaph prepared run --param` cannot express `Value::Bytes` yet, so
scenario 3 is browser-only until the CLI gains a binary value form.

## Browser access contract (ADR 0074)

The page calls `prepared_query` as a plain browser principal that owns nothing, so execution is
governed entirely by ADR 0074:

- **Default deny.** Every prepared op starts non-executable for the visitor:
  `authorize_prepared_execute` gates `prepared_query`
  (`crates/router/src/prepared.rs`) and the GRANT/REVOKE control path is compiled in unguarded
  (`crates/router/src/gql.rs`, authorization-only block → `execute_authorization_block`);
  neither is feature-gated in this build.
- **Publication.** The demo owner runs `gleaph prepared publish <op>`, which sends
  `GRANT EXECUTE ON PREPARED QUERY <op> TO PUBLIC` through the generic `gql_mutate` entrypoint;
  `unpublish` sends the symmetric `REVOKE … FROM PUBLIC`. Grammar and semantics are pinned by
  the Router's own grant tests (`crates/router/src/gql_grants.rs`,
  `publication_tests::owner_publishes_public_row_and_revoke_removes_exactly_it`: owner authority,
  exact-key revoke, repeated revoke = `NotFound`). All four knowledge op names are hyphen-free,
  so GAP-2026-08-24-005 does not bite.
- **Env wiring.** The page reads three Vite variables written by
  `demo/knowledge/scripts/write-env.mjs`. Router id SSOT precedence:
  `--canister`/`GLEAPH_CANISTER` (explicit mode; what the bootstrap banner exports) → the
  lazy-issuance cache `.gleaph/cache/account/<env>.router.json` (Account-based networks only).
  Gateway URL precedence: `--gateway-url`/`GLEAPH_GATEWAY_URL` → launcher status file →
  built-in default.

## Demo surface

The primary surface is the **Graph Explorer** embedded in the demo host (wasm target), which:

- loads the whole `knowledge` graph through the SDK `GraphLoadSpec` (content-agnostic; already
  implemented in `crates/gleaph-graph-explorer`);
- runs one selected prepared query through the Gleaph Rust SDK;
- correlates returned element ids to the rendered graph via `GraphIdentityMap`;
- highlights returned nodes/edges with the query overlay (`gpui-graph` overlay), without
  rebuilding the scene.

A small scenario selector/title layer sits beside the graph. The feed-style Solid frontend and the
relational-baseline comparison are removed.

## Scope

In scope for this slice:

- Replace `demo/social` demo content with the knowledge-graph demo.
- Add the `knowledge` graph type, migrations, seed config, and prepared queries.
- Embed the existing Graph Explorer as the demo graph surface (load graph → run query → overlay).
- Keep the Graph Explorer crate (`graph`, `session`, `mapping`, `query`, `presentation`) and its
  `GraphLoadSpec` as the loader; adjust only what graph names/labels it targets.

Out of scope (later slices):

- GraphRAG "generation" (LLM). The retrieval/traversal side is implemented; wiring an LLM
  (e.g. `ic-llm`) to consume graph contexts is a separate ADR and is explicitly deferred.
- Arbitrary GQL, query editor, result table, property highlighting.

## Graph creation

The `knowledge` graph is created via GQL `CREATE GRAPH` (migration
`000001_knowledge_graph_type/up/01_knowledge_graph.gql`). [ADR 0070](../adr/0070-create-graph-provisions-shards-and-sets-home.md)
makes `CREATE GRAPH` the self-sufficient graph-creation entry: it provisions the graph shard and
sets the created graph as the caller's home graph when the caller has none, so no out-of-band
`register_graph`/canister-placement step is needed to run a query against it.

## Boundary rules

- `crates/gpui-graph` stays general-purpose; no Gleaph types leak into it (already enforced).
- The `gleaph-graph-explorer` crate stays content-agnostic; the `knowledge` graph is described by
  data (`GraphLoadSpec`, seed, prepared statements), not by Gleaph-specific code in the crate.
- Removed superseded demo paths are deleted, not left with back-compat branches (pre-production
  simplicity).
