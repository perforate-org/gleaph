# social-demo-config

Date: 2026-07-09
Status: Implemented
Anchor timestamp: 2026-07-09 15:33:09 UTC +0000

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

## Directory layout

```text
config/
├── users/<user>/
│   ├── profile.yaml
│   └── posts/<stem>.yaml
├── topics/<id>.yaml
├── communities/<id>.yaml
└── scenarios/<id>.yaml
```

## YAML schemas

### `users/<user>/profile.yaml`

| Field | Type | Use |
|-------|------|-----|
| `id` | string | User `demo_id`; must match the directory name. |
| `name` | string | `User.name` property and display label. |
| `bio` | string (optional) | Display-only; not seeded into the graph in this slice. |
| `follows` | list of user ids | Generates `FOLLOWS` edges. |
| `memberships` | list of community ids | Generates `MEMBER_OF` edges. |

### `users/<user>/posts/<stem>.yaml`

| Field | Type | Use |
|-------|------|-----|
| `id` | string (optional) | Post `demo_id`; defaults to `post-<user>-<stem>`. |
| `body` | string | Post `body` property and display label. |
| `created_at` | nat64 (optional) | Defaults to a deterministic value derived from the file path. |
| `is_public` | bool (optional) | Defaults to `true`; stored as `1`/`0` in the graph. |
| `topics` | list of topic ids | Generates `HAS_TOPIC` edges. |
| `embedding` | object | `name`, `dims`, `metric`, `values`; required for deterministic seed equality. |

### `topics/<id>.yaml` and `communities/<id>.yaml`

| Field | Type | Use |
|-------|------|-----|
| `id` | string | `demo_id`; must match the filename. |
| `name` | string | `Topic.name` / `Community.name` property. |

### `scenarios/<id>.yaml`

| Field | Type | Use |
|-------|------|-----|
| `id` | string | PascalCase `SocialDemoScenario` variant name. |
| `preparedQueryId` | string | snake_case name sent to `prepared_register`. |
| `label` | string | Display label. |
| `shortLabel` | string | Short display label. |
| `feedTitle` | string | Feed panel title. |
| `explanationTitle` | string | Explanation panel title. |
| `rdbSummary` | string | Relational summary text. |
| `graphSummary` | string | Graph summary text. |
| `preparedQuery` | string | GQL string for `prepared_register`. |
| `semanticVector` | list of floats or `null` | Optional reference vector for semantic scenarios. |

## Build pipeline

`frontend/apps/social-demo/scripts/build-config.mjs` is the single source of
truth for the emitted artifacts:

1. Walk `config/users/<user>/{profile.yaml,posts/*.yaml}` to materialize User
   and Post vertices.
2. Walk `config/topics/*.yaml` and `config/communities/*.yaml` for layer-0
   nodes.
3. Derive all edges from `follows`, `memberships`, and `topics` fields.
4. Emit `social-graph.json` and `social-seeds.json` in the exact shape consumed
   by the existing apply-knowledge-map-seeds path.
5. Emit `scenarios.generated.ts` and `scenarios.generated.json` from the
   scenario YAMLs.

## Deterministic post id allocator and ordering

Post nodes are ordered by `created_at` descending (ties broken by
`<user>/<stem>` lexical order) so re-running the build reproduces the same
`social-graph.json` byte-for-byte. The deterministic walk applies to:

- User directories (alphabetical).
- Community and topic files (alphabetical by id).
- Post files within a user (loaded in filesystem order and then sorted by
  `created_at`).

## Edge id derivation

- `FOLLOWS`: `<source>-follows-<target>`.
- `MEMBER_OF`: `<source>-member-of-<target>` with the `community-` prefix
  stripped from the target id.
- `POSTED`: `<source>-posted-<fileStem>` (the file stem, not the post id).
- `HAS_TOPIC`: `<postId>-<topicId>`.

## Fallbacks for optional fields

- `created_at`: if omitted, a deterministic value derived from the SHA-256 of
  the post file path.
- `embedding`: if omitted, an 8-dimensional L2Squared vector derived from the
  first 8 bytes of `SHA-256("social-demo:<userId>:<postId>")`, scaled to
  `[-1, 1]`.

## Out of scope

This slice intentionally does not migrate `demo_id` to `u64`, persist `body` in
the graph, move the canister-side semantic query vector into YAML, or store
edges as individual files. Those changes are tracked as follow-up plans.
