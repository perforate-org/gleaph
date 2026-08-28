# Knowledge Graph Demo

A small technical-knowledge graph that exercises Gleaph's graph-native strengths in one demo:
variable-length path matching, shortest paths, graph-constrained semantic (vector) search, and
citation reach.

## Status of this quickstart

**The browser page and every CLI step run against a real local network**, brought up entirely by
the Gleaph CLI on the icp-cli network this directory's `icp.yaml` owns. There is no shell
bootstrap: `gleaph network start` detects the platform workspace, builds the wasm set from source
if stale, deploys Account/Provision, seeds the artifact catalog, and lets the migration lane issue
the Router (GAP-2026-08-24-006 is resolved; the old `scripts/bootstrap.sh` and grant/probe scripts
were removed on 2026-08-28 — everything they did is now CLI steps below).

Prerequisites: Rust toolchain with the `wasm32-unknown-unknown` target, Docker (OrbStack on
macOS) for icp-cli managed networks, Node ≥ 22 and pnpm, and a build of the CLI
(`cargo build -p gleaph-cli` from the repository root).

## Quickstart

All commands run from this directory (`demo/knowledge`). The whole chain is one command:

```bash
pnpm quickstart
```

which runs the annotated steps below — every line is the CLI's own idempotent surface, so the
script is safe to re-run (a re-run re-brings-up a fresh demo state). The steps, annotated:

```bash
# 0. Non-owner identity for the access-control story. `network start` registers the
#    session principal's Account automatically.
gleaph identity new dev && gleaph login

# 1. Bring up the platform on a fresh demo-local network (wasm32 builds; long on first run).
#    Builds from source when the checkout is stale, deploys Account/Provision, seeds the
#    artifact catalog, and prints the next-step banner. The Router id is cached in
#    .gleaph/cache/ — no environment exports needed.
gleaph network start

# 2. Apply the migration chain: graph type, graph, property indexes, vector index definition.
gleaph migration apply

# 3. Bulk load vertices + edges (durable job; state recorded in .load-state.json).
gleaph load seeds/vertices.jsonl seeds/edges.jsonl --graph knowledge

# 4. Ingest deterministic Document embeddings into the registered vector index.
node scripts/gen-embeddings.mjs   # byte-stable regeneration of seeds/embeddings.jsonl
gleaph embed ingest \
  --vertices seeds/vertices.jsonl \
  --embeddings seeds/embeddings.jsonl \
  --graph knowledge

# 5. Register the prepared queries, then apply the grant policy: data-plane rows and the
#    EXECUTE publication of all four ops in one declarative file (grants/knowledge.gql).
#    grants apply must follow prepared apply (EXECUTE rows reference registered op names).
gleaph prepared apply
gleaph grants apply

# 6. Regenerate the browser client from the live manifest, or run `pnpm dev` (it chains
#    the gateway-port resolution).
gleaph codegen

# 7. Smoke-test the registered queries from the shell (session = graph owner; the browser
#    page additionally runs them as an anonymous visitor through the generated client).
for op in variable-length-reach shortest-path citation-reach; do
  gleaph prepared run "$op"
done

# Unpublishing restores default-deny (non-owner runs start failing again):
#   gleaph prepared unpublish variable-length-reach
```

# Unpublishing restores default-deny (non-owner runs start failing again):
#   gleaph prepared unpublish variable-length-reach
```

> **Resolved (2026-08-28):** all four scenarios execute for anonymous visitors on a freshly
> brought-up network, and `cite_edge_id` is now declared as `List(Bytes)` so the generated
> clients validate the trail rows. The chain below is the diagnosis history; the probe and
> grant scripts it mentions were removed with the CLI supersession.
>
> **Known issue (2026-08-24, plan 0303 diagnosis):** the pass/deny boundary for
> non-owner execution is the ELEMENT_ID projection. Ops without it execute as dev
> (`variable-length-reach` after re-registration: 7 rows); `citation-reach` denies
> because its edge `ELEMENT_ID(e)` read is tenancy-only in Phase 1 (no edge-property
> grant resource), and `shortest-path` hits an unrelated planner/index-client issue
> under active sibling development. Full matrix and separating experiment:
> GAP-2026-08-24-008 (`design/implementation-gaps.md`) and artifacts §13–14.
>
> **Update (2026-08-25):** `gleaph prepared plan` / `apply` / `run` now print a stderr
> warning when an op projects ELEMENT_ID on an edge variable ("edge element ids are not
> stable identifiers and may change over time"). The tenancy-only edge-id read itself is
> slated for repair in follow-up work — rationale and boundary in
> `design/security/rbac-and-prepared.md`, *Element-id projection guidance*.
>
> **Update (2026-08-26, plan 0306 diagnosis):** the deny half of the 2026-08-24 matrix was
> a misattribution — there is nothing tenancy-only about edge `ELEMENT_ID(e)`. After
> `apply-public-grants.sh`, all four scenarios execute as dev except `shortest-path`
> (planner/index-client issue, unchanged): requirement extraction attributes the edge
> element-id projection through the CITES traversal row like any other read (GAP-2026-08-24-008
> residual follow-up (a); contract test
> `edge_element_id_projection_demands_stay_attributed`). What remains true of the warning:
> citation-reach's returned edge ids are unstable physical handles — the CLI stderr warning
> (commit 06261095f) covers exactly that stability fact.
>
> **Update (2026-08-26, plan 0307 / GAP-2026-08-26-001):** runtime verification corrected
> the previous update on one point — citation-reach registered and granted cleanly but
> could not EXECUTE for anyone yet: `ELEMENT_ID(e)` over a quantified path fail-closed as
> "requires element indexing" (executor gap since June, independent of grants). That gap
> is now implemented: scenario 4 executes end-to-end as dev after `apply-public-grants.sh`,
> returning the reachable documents with `cite_edge_id` as a LIST of edge ids, one per hop
> in traversal order (pinned by PocketIC `knowledge_demo_citation_reach_flow.rs`). The
> stability caveat above still applies to every id in the list. `shortest-path` remains
> blocked by the planner/index-client issue, unchanged.
>
> **Update (2026-08-26, plans 0310/0311 / GAP-2026-08-26-004):** the last blocked scenario
> is closed, and the "planner/index-client issue" framing is corrected. `shortest-path`'s
> plan leads with two labeled scans; the router's label-only multi-variable fallback
> dispatched it without seeds, which federation-configured shards reject fail-closed —
> property indexes were never the cause. Standalone topologies now seed those anchors via
> existing label postings (multi-shard stays fail-closed behind the federated-traversal
> ADR), prepared apply additionally rejects unservable index anchors up front, and the
> direction fix above makes the op read incoming from the sink. Scenario 2 now executes
> end-to-end as dev after `apply-public-grants.sh` (pinned by PocketIC
> `knowledge_demo_shortest_path_flow.rs`). All four demo scenarios execute as dev after
> grants.

### Browser host

```bash
pnpm install                 # once per checkout (workspace root)
pnpm generate:client         # regenerate src/generated/index.ts from the live Router
pnpm write-env --canister "$GLEAPH_CANISTER" --gateway-url "$GLEAPH_NETWORK"
pnpm dev                     # vite+ ready line, then open http://localhost:3000/
```

`GLEAPH_NETWORK` is dynamic (icp-cli assigns a new gateway port per network recreation);
resolve the current value with `icp network status local --json` from this directory.

The page lists the four scenarios. Selecting one executes it through the generated typed client
against the live Router and renders the returned rows:

- Scenarios 1, 2, and 4 run without parameters (literal-bound sources).
- Scenario 3 (`team-readable-documents`) needs a `$query` vector (`Value::Bytes`, 768
  little-endian f32s). The page generates it in-page from the same deterministic recipe as the
  ingested embeddings (`scripts/embedding-core.mjs`, shared with `gen-embeddings.mjs`), seeded by
  source id `d4`. By construction the top-1 row is therefore **"Vector search in graphs"**
  (`d4`); rows below it come from cosine similarity over the remaining public Platform-owned
  documents.

Errors from the Router surface verbatim in the page (e.g. running an unpublished scenario as a
non-owner shows `not authorized`).

## Embedding ingestion path

- `seeds/embeddings.jsonl` carries one `{source_id, values}` row per `Document`, generated by
  `scripts/gen-embeddings.mjs`. The generator is deterministic (sha256-seeded xorshift32,
  L2-normalized, 768 dims) — rerunning it must not diff the checked-in file
  (`node --test scripts/embedding-core.test.mjs` pins this against the seeds).
- The bulk-load wire carries no embedding bytes by design (ADR 0064 §6): the vector canister is
  the sole owner of embedding data. `gleaph embed ingest` runs strictly after a Completed load:
  it re-reads the vertices NDJSON for `source_id` order, pages the load job's chunk receipts to
  reconstruct `source_id → encoded vertex id`, and pushes batches through Router's existing
  admin batch API (`ingest_vertex_embeddings`). This is the B案 ingestion path: zero canister
  changes, no durable-wire format changes.
- Dimension count and finiteness stay Router-owned (`validate_embedding_values` against the
  registered spec); the CLI validates structural input rules only.

## Prepared queries

1. **Variable-length reach** (`variable-length-reach.gql`) — Concepts that can reach the
   anchor concept "Graph databases" over `RELATED_TO` within 1..3 hops (the seeds author
   edges toward the anchor, which is a pure sink — an outgoing pattern legitimately returns
   0 rows, which is how the direction was settled). Surfaces path quantifiers.
2. **Shortest path** (`shortest-path.gql`) — `MATCH ANY SHORTEST` between two concepts;
   surfaces graph-native shortest path.
3. **Graph-constrained semantic search** (`team-readable-documents.gql`) — `SEARCH` over the
   `document_embedding` vector index for Documents similar to `$query`, constrained to public
   Documents owned by members of Team "Platform". `$query` is the encoded F32 vector produced by
   the embedding generator (in-page via the shared module).
4. **Citation reach** (`citation-reach.gql`) — Documents reachable through `CITES` up to depth 3,
   returning edge identities. The CLI prints a warning when registering or running this op
   because it projects `ELEMENT_ID` on an edge variable: edge element ids are not stable
   identifiers and may change over time, so treat the returned ids as a display trail, not as
   stored references.

## Environment wiring reference (`pnpm write-env`)

`.env.local` carries three Vite variables. Precedence mirrors the script:

- Router id: `--canister` flag → `GLEAPH_CANISTER` env → `.gleaph/cache/account/<env>.router.json`
  (lazy issuance cache; only exists on Account-based networks).
- Gateway URL: `--gateway-url` flag → `GLEAPH_GATEWAY_URL` env → the Gleaph-owned launcher status
  file (`$TMPDIR/gleaph-local-status/status.json`; future pure-CLI path) → default
  `http://localhost:8000`.
- `VITE_FETCH_ROOT_KEY=true` always (local networks serve a self-signed root key).

Assertions: `node --test scripts/write-env.test.mjs`.

## Later work

- Pure-CLI platform bring-up (see GAP-2026-08-24-006) so `bootstrap.sh` collapses into
  `gleaph network start`.
- `prepared run --param` support for binary/f32-array values (unblocks the scenario-3 CLI smoke).
