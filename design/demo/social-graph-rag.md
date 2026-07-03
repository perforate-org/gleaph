# Social Graph and GraphRAG Comparison Demo
Last updated: 2026-07-03
Anchor timestamp: 2026-07-03 17:52:45 UTC +0000

## Status

**Partially Implemented** — Phase 1 backend subset is implemented. A canonical social graph
manifest and reproducible Router seed operations exist, and the public-timeline, Alice home-feed,
and topic-path prepared-query contracts are verified end-to-end through the application-owned
`gleaph-social-demo-gateway` canister, with anonymous callers invoking the three fixed scenarios
and the Gateway principal acting as the graph-visible default-Executor caller (the Gateway is in
the graph `admins` set so Router can resolve the prepared plan, but it remains a default Router
Executor with no ad-hoc GQL `Read` role). Internet Identity, vector search, LLM calls, GraphRAG
orchestration, and authenticated ownership remain explicitly planned and out of scope for this
slice.

## Phase 1 implementation note

As of 2026-07-03 17:52:45 UTC +0000:

- Canonical manifest: `frontend/apps/knowledge-map/seeds/social-graph.json`.
- Generated seed artifact: `frontend/apps/knowledge-map/seeds/social-seeds.json`.
- Seed generator: `frontend/apps/knowledge-map/scripts/generate-seeds.mjs` now accepts arbitrary
  manifest/output paths while preserving the existing knowledge-map output by default.
- Seed applier: `frontend/apps/knowledge-map/scripts/apply-knowledge-map-seeds.mjs` now accepts
  optional seeds path, canister name, and method name arguments while preserving its original
  knowledge-map defaults.
- Gateway canister: `crates/social-demo-gateway`.
- Gateway PocketIC test target: `crates/pocket-ic-tests/tests/social_graph_demo.rs`.
- Verified contracts:
  - public posts in exact reverse chronological order, excluding the private adversary post,
    executed by an anonymous caller through the Gateway;
  - Alice home feed through `FOLLOWS -> POSTED`, excluding public but unfollowed authors,
    executed by an anonymous caller through the Gateway;
  - topic explanation path through a followee's post, returning the node and edge identities
    that explain why the result was selected, with a non-matching topic adversary excluded,
    executed by an anonymous caller through the Gateway;
  - fail-closed RBAC (the Gateway principal is graph-visible and a default Router Executor with no
    ad-hoc `Read` role; a default-Executor principal and anonymous callers cannot run general
    ad-hoc `gql_query` directly on Router);
  - the Gateway API cannot express arbitrary GQL, prepared-query names, graph names, or
    client-controlled parameters.

## Purpose

Build an internal Singularity Society demo, based on the
[Twitter clone with Supabase tutorial](https://singularitysociety.github.io/societys_statement/development/twitter_supabase/README.html),
that demonstrates three progressively richer retrieval models:

1. a relational-style social application baseline;
2. graph-native relationship exploration and explainable recommendation;
3. semantic retrieval combined with graph expansion and GraphRAG evidence.

The demo is comparative, not a claim that Gleaph already replaces every Supabase capability. It
must show implemented behavior honestly and label external or planned components explicitly.

## Audience

- Singularity Society members comparing relational, graph, vector, and GraphRAG approaches;
- developers evaluating which application problems fit Gleaph;
- Gleaph contributors deciding which product-facing capability to implement next.

## Product claim

The demo should be able to truthfully say:

> Relational storage handles the basic social application well. Gleaph makes changing, multi-hop
> relationships directly queryable, can explain results as paths, and can combine those paths with
> semantic vector retrieval to produce a bounded evidence subgraph for an external LLM.

It must not claim:

- that graph storage is inherently better for simple CRUD or chronological scans;
- that Gleaph implements Supabase-compatible row-level security;
- that Gleaph provides Internet Identity, OAuth, sessions, realtime subscriptions, full-text search,
  embedding generation, or LLM inference;
- that GraphRAG orchestration is already a native Gleaph subsystem.

## Authentication and identity boundary

### Ownership

The **application owns authentication**. For an Internet Computer application, the application
frontend integrates Internet Identity, manages the login/logout experience and identity-bearing
agent, and calls Gleaph using the resulting IC identity.

Gleaph does not own Internet Identity integration or the application session. Gleaph owns the
database-side boundary after the call arrives:

- the Router receives the IC caller principal;
- Router role and prepared-query authorization decide which entrypoints the caller may execute;
- `MSG_CALLER()` makes that principal available to the executed query;
- graph data may store an `IC.PRINCIPAL` property when application data must refer to an identity.

This keeps the dependency direction explicit:

```text
Internet Identity
  -> application identity/session
    -> authenticated IC agent call
      -> Gleaph Router msg_caller
        -> prepared-query and graph-data authorization predicates
```

### SDK and tooling opportunity

Gleaph may provide optional application tooling without becoming the authentication owner:

- a frontend client that accepts an already configured identity-bearing agent;
- generated or typed wrappers for prepared queries;
- helpers for encoding `IC.PRINCIPAL` parameters;
- local-development identity fixtures;
- examples showing Internet Identity wiring and caller-aware prepared queries.

Such tooling must not persist application sessions inside Gleaph, choose an identity provider, or
silently turn a Principal into an application-specific `User` vertex. The application owns that
mapping and its lifecycle.

### Initial demo decision

The first demo deliberately omits Internet Identity. It is a public, read-only, deterministically
seeded comparison experience. Alice, Bob, and other selected users are scenario subjects, not the
authenticated viewer.

Consequences:

- the frontend may execute only the three fixed read-only scenarios through the application-owned
  `gleaph-social-demo-gateway` canister, which delegates to administrator-registered Router
  prepared queries as the Gateway principal;
- the Gateway exposes no arbitrary GQL, prepared-query name, graph selection, or client-controlled
  parameters;
- interactive writes, private drafts, account settings, and ownership-sensitive mutations are out
  of scope;
- the UI must not display a fake login state or imply that the selected scenario user is
  `MSG_CALLER()`;
- the demo must not present its query predicates as an RLS-equivalent security proof;
- Router observes the Gateway principal, not the anonymous browser caller. A future caller-aware
  prepared query may capture `ic_cdk::api::msg_caller()` in the Gateway before `await` and pass it
  as a Gateway-generated `IC.PRINCIPAL` parameter; this slice does not add that plumbing.

Internet Identity should be added in a later application phase when the demo intentionally covers
caller-owned drafts or mutations. That addition is application work unless it reveals a missing
Gleaph authorization primitive.

## Comparison model

### Relational baseline

The baseline mirrors the tutorial's core model:

```text
users
posts(user_id, body, created_at, is_public)
follows(follower_id, followee_id)
```

It demonstrates:

- public posts ordered by time;
- posts from followed users;
- the ownership and visibility rules an RDB application must enforce;
- the join-table representation of a many-to-many follow relation.

This baseline should be described fairly. An RDB is a strong fit for CRUD, integrity constraints,
and chronological pagination. The graph comparison begins where relationship shape and traversal
depth become product behavior.

### Gleaph graph model

```text
(User)-[:FOLLOWS]->(User)
(User)-[:POSTED]->(Post)
(Post)-[:REPLY_TO]->(Post)
(Post)-[:MENTIONS]->(User)
(Post)-[:HAS_TOPIC]->(Topic)
(User)-[:MEMBER_OF]->(Community)
(Post)-[:CITES]->(Document)
```

Canonical social state belongs to Graph shards. Router owns public query entry, name resolution,
prepared-plan lookup, index orchestration, shard dispatch, and result aggregation. Property and
vector indexes remain derived candidate-generation structures rather than alternate social-data
sources of truth.

The graph portion should demonstrate:

- a home timeline reached through `FOLLOWS` and `POSTED` edges;
- mutual follows and friends-of-friends;
- reply, mention, topic, and community paths;
- recommendations accompanied by the path that caused each recommendation;
- discussion or influence paths that can evolve without adding a new join-specific API.

### Vector extension

Post vertices receive canonical embeddings owned by Graph shards. A vector index supplies derived
candidate generation. The Router coordinates vector search and graph execution.

The vector portion should compare:

1. graph-only retrieval;
2. vector-only semantic retrieval;
3. vector candidates constrained or expanded by graph relationships.

Representative questions:

- "Find posts discussing decentralized identity even when they do not use that exact phrase."
- "Among semantically relevant posts, show those written by followed users or members of a selected
  community."
- "Recommend an unfamiliar author and show the social/topic path connecting them to Alice."

Full-text and native hybrid text/vector ranking are not implemented. The first version should not
introduce them merely to complete the demo.

### GraphRAG extension

GraphRAG orchestration remains outside Gleaph for this demo. The preferred IC-native implementation
is an application-owned Rust canister using DFINITY's
[`ic-llm`](https://github.com/dfinity/llm/blob/main/rust/README.md) library to call the LLM canister:

```text
question
  -> external embedding provider
  -> Gleaph vector SEARCH for Post or Chunk candidates
  -> Gleaph graph expansion to Author, Topic, Reply, Claim, and Document
  -> bounded evidence-subgraph serialization
  -> application GraphRAG canister using ic-llm
  -> IC LLM canister
  -> answer plus Gleaph element/path provenance
```

Gleaph owns retrieval and provenance-bearing graph results. The application GraphRAG canister owns
prompt construction, model selection, the `ic-llm` call, answer formatting, and generation policy.
The LLM canister owns model inference. This keeps LLM dependencies out of the Router and Graph
crates, while allowing the deployed demo to remain IC-native.

`ic-llm` supports prompt, chat, and tool-call messages, defaults to the mainnet LLM canister, and can
target a locally deployed LLM canister backed by Ollama. The initial GraphRAG path should nevertheless
be **retrieve first, then generate**: the application calls a fixed, bounded Gleaph prepared query,
serializes its evidence result, and sends that result to the model. It must not give the model a
general-purpose GQL tool or let model-selected tool arguments bypass prepared-query authorization.

The documented `ic-llm` surface is a text-generation/chat interface, not an embedding API. Post and
question embedding generation therefore remains a separate application-owned provider boundary.
The deterministic demo may use precomputed seed embeddings before live embedding generation is
introduced.

The GraphRAG UI must display the evidence subgraph separately from generated prose so viewers can
inspect which posts, people, topics, and citations support the answer.

## Current implementation assessment

As verified against the repository on 2026-07-03 UTC:

| Capability | Current state | Demo use |
|---|---|---|
| Vertex and edge mutation through Router GQL | Implemented | Seed users, posts, and relationships |
| Graph traversal, filtering, ordering, limits, aggregation | Implemented for the required bounded shapes | Timelines and relationship queries |
| Variable-length and shortest-path execution | Implemented for supported shapes | Explainable connection paths |
| Property equality/range indexes | Implemented with documented consistency semantics | Candidate filtering |
| Prepared queries and `MSG_CALLER()` | Implemented | Narrow public read surface now; caller-aware queries later |
| Graph-scoped Router roles | Implemented | Protect administration and ad-hoc query access |
| Application-owned public read Gateway | Implemented | Anonymous callers execute fixed scenarios through the Gateway; no arbitrary GQL/names/params |
| Transparent row-level policy engine | Not implemented | Do not claim RLS parity |
| Canonical vertex embeddings and derived vector indexes | Implemented | Semantic post retrieval |
| Vector `SEARCH` joined with graph execution | Implemented for bounded vertex-only shapes | Graph-aware semantic retrieval |
| GQL vector-index DDL | Planned | Bootstrap through current admin API |
| Full-text and native hybrid provider | Planned | Not required for the first demo |
| Embedding generation | External to Gleaph | Application integration or deterministic seed data |
| LLM inference | IC LLM canister via application-owned `ic-llm` client | GraphRAG generation |
| Realtime/changefeed subscriptions | Not implemented | Use static seed data or explicit refresh |

This table is a dated repository assessment, not a permanent product support matrix. Implementation
work must update the relevant active design contracts when a status changes.

## Demo scenarios

### Scenario A: public timeline baseline

Show public posts in reverse chronological order. Explain that this is intentionally a case where
the relational and graph solutions are both straightforward.

### Scenario B: graph-native home and discovery

Select Alice as a scenario subject and show:

- followed authors' posts;
- mutual connections;
- two-hop author discovery;
- the exact paths that explain each result.

The selection is presentation state, not authentication.

### Scenario C: topic and discussion propagation

Trace a post through replies, mentions, topics, communities, and cited documents. Allow the viewer
to change the starting post without changing the data model or adding a bespoke join endpoint.

### Scenario D: semantic discovery

Enter a natural-language query, retrieve semantically similar posts, and contrast the results with
the graph-only neighborhood. Then apply a graph constraint or expansion and explain the difference.

### Scenario E: GraphRAG evidence

Ask a bounded question, retrieve and expand an evidence subgraph, call the IC LLM canister through
the application-owned `ic-llm` client, and render:

- the generated answer;
- source posts and documents;
- author/topic/reply/citation paths;
- similarity and graph-local ranking signals where available.

## Runtime topology

```mermaid
flowchart LR
    U["Viewer"] --> F["Demo frontend"]
    F --> GW["Social demo Gateway"]
    F --> R["Gleaph Router"]
    GW --> R
    R --> P["Property Index"]
    R --> V["Vector Index"]
    R --> G["Graph shard"]
    F --> O["GraphRAG orchestration service"]
    O --> R
    O --> E["Embedding provider"]
    O --> L["IC LLM canister via ic-llm"]
```

For the first read-only graph comparison, the frontend calls the application-owned Gateway for
the three fixed scenarios. Router sees the Gateway principal on the delegated composite query; the
original browser caller remains anonymous and has no graph visibility. GraphRAG, embedding, and
LLM components remain absent until Phase 3.

When GraphRAG is enabled, the orchestration service calls only Router-facing APIs; it must not
query Graph, Property Index, or Vector Index canisters directly.

Internet Identity is intentionally absent from the initial topology. A later authenticated
application topology inserts Internet Identity between the viewer and the application's
identity-bearing agent; it does not insert Internet Identity inside Gleaph.

## Frontend and reuse strategy

Reuse the existing knowledge-map demo's deployment, Router-row adaptation, SVG graph, playback, and
technical-flow patterns where they fit. Begin as a new scenario family or route in the existing demo
application rather than creating another frontend stack immediately.

The social timeline presentation may introduce dedicated components, but it should share:

- canister environment and Router client setup;
- graph node/edge view-model primitives;
- evidence-path rendering;
- technical execution-flow presentation;
- deterministic seed generation and Router-owned seed application.

Split into a separate application only if the social timeline and GraphRAG interaction model makes
the existing application boundary demonstrably incoherent.

## Source-of-truth and security rules

- Gleaph query results are the source of truth for graph nodes, edges, paths, and retrieval results.
- Scenario definitions may select a starting subject or question but must not contain canonical
  result rows.
- External embeddings are inputs to canonical embedding writes; the vector index remains derived.
- Generated LLM prose is never canonical graph data unless an explicit later feature stores it.
- The frontend and GraphRAG service call Router only.
- The public initial demo exposes no general ad-hoc GQL and no mutation entrypoint.
- A selected scenario identity must never be substituted for `MSG_CALLER()`.
- Authenticated ownership claims require a real application identity and an independently reviewed
  authorization contract.

## Implementation phases

### Phase 1: deterministic graph comparison

- Define a small social seed graph with memorable users, posts, topics, replies, and communities.
- Add public timeline, home/discovery, and propagation scenarios.
- Execute every query through Router.
- Reuse the existing knowledge-map visualization and deployment path where practical.
- Keep the experience public and read-only.

### Phase 2: vector comparison

- Add externally generated Post embeddings to the deterministic seed.
- Register and activate one vector index through the current admin surface.
- Add vector-only and graph-aware semantic retrieval scenarios.
- Make ranking and path explanations independently visible.

### Phase 3: GraphRAG orchestration

- Add a thin application-owned orchestration service.
- Serialize bounded evidence subgraphs with stable element and path provenance.
- Integrate an embedding provider and DFINITY's `ic-llm` client behind application configuration.
- Keep retrieval deterministic and bounded before invoking the LLM canister; do not expose arbitrary
  GQL as an LLM tool in the first implementation.
- Display generated text and evidence separately.

### Phase 4: authenticated ownership demonstration

- Integrate Internet Identity in the application frontend.
- Define the application mapping between Principal and `User` vertex.
- Register caller-aware prepared queries using `MSG_CALLER()`.
- Add private drafts and ownership-sensitive mutations only after adversarial authorization tests.
- Decide from demonstrated gaps whether Gleaph needs additional SDK helpers or a new authorization
  primitive.

Phase 4 is not a prerequisite for the internal retrieval comparison. It is required before the demo
claims authenticated ownership isolation.

## Validation strategy

Prefer contract tests over a broad end-to-end matrix:

- Graph/Router unit or integration tests for query shapes and fail-closed boundaries;
- one PocketIC fixture family for the deterministic social graph and Router-only query path;
- focused vector-search E2E after Phase 2;
- application-level tests with fake embedding and `ic-llm` adapter boundaries for GraphRAG
  serialization;
- a small number of manual browser stories for visual explanation and Internet Identity only when
  Phase 4 begins.

Do not add canbench solely because a demo scenario exists. Add or update benchmarks only when the
implementation changes traversal, ranking, serialization, indexing, or canister execution paths.
The existing feed and friends-of-friends benchmarks should be checked before adding duplicate cases.

## ADR threshold

This demo document is sufficient while implementation composes existing boundaries. Create a
separate ADR only if the demo demonstrates the need for a durable architectural decision, such as:

- a Gleaph-owned generic row-level policy mechanism;
- a new public authorization or prepared-query capability;
- a new embedding-generation ownership boundary;
- native GraphRAG/LLM orchestration inside Gleaph;
- realtime subscriptions or changefeeds;
- a native full-text or hybrid-search provider.

Internet Identity application integration by itself does not require a Gleaph ADR.

## Open decisions

1. Which existing knowledge-map components can support a timeline-plus-graph layout without creating
   a second frontend application?
2. Which deterministic posts and relationships best make each retrieval model visibly different?
3. Should Phase 1 call registered prepared queries directly, or use a narrow Router-owned demo read
   endpoint before the prepared-query frontend wrapper exists?
4. Which embedding provider should the application demo adapter support first, and which `ic-llm`
   model should be the bounded default?
5. What evidence-subgraph row format is sufficient without becoming a new public Router API?
6. Which application SDK helpers are actually needed once Internet Identity is integrated in Phase
   4?

## Related documents

- [knowledge-map.md](knowledge-map.md)
- [../architecture/overview.md](../architecture/overview.md)
- [../security/rbac-and-prepared.md](../security/rbac-and-prepared.md)
- [../gql/extension-syntax.md](../gql/extension-syntax.md)
- [../gql/plan-format.md](../gql/plan-format.md)
- [../index/property-index.md](../index/property-index.md)
- [../index/vector-index.md](../index/vector-index.md)
