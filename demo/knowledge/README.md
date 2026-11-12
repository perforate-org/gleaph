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

# 2. Apply the migration chain: graph type, graph, property indexes, vector index definition,
#    and the TEXT indexes (Document.body + Document.title; a graph may host several).
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
#    EXECUTE publication of all five canonical ops in one declarative file
#    (grants/knowledge.gql). grants apply must follow prepared apply (EXECUTE rows reference
#    registered op names). The expanded `prepared-param-expanded` catalog has its own EXECUTE
#    policy for the parameterized/title ops: `gleaph grants apply --file
#    grants-expanded/knowledge-param-expanded.gql` after the expanded prepared apply.
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

> **Update (2026-08-29, plan 0312 / ADR 0090):** the explorer used to under-render edges
> because the router's edge element-id encoding dropped the bucket label — `(shard, owner,
> slot)` only — so two edges from the same source under different labels with coincident
> per-bucket slot indices collided on the wire (37 seeded edges, 25 distinct ids in the
> demo). [ADR 0090](../design/adr/0090-edge-element-id-label-attribution.md) extends
> `GlobalEdgeId` to `(shard, owner, label, slot)` and bumps the wire layout from 12 to 16
> bytes. The CLI stderr warning still applies (edge ids are physical handles unstable
> across compaction), but two distinct edges no longer claim each other's ids. Host-level
> contract tests in `crates/graph/src/plan/query/executor/path/element_id_per_label.rs`
> pin the new identity; the PocketIC target
> `crates/pocket-ic-tests/tests/edge_element_id_per_label_uniqueness.rs` pins the demo
> shape end-to-end.

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

## Full-text indexes

The migration chain creates two TEXT indexes, each provisioned its own text canister:

| Index | Property | Prepared query | Example |
| --- | --- | --- | --- |
| `document_text` | `Document.body` (+ title via fusion) | `document-text-search` | `gleaph prepared run document-text-search --param 'query="Graph databases"'` |
| `document_title_text` | `Document.title` | `document-title-search` | `gleaph prepared run document-title-search --param 'query="GraphRAG"'` |
| fused body+title | both | `document-search-fused` (`@text-fusion body,title`) | `gleaph prepared run document-search-fused --param 'query="graph"'` |

Inspect counters (documents delivered vs flushed) with
`gleaph text-index --graph knowledge --name <index> --network local`; the `canister=` field names
the target owning that index. `prepared-param-expanded/document-title-search.gql` is the
title-side query template.
The canonical `document-text-search` itself declares `@text-fusion body,title`, so the body scan's
score is summed with the title index's score per document: a title-only term like `GraphRAG` matches
through the normal NL/prepared query, and a document matching body and title scores 4.0 instead of
2.0. `document-search-fused` is the same shape exposed separately from the expanded catalog. The
Router rejects a bad fusion declaration at `prepared apply` time (wrong first property, non-leading
scan, or a property without a Ready TEXT index).

Publication: the canonical body query is published by `grants/knowledge.gql`. The title query and
the parameterized variants live in the expanded catalog, so publish them after the expanded apply:

```bash
gleaph prepared apply --dir prepared-param-expanded
gleaph grants apply --file grants-expanded/knowledge-param-expanded.gql
```

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

## Natural-language routing (prototype)

`gleaph ask` routes a natural-language question to the prepared queries under `prepared/`. It is
the first Phase-0 slice of the decision-model direction recorded in
`design/investigations/2026-11-09-gleaph-decision-model-research.md`:

- candidates are derived from the local prepared directory (name, `///` description, `$param`
  names); only read-only operations are routable;
- ranking is a **deterministic lexical baseline** — IDF-weighted coverage of the question's
  vocabulary over each candidate's description — with an abstain threshold. There is no model, no
  network call, no execution and no authorization decision in this path;
- a golden set can be scored offline: `--eval` reports strict top-1, abstain recall and per-op /
  per-language slices.

```bash
# route one question
gleaph ask -q "Which documents cite the GraphRAG paper up to depth 3?"

# score the golden set (JSONL: question, expected_op | expect_abstain, lang)
gleaph ask --eval eval/golden-routing.jsonl

# machine-readable output
gleaph ask -q "..." --json --limit 3 --threshold 0.34
```

`eval/golden-routing.jsonl` is a hand-authored 16-item smoke seed. The evaluation set is generated:
`eval/routing/{train,dev,test}.jsonl` holds 900/200/400 operation-level items (20% out-of-scope,
English and Japanese, three abstraction levels), split by template family and by held-out entities.
It is generated from the checked-in seeds, so regeneration is byte-stable:

```bash
node scripts/gen-routing-golden.mjs        # writes eval/routing/*
node --test scripts/gen-routing-golden.test.mjs
gleaph ask --eval eval/routing/test.jsonl
```

See [`eval/routing/README.md`](eval/routing/README.md) for the split discipline, the measured router
numbers, the abstain policy and the known limitations (op-level only; slots wait for catalog
parameterization).

Paraphrased slices guard against template-wording overfit:

```bash
node scripts/paraphrase-routing.mjs --input eval/routing/test.jsonl \
  --output eval/routing/test-paraphrased.jsonl --limit 120 \
  --endpoint http://127.0.0.1:8082 --model qwen
node --test scripts/paraphrase-routing.test.mjs
```

`test-paraphrased.jsonl` is evaluation only; `train-paraphrased.jsonl` is for future fine-tuning.

A decision model can replace the baseline without changing the surface: `--model-url` sends a
Jev-compatible System One request (`POST /v1/systemone`, one shared context plus one `noul`
question per candidate). llama.cpp serves that contract locally, and a hosted Jev endpoint works
unchanged. If the endpoint fails, the command warns and falls back to the lexical baseline, so the
surface never depends on a model being up.

```bash
gleaph ask -q "Which documents cite the GraphRAG paper up to depth 3?" \
  --model-url http://127.0.0.1:8080/v1/systemone --model jev-latest \
  --shadow .shadow/routing.jsonl

# Evaluate the lexical baseline and the model on the same golden set.
gleaph ask --eval eval/routing/dev.jsonl \
  --model-url http://127.0.0.1:8080/v1/systemone --model jev-latest \
  --shadow .shadow/comparison.jsonl
```

### Router-owned model routing

A graph with inference enabled routes through the Router, which owns the deployment inference
canister (ADR 0111). The CLI needs only the Router and graph name; the tokenizer is installed in
the inference canister (ADR 0113):

```bash
gleaph ask -q "Which documents cite the GraphRAG paper up to depth 3?" \
  --graph knowledge --canister <ROUTER_PRINCIPAL> --network local --plan
```

The Router builds the per-graph routing context from the graph type and prepared metadata, checks
graph tenancy and the per-graph inference flag, and returns the selected operation plus typed slots
(ADR 0114). The inference canister owns the tokenizer (ADR 0113), so the CLI needs no local
tokenizer; the tokenizer is still installed in the canister for exact token-count quota.

Install the pinned tokenizer through the Router with the operator binary (it checks the release
hash and the on-chain tokenizer fingerprint before enabling the graph):

```bash
cargo run -p gleaph-operator --bin gleaph-inference-install -- \
  --router <ROUTER_PRINCIPAL> --network local \
  --tokenizer "$GLEV_TOKENIZER" --tokenizer-sha256 <TOKENIZER_SHA256> \
  --graph knowledge
```


With `--eval`, adding `--model-url` prints both reports (strict top-1, abstain recall, per-operation
and per-language slices), the top-candidate agreement between the two routers, and any model request
failures (counted as abstentions). `--shadow` then writes one JSONL record per item with both
decisions, which is the raw material for calibration and error triage.

`--catalog` overrides the routing descriptions with a JSON map of "use when" lines (see
`eval/routing/catalog.json`); the model and the lexical baseline share the same override. On the
held-out test split, Laya 421M (choice mode) reached 75.9% strict top-1 versus 65.0% for the lexical
baseline — the numbers and caveats are in [`eval/routing/README.md`](eval/routing/README.md).

`--shadow` appends one JSONL record per routed question containing the lexical decision, the model
decision (or its error) and the candidate list — the raw material for calibration and teacher-label
collection.

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

## Graph-owner routing profile and calibration

Once a graph is inference-enabled, the graph owner/admin can manage its general-routing profile
without the platform operator (ADR 0115). The same identity may also `gleaph prepared apply` the
operations bound to that graph, and `gleaph prepared drop` them; names remain Router-global, so a
name already bound to another graph requires the platform `PREPARE_REGISTER` capability.

```sh
# Read the current profile (JSON for automation).
gleaph route profile --canister "$ROUTER" --graph gleaph.knowledge --json

# Apply a calibrated threshold/policy. bps is 0..=10000 (higher = more abstention).
gleaph route profile --canister "$ROUTER" --graph gleaph.knowledge \
  --set-threshold-bps 2000 --policy-version 2

# Calibrate from a labelled dev set; this prints a recommendation and never writes the profile.
gleaph ask --calibrate --graph gleaph.knowledge --canister "$ROUTER" \
  --eval dev-questions.jsonl --target-oos 0.95 --target-selective 0.85 \
  --policy-version 2 --calibration-out calibration.json
```

Then apply the recommended `recommended_threshold_bps`/`policy_version` with
`gleaph route profile --set-threshold-bps ...`. Calibration only covers the labelled dev set; do
not tune on the acceptance/holdout set.
