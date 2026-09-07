# Gleaph

<p align="center">
  <img src="assets/gleaph_logo_icon.webp" alt="Gleaph" width="720">
</p>

**Gleaph** is a unified graph database engine designed to run on the
[Internet Computer](https://internetcomputer.org/).

One query plan answers graph traversal, full-text search, and vector search —
with data-level authorization enforced inside the plan, so callers only ever
reach what they are allowed to reach.

Gleaph combines:

- **GQL** (Graph Query Language, ISO/IEC 39075) as the standard query interface
- **LARA**, a CSR-derived stable graph storage layout optimized for canisters
- **Full-text search** using varint postings and block-max pruning (derived from
  the MIT-licensed RISE reference implementation)
- **Vector search** over graph-associated embeddings (SPANN-inspired partitioned
  index with quantized centroids)
- **Authorization-aware execution**: data-plane grants are part of the plan, not
  an afterthought filter applied after retrieval
- **Prepared Queries** for safe frontend-to-canister execution
- Typed client SDKs and prepared-query code generation (TypeScript, JavaScript,
  Rust, Motoko)
- A path toward **federated graph shards**

Gleaph keeps data inside the owner's sovereignty boundary: every graph lives in
its own dedicated graph canisters, data-plane grants are part of every query
plan — there is no bypass route for ad-hoc reads — and your data remains
readable through authorized GQL access at all times.

## Why Gleaph?

Modern applications increasingly need to combine structured relationships with
semantic similarity.

A knowledge graph may tell you:

- which documents cite each other
- which users can access which records
- which entities belong to the same project
- which events happened before or after another event

A full-text index may tell you:

- which documents literally contain these terms
- how prominently terms occur across a corpus

A vector index may tell you:

- which passages are semantically close
- which entities are similar to each other
- which memories or documents are relevant to a prompt

Gleaph aims to bring these together: graph traversal, filtering, authorization,
full-text matching, and vector-aware ranking should be expressible as one query
plan instead of being stitched together across unrelated systems.

## GQL

Gleaph uses [GQL](https://www.gqlstandards.org/) (Graph Query Language,
[ISO/IEC 39075](https://www.iso.org/standard/76120.html)) as its query language.

The `gleaph-gql` and `gleaph-gql-planner` crates are intended to remain general-purpose
GQL crates. Internet Computer-specific behavior is implemented outside those portable
layers.

Gleaph-specific syntax — `SEARCH … IN (VECTOR INDEX …)`, `CREATE TEXT INDEX`, inline
edge properties, `GRANT`/`REVOKE`, `GLEAPH.*` procedures, and IC values — is a documented
dialect layer ([gql/extension-syntax.md](design/gql/extension-syntax.md)) implemented
outside the portable crates.

## Storage: LARA

Gleaph's storage model is based on **LARA** (Localized Adjacency Relocation Array), a
graph representation derived from Compressed Sparse Row (CSR).

LARA is designed for stable memory and canister constraints: compact adjacency storage,
predictable traversal, and local relocation without assuming a conventional server
runtime. A tree-CSR mode bounds the mutation and placement costs of high-degree label
buckets with a fixed 4-KiB block store.

## Prepared Queries

Prepared Queries allow privileged callers to pre-register GQL programs that
less-privileged callers can execute. Registration is gated by administrative capability,
publication is an explicit `GRANT EXECUTE ON PREPARED QUERY <name> TO PUBLIC`, and each
registered query stores its statically extracted privilege-requirement set so plan-time
enforcement can check coverage before execution.

This is especially important on the Internet Computer: frontends can safely call Gleaph
directly without requiring a custom backend canister for every application query. The CLI
scaffolds, validates, registers, publishes, and runs prepared queries, and `gleaph codegen`
turns registered queries into typed clients.

## Full-Text Search

Gleaph's full-text search runs in a dedicated text canister owning a custom segment-LSM
inverted index: immutable stable-memory segments, varint postings, and block-max pruning
(WAND / BM-MaxScore) ported from the MIT-licensed RISE reference implementation. Scoring
uses integer fixed-point arithmetic so that evaluation fits within canister instruction
budgets.

Text indexes are declared with Router-owned GQL DDL; the migration path drives the
text-canister backfill lifecycle:

```gql
CREATE TEXT INDEX jp_docs FOR (v:Document) ON (v.body) ANALYZER mecab
```

Queries use `text_score(prop, query)`, which resolves against a ready TEXT definition and
lowers into a top-k or threshold `TextScan` plan operator. The analyzer is creation-pinned:
`multilingual` (the default script-dispatched composite, branded koine), `unicode_bigram`
(CJK character bigrams over NFKC/lowercase), and `mecab` (the morph-dict byte-image
MeCab-format Viterbi engine for Japanese).

## Vector Search

Vector search ships as a SPANN-inspired partitioned index with quantized centroids,
storing embeddings as graph-associated payloads. Candidates are filtered through graph
patterns, labels, properties, or access rules; results come back graph-shaped rather
than isolated vector hits.

Search is authorization-aware: candidate generation oversamples, filters through the
caller's visibility, and iteratively deepens until the requested rows (or the budget) are
reached, so results reflect the top-k of the *visible* subset and partial results carry an
explicit truncation marker. The vector canister stays policy-blind; evaluation happens
inside the plan.

A two-tier precision encoding accelerates the first-stage scan with 1-bit RaBitQ codes
over a seeded randomized rotation, while same-page exact rerank keeps the stored tier as
the advertised quality: stored bytes remain the single source of truth for scoring and
ranking.

This is different from positioning Gleaph as a standalone vector database. The important
idea is graph-first retrieval: vectors become part of the graph execution model.

Future work may include:

- vector-aware GQL extensions or procedures
- hybrid ranking over graph distance, edge weights, properties, and embedding distance
- shard-aware vector retrieval for federated deployments

## GraphRAG Direction

Gleaph is a natural fit for GraphRAG-style systems because it can represent both the
retrieval substrate and the authorization model.

A GraphRAG application could use Gleaph to model:

- documents, chunks, entities, claims, and citations
- relationships between extracted entities
- provenance from generated answers back to source material
- tenant, user, and role-based access control
- semantic similarity between chunks or entities

Instead of retrieving text chunks first and reconstructing context later, Gleaph can make
retrieval graph-aware from the beginning:

1. find semantically relevant chunks
2. traverse to related entities, documents, or citations
3. filter by caller identity and prepared-query permissions
4. return a bounded, explainable context subgraph for generation

Because retrieval goes through authorized plans, nothing enters the generation context
except through the same query machinery — authorization is inherited by construction
rather than bolted onto the pipeline.

## Internet Computer Integration

Gleaph extends GQL for Internet Computer use cases with:

- `IC.PRINCIPAL`
- `IC.MSG_CALLER()`

These allow prepared queries to express caller-aware access patterns directly. The full
dialect surface is documented in
[gql/extension-syntax.md](design/gql/extension-syntax.md).

## Access Control

Authorization has two orthogonal dimensions (see
[design/security/rbac-and-prepared.md](design/security/rbac-and-prepared.md)):

- **Administrative capabilities.** A per-principal `AdminCaps` bitset (default deny)
  governs platform operations: prepared-query registration, index and vector DDL, catalog
  migrations, procedures, federation management, and grant administration. Capability
  holders get no implicit data or metadata access.
- **Data-plane grants.** Per-graph, directional, property-level privileges
  (`GRANT <privilege> ON GRAPH <graph> … TO <subject>`, with a virtual `PUBLIC` subject
  and default deny) are enforced at plan time: every vertex label, edge direction,
  projected property, and mutation in a built plan must be covered by the caller's
  effective privileges. Conditional policies attach caller-derived constant predicates
  and bounded ReBAC `EXISTS` traversal; `EXPLAIN AUTHORIZATION` diagnoses a prepared
  query's privilege coverage.

Cross-tenant metadata reads additionally require time-boxed, approval-backed elevation;
there is no superuser bypass.

Graph shards do not expose arbitrary user GQL. They execute planned work from the router
or trusted peer shards.

## Ownership and Trust

Gleaph is open source (Apache-2.0 / MIT) and self-deployable: every graph lives in its
own dedicated graph canisters on the Internet Computer, and running costs are the cycles
you fund yourself — the engine adds no hidden per-query fees.

Trust is enforced by construction rather than by policy documents:

- graph shards expose no ad-hoc read path; they execute planned work from the router or
  trusted peer shards;
- data-plane grants are bound into every query plan, so nothing reaches a result — or an
  AI context window — except through the same authorization machinery;
- schema migrations are checksummed and parent-chained in a canonical ledger.

Canister controllership is currently operator-managed. The roadmap moves controllership
from platform operators toward graph owners and, ultimately, community governance —
making "who can touch my data" a property of the system instead of the operating team.

## Semantic Query Direction

Long-term, Gleaph extends its optimizer so that AI inference — extraction,
classification, reranking — becomes additional physical operators: budgeted in cycles,
recorded with provenance, and executed inside the sovereignty boundary by default. This
is a design direction under exploration, not a shipped feature.

## Command-line interface

The top-level `gleaph` command drives the platform from a developer machine. Project-level
defaults (networks, deployment principals, directories for migrations, prepared queries,
grants, and codegen output) live in a checked-in `gleaph.toml`, so most commands run
flag-free inside a project.

| Command | Purpose |
| --- | --- |
| `gleaph network start` / `stop` / `status` | Launch a local network, deploy the Account and Provision canisters, and seed the WASM artifact catalog |
| `gleaph login` / `signup` / `identity` | Resolve the caller principal, register an Account, and manage identities |
| `gleaph migration` | Create, validate, plan, and apply immutable schema migrations |
| `gleaph prepared` | Register, publish, and run prepared queries (`new`, `plan`, `status`, `apply`, `drop`, `publish`, `unpublish`, `run`) |
| `gleaph grants apply` | Apply the declarative data-plane grant policy (`GRANT`/`REVOKE` policy files) |
| `gleaph load` | Bulk-load initial vertices and edges through the durable Router bulk-load protocol |
| `gleaph embed ingest` | Push deterministic vertex embeddings into a registered vector index after a bulk load |
| `gleaph vector` | Vector dispatch fleet controls |
| `gleaph codegen` | Generate typed prepared-query clients and adapters |

Code generation exposes the same options as `gleaph-codegen` and targets `typescript`,
`javascript`, `rust`, `rust-canister`, and `motoko`:

```sh
cargo run -p gleaph-cli -- codegen \
  --manifest path/to/manifest.json \
  --target typescript \
  --output src/generated.ts
```

Migration packages live under `./migrations` by default. `new` creates the next
six-digit package containing `migration.toml` and `up.gql`; `plan` validates and
prints the local parent chain. `status` and `apply` are Router-backed commands;
both require `--canister` and accept the network/identity options shown below:

```sh
cargo run -p gleaph-cli -- migration new init_graph --description "Initial graph"
cargo run -p gleaph-cli -- migration plan
cargo run -p gleaph-cli -- migration status --canister <router-principal> -n local
cargo run -p gleaph-cli -- migration apply --canister <router-principal> -n local --identity path/to/admin.pem
```

Migration v1 accepts one or more additive statements per package — `CREATE GRAPH TYPE`,
`CREATE GRAPH … TYPED`, `CREATE INDEX` (several statements may chain as sequential
sub-builds), `CREATE VECTOR INDEX`, and `CREATE TEXT INDEX` (text-canister backfill) —
chained with `NEXT` and applied atomically as one immutable wire payload. The payload is
authored either as a single `up.gql` or as `up/*.gql` fragments concatenated in sorted
filename order. The Router records the immutable checksum and parent link in its canonical
ledger.

A full walkthrough — from `gleaph network start` to graph, full-text, and vector queries
over a real local network — lives in [`demo/knowledge`](./demo/knowledge).

## SDKs

Gleaph ships client surfaces split by consumer (see [`sdk/`](./sdk/)):

- `@gleaph/sdk` (`sdk/client/js`) — TypeScript/JavaScript client runtime with typed DTOs,
  IC transport, and the prepared-query runtime.
- `gleaph-sdk` (`sdk/client/rust`) — Rust application-client SDK over `ic-agent`, mirroring
  the TS surface with dynamic GQL, prepared operations, and bulk load.
- `gleaph-cdk` (`sdk/canister/rust`) — helpers for canisters that call the Router from
  inside the Internet Computer.
- `gleaph-router-wire` (`crates/router-wire`) — the single source of truth for the Router
  data-plane wire contract shared by both Rust clients.

Generated prepared-operation bindings (`gleaph codegen`) compose with these clients through
a shared `PreparedExt` trait, so application clients and canisters share one generated shape.

## Design Documentation

Architecture, GQL layers, federation, execution, storage, and security notes live in
[`design/`](./design/).

## License

This project is licensed under either of [Apache License, Version 2.0](./LICENSE-APACHE) or [MIT License](./LICENSE-MIT) at your option.
