# social-demo-config

Date: 2026-07-15
Status: Implemented
Anchor timestamp: 2026-07-15 01:12:11 UTC +0000
Last updated: 2026-07-15 UTC

## Purpose

The social-demo sample graph is authored as per-file YAML under
`frontend/apps/social-demo/config/` rather than as a single hand-maintained JSON
file and inline shell literals. A single build script emits the four artifacts
that the rest of the pipeline already consumes:

1. `frontend/apps/knowledge-map/seeds/social-graph.json` — graph topology.
2. `frontend/apps/knowledge-map/seeds/social-seeds.json` — seed GQL strings.
3. `frontend/apps/social-demo/src/data/scenarios.generated.ts` — TypeScript
   scenario definitions for the React app.
4. `frontend/apps/social-demo/src/data/scenarios.generated.json` — scenario
   metadata for deploy tooling.

All four artifacts are committed to the repository (option A), so the React app
and deploy script can run without first regenerating them.

The current fixture has 23 users, 61 Posts (60 public and one private), 8 reply
edges, and Alice follows six active authors. It is large enough to make multi-hop
traversal visible while keeping deterministic E2E assertions practical. The users
are split into mostly English and mostly Japanese follow clusters, with posts in
both languages. A small number of cross-cluster follows remain to model realistic
boundary connections without erasing the cluster shape.

## Directory layout

```text
config/
├── users/<user>/
│   ├── profile.yaml
│   └── posts/<stem>.yaml
├── topics/<id>.yaml
├── communities/<id>.yaml
├── feeds/<id>.yaml
└── scenarios/<id>.yaml
```

## demo_id representation

User, community, topic, and feed configuration keys remain human-readable. User configuration keys
are emitted as the stable `User.user_id` identity and are used for all user-to-user seed matches;
numeric `demo_id` is only a fixture-local display/reference value and must not identify a user in a
scenario query. Post YAML has no
identity field: `build-config.mjs` derives a stable opaque graph key as `p_<20 hex chars>` from a
SHA-256 namespace plus `users/<author>/posts/<stem>.yaml`. It then assigns every entity a
deterministic global numeric `demo_id` (stored as an Int64 in the graph and decoded as `bigint` on
the Candid wire). Allocation is users, communities, topics, posts, then feeds; each group is sorted
by its configuration path. The current fixture therefore allocates users `1..23`, its community
as `24`, topics `25..26`, Posts `27..87`, and `public-feed` as `88`.

The emitted seed GQL strings use plain integer literals `demo_id: <N>` (no `: u64`
cast — the graph mutation property-expression evaluator does not support
`ExprKind::Cast`). The id-to-string mapping is emitted as `DEMO_ID_MAP` in
`scenarios.generated.ts` so the React app can convert textual keys to numeric
values when needed.

## YAML schemas

### `users/<user>/profile.yaml`

| Field         | Type                  | Use                                                    |
| ------------- | --------------------- | ------------------------------------------------------ |
| `id`          | string                | Stable `User.user_id`; must match the directory name.  |
| `name`        | string                | `User.name` property and display label.                |
| `bio`         | string (optional)     | Display-only; not seeded into the graph in this slice. |
| `follows`     | list of user ids      | Generates `FOLLOWS` edges.                             |
| `memberships` | list of community ids | Generates `MEMBER_OF` edges.                           |

### `users/<user>/posts/<stem>.yaml`

| Field        | Type              | Use                                                                                               |
| ------------ | ----------------- | ------------------------------------------------------------------------------------------------- |
| `body`       | string            | Post `body` property and display label.                                                           |
| `created_at` | nat64 (optional)  | Defaults to a deterministic value derived from the file path.                                     |
| `is_public`  | bool (optional)   | Defaults to `true`; stored as a native GQL BOOL in the graph (compare with `= TRUE` / `= FALSE`). |
| `topics`     | list of topic ids | Generates `HAS_TOPIC` edges.                                                                      |
| `reply_to`   | `<author>/<stem>` (optional) | Resolves a parent config path and generates one canonical outgoing `REPLY_TO` edge.       |

An explicit post `id` is rejected. The generator owns opaque Post identity, resolves
`reply_to` through `<author>/<stem>`, and derives the numeric `demo_id` from the global allocator.
| `embedding` | object | `name`, `dims`, `metric`, `values`; required for deterministic seed equality. |

### `topics/<id>.yaml` and `communities/<id>.yaml`

| Field  | Type   | Use                                       |
| ------ | ------ | ----------------------------------------- |
| `id`   | string | `demo_id`; must match the filename.       |
| `name` | string | `Topic.name` / `Community.name` property. |

### `feeds/<id>.yaml`

| Field      | Type   | Use                                      |
| ---------- | ------ | ---------------------------------------- |
| `id`       | string | Feed `demo_id`; must match the filename. |
| `name`     | string | `Feed.name` property and display label.  |
| `gqlLabel` | string | GQL vertex label (currently `Feed`).     |

### `scenarios/<id>.yaml`

| Field              | Type                     | Use                                               |
| ------------------ | ------------------------ | ------------------------------------------------- |
| `id`               | string                   | PascalCase `SocialDemoScenario` variant name.     |
| `preparedQueryId`  | string                   | snake_case name sent to `prepared_register`.      |
| `label`            | string                   | Display label.                                    |
| `shortLabel`       | string                   | Short display label.                              |
| `feedTitle`        | string                   | Feed panel title.                                 |
| `rdbSummary`       | string                   | Relational summary text.                          |
| `graphSummary`     | string                   | Graph summary text.                               |
| `preparedQuery`    | string                   | GQL string for `prepared_register`.               |
| `semanticVector`   | list of floats or `null` | Optional reference vector for semantic scenarios. |

## Build pipeline

`frontend/apps/social-demo/scripts/build-config.mjs` is the single source of
truth for the emitted artifacts:

1. Walk `config/users/<user>/{profile.yaml,posts/*.yaml}` to materialize User
   and Post vertices.
2. Walk `config/topics/*.yaml` and `config/communities/*.yaml` for layer-0
   nodes.
3. Walk `config/feeds/*.yaml` for layer-0 Feed nodes.
4. Derive all canonical edges from `follows`, `memberships`, `topics`, and `reply_to` fields.
5. Derive materialized feed edges `IN_PUBLIC_FEED` and `IN_HOME_FEED` from
   canonical `POSTED`, `FOLLOWS`, and `is_public` facts, emitting them oldest-first
   so the Graph's default descending fixed-label scan returns newest posts first.
6. Emit ordinary edge `INSERT` mutations. Graph-owned deferred storage prepares a
   dense leaf before the next write, so source fan-out is not a seed-writer concern.
7. Emit `social-graph.json` and `social-seeds.json` in the exact shape consumed
   by the existing apply-knowledge-map-seeds path.
8. Emit `scenarios.generated.ts` and `scenarios.generated.json` from the
   scenario YAMLs.

## Deterministic post id allocator and ordering

Post graph keys and numeric `demo_id` values are allocated by `<user>/<stem>` lexical order;
feed-edge insertion is independently ordered by `created_at`. Both deterministic passes make
re-running the build reproduce the same `social-graph.json` byte-for-byte. The deterministic walk applies to:

- User directories (alphabetical).
- Community and topic files (alphabetical by id).
- Post files within a user (loaded in filesystem order and then sorted by
  `created_at`).

## Edge id derivation

- `FOLLOWS`: `<source>-follows-<target>`.
- `MEMBER_OF`: `<source>-member-of-<target>` with the `community-` prefix
  stripped from the target id.
- `POSTED`: `<source>-posted-<fileStem>` (the file stem, not the post id).
- `REPLY_TO`: `<replyPostId>-reply-to-<parentPostId>`.
- `HAS_TOPIC`: `<postId>-<topicId>`.
- `IN_PUBLIC_FEED`: `post-<postId>-in-public-feed`.
- `IN_HOME_FEED`: `<postId>-in-home-<followerId>`.

Feed edge ids are stable so re-running the build does not duplicate edges in an
idempotent seed.

## Materialized feed edges

`IN_PUBLIC_FEED` and `IN_HOME_FEED` are derived from the canonical graph, not authored
by hand:

- `IN_PUBLIC_FEED`: for every public Post, emit one edge from Post to the `public-feed` Feed.
- `IN_HOME_FEED`: for every public Post, emit one edge from Post to its author and one edge to
  every User that follows that author. Recipient ids are de-duplicated, so a self-follow cannot
  create a duplicate home-feed edge.

Both labels are materialized oldest-first (sorted by `created_at`, ties broken by
`<user>/<fileStem>`). The seed executor inserts edges in manifest order, so Gleaph's
labeled-edge store records insertion order. The default descending fixed-label scan
(`OutEdgeOrder::Descending`) then returns newest posts first without an explicit
`ORDER BY`. `ORDER BY created_at DESC` is therefore no longer required in the
`PublicTimeline` and `AliceHomeFeed` prepared queries.

The `AliceHomeFeed` query retains the redundant `WHERE p.is_public = TRUE` predicate
to preserve the visible read contract and fail closed if the derivation rule ever changes.

The seed runner submits bounded pages through Router `gql_execute_idempotent_batch` (16 mutations
per page by default). Each page item remains an independent idempotent mutation with its own stable
client mutation key; the Router executes items in order and may have partially committed a page when
an item fails. Replaying the page is safe and resumes through the existing idempotent journal. The
`SEED_PAGE_SIZE` environment variable or fifth CLI argument can lower the page size; values above
16 are rejected by both the runner and Router. The legacy single-mutation method remains available
when an explicit method name is passed to the runner.

The runner does not issue `GLEAPH.FINALIZE_*` before ordinary edges. Storage owns write-safety
preparation; finalize procedures remain optional batch-ingest reclaim controls rather than social
state or a per-mutation protocol.

## Reply threads

`reply_to` is authored on the replying Post as `<author>/<stem>` and is the only configuration
source for a reply relationship. The generator resolves that reference to the opaque Post key,
validates it exists, and emits `(reply)-[:REPLY_TO]->(parent)`
after all `POSTED` edges have created their Post endpoints. The public timeline and Alice home-feed
prepared queries project `parent_post_id` through `OPTIONAL MATCH`, preserving root posts as `NULL`.
The frontend reconstructs the visible tree only from rows returned by that feed; a reply whose
parent is absent from the current feed remains visible as a root rather than being dropped.

## Fallbacks for optional fields

- `created_at`: if omitted, a deterministic value derived from the SHA-256 of
  the post file path.
- `embedding`: if omitted, an 8-dimensional L2Squared vector derived from the
  first 8 bytes of `SHA-256("social-demo:<userId>:<postId>")`, scaled to
  `[-1, 1]`.

## Prepared query columns

The 6 scenario `preparedQuery` strings share a common set of RETURN columns:

| Column                                               | Type                                       | Source              | Use                                                     |
| ---------------------------------------------------- | ------------------------------------------ | ------------------- | ------------------------------------------------------- |
| `post_id`                                            | numeric (Int64 in graph, `bigint` on wire) | `p.demo_id`         | Stable deterministic post id from the global allocator. |
| `body`                                               | text                                       | `p.body`            | The post content rendered by the React app.             |
| `created_at`                                         | nat64                                      | `p.created_at`      | Chronological ordering for non-semantic scenarios.      |
| `parent_post_id`                                     | numeric or `NULL`                          | `parent.demo_id`    | Optional canonical reply parent for timeline tree display. |
| `distance`                                           | float32                                    | vector SEARCH       | L2-squared distance for semantic scenarios only.        |
| `follows_edge_id`, `second_follows_edge_id`, `posted_edge_id`, `topic_edge_id` | text | edge `demo_edge_id` | Four-hop relationship trail explanation in `TopicPath`. |
| `topic_id`                                           | numeric (Int64 in graph, `bigint` on wire) | `t.demo_id`         | Stable topic id in `TopicPath`.                         |

All columns except `distance` are stored graph properties or edge properties; the
GQL layer simply projects them. The `body` column was added to the seed GQL in
Plan 0062 and surfaced in the prepared queries in Plan 0064. Plan 0068 extended
the GQL planner's `property_uses()` to include row-local operator expressions, so
the Router-resolved property table now carries `body` for all six scenarios
(including the SEARCH subplan used by AliceSemanticFeed); the planner remains the
single source of truth for the semantic inventory of property names.

## TopicPath workload rationale

`TopicPath` intentionally uses a four-edge path so the demo exposes the workload
shape where graph storage is useful. An RDB can answer the same question, but a
normalized implementation must repeatedly join follow/link tables and carry
intermediate candidate rows before reaching the author, post, and topic. Indexes
can reduce key lookup cost without removing intermediate-result construction or
fan-out. At production scale, read-time execution may therefore require
denormalized paths, materialized recommendations, or precomputed feeds, trading
read latency for write amplification and freshness complexity.

This wording is intentionally workload-specific rather than a claim that graph
databases always outperform RDBs. The external comparison used by the scenario
reported SQL Server as competitive for simple or low-join queries and Neo4j as
faster for more complex multi-join recommendation queries at larger data sizes.
The study is an independent experiment, not a universal benchmark for every
engine or schema.

For an additional concrete reference, a 1-million-user friends-of-friends
benchmark reports MySQL versus Neo4j execution times of `0.016 s / 0.010 s` at
depth 2, `30.267 s / 0.168 s` at depth 3, `1,543.505 s / 1.359 s` at depth 4,
and `not finished in 1 hour / 2.132 s` at depth 5. These figures are workload-
and implementation-specific; they are included to make the depth-related
scaling problem tangible, not as a claim about every RDB or graph engine.

References:

- [Neo4j, Graph database vs. relational database](https://neo4j.com/blog/graph-database/graph-database-vs-relational-database/)
- [Stanescu, A Comparison between a Relational and a Graph Database in the Context of a Recommendation System](https://annals-csis.org/proceedings/2021/pliks/33.pdf), DOI 10.15439/2021F33
- [Akamai, Differences between graph and relational databases](https://www.akamai.com/cloud/guides/differences-between-graph-and-relational-databases) (provided background reference; the scenario does not treat its vendor guidance as benchmark evidence)

## Semantic vector at runtime

The two semantic scenarios (`SemanticDiscovery`, `AliceSemanticFeed`) use an
8-dimensional query vector. The vector is authored in the per-scenario YAML
(`config/scenarios/{semantic-discovery,alice-semantic-feed}.yaml`) under
`semanticVector:` and emitted into `scenarios.generated.json` by
`build-config.mjs`.

The `social-demo-gateway` canister loads the vector at init time through Rust
`include_str!` on `frontend/apps/social-demo/src/data/scenarios.generated.json`
(no separate `build.rs`). The JSON is parsed once with `serde_json::from_str` and
stored in a thread-local; `scenario_to_request` selects the vector for the active
scenario and encodes it as a compact-binary GQL parameter blob. Updating the
vector therefore requires only changing the YAML and rebuilding the canister
(after regenerating the JSON artifact).

## Out of scope

This slice intentionally does not store edges as individual files. That change is tracked as a follow-up plan.
