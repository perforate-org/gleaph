# Knowledge Graph Demo

A small technical-knowledge graph that exercises Gleaph's graph-native strengths in one demo:
variable-length path matching, shortest paths, graph-constrained semantic (vector) search, and
citation reach.

## Status of this quickstart

**The browser page and every CLI step run against a real local network**, brought up by
`scripts/bootstrap.sh` on a dedicated managed network owned by this directory's `icp.yaml`
project. A fully automated clean-checkout bring-up **without** that script is not achieved yet:
the pure-CLI path (`gleaph network start` deploying Account/Provision and provisioning the
platform) is blocked by GAP-2026-08-24-006 (`design/implementation-gaps.md`) — launcher gateway
management-call rejection plus launcher lifetime coupling — so this bootstrap script performs the
documented dev-mode topology sequence instead (ADR 0056 `register_graph`, ADR 0071 targetless
index + admin attachment).

Prerequisites: Rust toolchain with the `wasm32-unknown-unknown` target, Docker (OrbStack on
macOS) for icp-cli managed networks, Node ≥ 22 and pnpm, and a debug build of the CLI
(`cargo build -p gleaph-cli` from the repository root).

## Quickstart

All commands run from this directory (`demo/knowledge`).

```bash
# 0. Bring up the platform on a fresh demo-local network (wasm32 builds; long on first run).
#    The final banner prints and exports:
#      GLEAPH_NETWORK / GLEAPH_FETCH_ROOT_KEY / GLEAPH_CANISTER
#    GLEAPH_CANISTER is the single Router-id SSOT for everything below.
scripts/bootstrap.sh

# In a NEW shell, re-create those exports (values differ per machine):
export GLEAPH_NETWORK=http://localhost:<port>   # printed by the banner
export GLEAPH_FETCH_ROOT_KEY=true
export GLEAPH_CANISTER=<router principal>       # printed by the banner

# 1. Non-owner identity for the access-control story (and completion of criterion flows).
gleaph identity new dev && gleaph login

# 2. Bulk load vertices + edges (durable job; state recorded in .load-state.json).
gleaph load --vertices seeds/vertices.jsonl --edges seeds/edges.jsonl

# 3. Ingest deterministic Document embeddings into the registered vector index.
node scripts/gen-embeddings.mjs   # byte-stable regeneration of seeds/embeddings.jsonl
gleaph embed ingest \
  --vertices seeds/vertices.jsonl \
  --embeddings seeds/embeddings.jsonl \
  --graph knowledge

# 4. Register the prepared queries (including the scenario-3 semantic search).
gleaph prepared apply
gleaph prepared status

# 5. Publish all four scenarios for PUBLIC (owner action; ADR 0074 publication).
for op in variable-length-reach shortest-path team-readable-documents citation-reach; do
  gleaph prepared publish "$op"
done

# 6. Grant the PUBLIC data-plane surface the scenarios traverse (ADR 0074 separates
#    EXECUTE on the op from match/traverse/read on the graph).
bash scripts/apply-public-grants.sh

# 7. Smoke-test three registered queries from the shell as the NON-owner (`dev`) —
#    this is the post-publication contrast proof. Scenario 3 is not expressible here
#    because its `$query` parameter is Value::Bytes (768 f32s); `prepared run --param`
#    accepts JSON scalars/arrays only (see Later work). The browser page runs it.
for op in variable-length-reach shortest-path citation-reach; do
  gleaph prepared run "$op"
done

# Unpublishing restores default-deny (non-owner runs start failing again):
#   gleaph prepared unpublish variable-length-reach
```

> **Known issue (2026-08-24, plan 0303 diagnosis):** the pass/deny boundary for
> non-owner execution is the ELEMENT_ID projection. Ops without it execute as dev
> (`variable-length-reach` after re-registration: 7 rows); `citation-reach` denies
> because its edge `ELEMENT_ID(e)` read is tenancy-only in Phase 1 (no edge-property
> grant resource), and `shortest-path` hits an unrelated planner/index-client issue
> under active sibling development. Full matrix and separating experiment:
> GAP-2026-08-24-008 (`design/implementation-gaps.md`) and artifacts §13–14.

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
   returning edge identities.

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
