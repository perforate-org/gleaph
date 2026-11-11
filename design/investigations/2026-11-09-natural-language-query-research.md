# Natural-language query over Gleaph: hybrid retrieval, decision-model routing, and the IC reality check

Last updated: 2026-11-09
Status: **research record** (append-only). This document is the evidence base for a future ADR
on a natural-language query surface. It decides nothing and ships no capability.

Markers used below: `[verified]` = the primary source was read and says this; `[claimed]` = only
a vendor, blog or author claims it, or only a secondary write-up reports it; `[unverified]` = no
primary source was reached. Numbers carry the publication date of their source.

## Purpose

Answer three questions before any code is written:

1. Which way of combining **graph traversal, full-text and vector retrieval** is both current and
   cheap enough for a canister-budget engine, and what is actually measured about it?
2. What is **JEV** (the "mute model" that went viral in September 2026), why does it keep appearing
   next to graph databases, and does anything about it concern the Internet Computer?
3. What can a **natural-language query layer** look like on top of Gleaph without moving the trust
   boundaries the engine already enforces?

## Non-goals

- No contract change, no ADR, no parser/planner work in this record.
- No evaluation of Gleaph against public retrieval benchmarks (no such benchmark covers GQL
  dialects; see §8).
- No claim that the AI path is shipped. The positioning note (`plans/positioning.md`, local-only)
  requires future/passive voice for AI until a Phase-4 implementation exists; this record follows
  that rule.

## Summary

1. **Free-form NL→GQL is not shippable to untrusted callers in 2026.** Best measured exact match
   is ~62–65% for Cypher and 35–45% for GQL; multi-turn session-exact is under 5%; an 8B model
   produces *executable* queries 90.7% of the time and *correct* ones 18.8% of the time.
2. **The viable first NL surface is a router over prepared queries with typed slots.** Measured
   JEV-style routing reaches 89.5–93.8% top-1 on a 150-way choice at 90–137 ms, and it does not
   move Gleaph's trust boundary: the prepared query's static requirement set still authorizes.
3. **The engine-side gap is one deterministic fusion operator, not a new index type.** Neo4j now
   ships exactly this shape (`SEARCH` for vector since 2026.01 and full-text since 2026.09,
   fused by weighted RRF in Cypher). Gleaph already has both candidate generators; what is
   missing is an RRF/WRRF merge over two bounded, already-authorized streams.
4. **The graph's defensible value is constraints, provenance and authorization — not ranking.**
   No primary source shows three-way graph+text+vector fusion beating two-way text+vector fusion,
   and 2025–2026 replications report GraphRAG underperforming vanilla RAG on general tasks.
5. **JEV is a hosted decision model, not a graph feature.** It is cheap ($0.042/M input tokens),
   fast (~0.1–0.5 s), typed (`choice`/`score`/`noul`) and paid for by someone else's trust
   boundary: the question leaves the operator's boundary, which conflicts with Gleaph's
   sovereignty claim unless it is self-hosted.
6. **"JEV on IC" is a documented absence of vocabulary and a small, real presence of code.**
   No forum/GitHub discussion uses the name, but onicai's `llama_cpp_canister` runs the TypeSafe
   System One contract in a canister (`run_decision`, Julia-1/Laya/Kev-0.8B) and an independent
   project runs a 4B model in a canister in 27–258 s per update. None of it is affordable as a
   query-path router; the hosted API is 200–7,000× cheaper per decision.

## 1. What the engine provides today

Everything below is from the current repository, not from this research.

### 1.1 Retrieval primitives

| Primitive | State | Source of truth |
| --- | --- | --- |
| Graph traversal, path quantifiers, `ANY SHORTEST`, label/property indexes | Implemented | `design/architecture/overview.md`, `design/index/property-index.md` |
| Full-text index, `text_score(prop, query)` with top-k, threshold and compound lowering | Implemented v1 | `design/index/text-index.md` |
| Candidate-scoped text scoring (`search_candidates(query, keys)`, ≤256 keys, all matching scores) | Implemented (plan 0344) | `design/index/text-index.md` |
| Vector index and `SEARCH ... IN (VECTOR INDEX ...)` (leading and non-leading, in-index filters) | Partially implemented | `design/index/vector-index.md`, `design/gql/extension-syntax.md` |
| Authorization inside the plan (grants + policy predicates), no ad-hoc bypass for browsers | Implemented | `design/adr/0078-authz-aware-vector-search.md`, `design/security/rbac-and-prepared.md` |
| Prepared queries: name-addressed, static requirement set, `GRANT EXECUTE ON PREPARED QUERY` | Implemented | `design/security/rbac-and-prepared.md` |

Two numbers matter for budgeting. The text canister's cost is measured: a global top-100 costs
4.00M instructions, the same query restricted to 256 candidate keys costs 2.79M. The vector
canister's Slice 6 measurement is ~164K instructions per scanned row, which made a naive
148K-row partition scan (~24B instructions) infeasible.

Three limits matter for a hybrid design:

- `text_score` v1 scores `weight × tf`; the fixed-point BM25 formula is explicitly deferred until
  the catalog owns per-term weights (`design/index/text-index.md`). The score is therefore
  **unbounded and not comparable** to a cosine similarity.
- `SEARCH` in-index disjunctive filters are capped at 4096 candidates; text index v1 has no
  phrase/fuzzy/trigram support.
- Vector search is partly implemented: PQ and HNSW are deferred, and exact-scan fallbacks remain.

### 1.2 Composition: what works today and what does not

`SEARCH` can be constrained by graph pre-conditions and RBAC predicates, and `text_score` can be
evaluated over candidates produced by a traversal prefix (ADR 0095). So *graph + one retrieval
signal* is expressible in one plan.

**Graph + full-text + vector fused by score is not implemented.** `design/demo/social-graph-rag.md`
states it directly: "Full-text and native hybrid text/vector ranking are not implemented." The
`README.md` headline — "One query plan answers graph traversal, full-text search, and vector
search" — is a statement about the engine's *shape* (all three live under one planner and one
authorization model), and it is broader than the demonstrated fusion contract today. §10 records
the wording gap.

### 1.3 Trust boundaries the NL layer must not move

The relevant contracts already exist and are unusually strict:

- The vector canister is **policy-blind**; visibility is enforced by the graph tail plan, and
  "nothing enters LLM context that did not pass layer 2 through the same query machinery"
  (`design/adr/0078-authz-aware-vector-search.md`).
- Conditional policies bind vector search and GraphRAG: candidate generation and context assembly
  must observe policy-filtered visibility (`design/adr/0075-conditional-policies-constant-pushdown.md` §7).
- Treating a prepared query's requirement set at registration time means *whoever selects the
  query* cannot widen authority (`design/security/rbac-and-prepared.md`).
- LLM-based GraphRAG generation is explicitly deferred, and the demo contract requires
  **retrieve first, then generate**: the model gets a bounded evidence result, never a
  general-purpose GQL tool (`design/demo/knowledge-graph-demo.md`, `design/demo/social-graph-rag.md`).

Any NL design that honors these four sentences is compatible with the engine. Anything that
filters, ranks or assembles context outside a plan is not.

## 2. Hybrid retrieval: what the evidence actually supports

### 2.1 The reproducible win is lexical + dense fusion, not "graph"

The one hybrid pattern with consistent, reproducible evidence is **BM25 + dense retrieval fused by
rank**. It is offered as a first-class option by Elasticsearch, Weaviate, Qdrant and Neo4j (Weaviate
made normalized-score fusion its default in v1.24; Qdrant's RRF default `k` is 2, which is itself
evidence that the parameter is a tuning knob, not a constant). The graph-signal claim is
weaker than the marketing suggests: 2025–2026 replications report that GraphRAG "frequently
underperforms vanilla RAG on many real-world tasks", and even graph-positive work (HippoRAG 2)
concedes that graph methods trade factual recall for associativity.

No primary source found in this survey shows a *three-way* graph+text+vector fusion beating a
two-way text+vector fusion on retrieval metrics. The graph is justified by three other things,
which the engine can actually defend:

1. **Constraints** — restrict retrieval to an authorized or structurally reached set (this is what
   `SEARCH ... WHERE` and candidate-scoped `text_score` already do).
2. **Provenance** — the path/edge identities that explain *why* a row was returned
   (`design/demo/knowledge-graph-demo.md` scenario 4).
3. **Authorization** — the thing that cannot be bolted on afterwards.

That reframing matters for product claims: Gleaph's differentiator is not "graph improves
ranking", it is "retrieval is constrained and authorized before anything is generated".

### 2.2 Fusion belongs in the query plan, not in a new index type

Neo4j introduced a `SEARCH` clause for vector indexes in 2026.01 and for full-text indexes in
2026.09, and documents the combination as a *retrieval pattern* computed in Cypher with weighted
reciprocal rank fusion — not as a separate hybrid index. Its manual says the same thing in
operational terms: "When combining vector results with full-text results or other scored sources,
rank each source independently rather than comparing raw score values directly"
(Neo4j Cypher Manual, `SEARCH`, accessed 2026-11-09) `[verified]`.

This is the strongest single confirmation that Gleaph's architecture is right: two search clauses,
one plan, deterministic merge, no new canister and no new index kind. What is missing on the
Gleaph side is exactly one thing — a deterministic operator that fuses two ranked streams after
each branch's authorization predicates have been lowered.

### 2.3 Fusion function: RRF first, weights later, normalization last

Reciprocal rank fusion is `score(d) = Σ_r 1 / (k + rank_r(d))`. Why it is the right default here:

- It is **rank-based**, so it does not require the text score and the vector score to be on the
  same scale — which they cannot be today (`weight × tf` is unbounded; cosine is bounded;
  L2 is a distance).
- It is **deterministic under fixed-point arithmetic**: integer `floor(2^20 / (k + rank))` terms
  summed in a fixed order reproduce exactly, with no floating-point normalization to argue about.
- Its parameter can be exposed without re-architecting: `k` and per-source weights are settings.

The counter-evidence is real and should be recorded: Bruch et al. (TOIS 2023, arXiv 2210.11934)
report that RRF is sensitive to its parameter and that a convex combination of normalized scores
beats RRF when a small amount of tuning data exists. Also, "k = 60 is optimal" has no primary
support — Elasticsearch, Weaviate and Neo4j document 60 as a habit; Qdrant ships 2.

Consequence: ship RRF with `k` as configuration, default near 60, and do not build score
normalization first. Convex combination becomes worth revisiting the moment `text_score` is true
BM25 and both streams produce interpretable, bounded scores.

### 2.4 Cost shape

Fusion is `O(k log k)` after each source has produced its top-k, so the cost is dominated by the
two candidate generations, not by the fusion. That is why the contract has to make the two
streams *bounded*: `sourceK` as a literal, both branches top-k only, no unbounded scans. The
existing `LIMIT`-literal discipline in the planner is the pattern to copy.

## 3. JEV: what it is, and what it means for a graph engine

### 3.1 Facts

"Jev" is the decision model from **TypeSafe AI** (San Francisco, founded 2024; $40M seed led by
DCVC; founders include Diogo Almeida, an InstructGPT co-author) `[claimed]`. It does not generate
text. You send a `state` plus typed `questions`, and it returns typed answers with probabilities:

- `choice` — one option from a declared list (up to 255/256 options),
- `score` — a position on ordered levels,
- `noul` — the calibrated probability that a yes/no statement is true,
- `box` — a bounding box (added in Jev 1.14, October 2026).

API shape: `POST https://api.typesafe.ai/v1/systemone` with `{model, state, questions}`; answers
carry `choice`/`probabilities`/`confidence` (confidence approximates `(n·p − 1)/(n − 1)`).
`jev-1.13.0` and `jev-1.14.0` cost **$0.042 per million input tokens, output free**, with a 64k
per-request / 32k per-question context (`docs.typesafe.ai/models`, accessed 2026-11-09) `[verified]`.

It is non-autoregressive, trained with RLCD for calibrated decisions, and the same weights serve
every customer (no fine-tuning), so its accuracy is shaped entirely by the request. The name comes
from **Jevons' paradox**: make a decision cheap enough and software will call it everywhere
(Japanese write-up, note.com, 2026-09-25) `[claimed]`.

Known weaknesses, from the vendor's own docs and an independent integrator's measurements:

- arithmetic, dates, counting and multi-step reasoning are weak; prompt injection moves answers;
  non-English is accepted but lower accuracy;
- it cannot step outside the declared options, but it can be **confidently wrong** inside them;
- option order alone changes ~7% of answers, and repeated calls on the same input vary ~4.3%
  (open model card for a distilled JEV-style model, `autotrust/JEV-27B`) `[claimed]`;
- it is a **hosted US service** — the state leaves your boundary.

### 3.2 Why it keeps appearing next to graph databases

Two concrete graph patterns exist, both from September 2026:

1. **Beam search over relationships.** `jexp/neo4jev` (MIT, 2026-09-18) walks a Neo4j graph one
   hop at a time: the current node's outgoing relationships become `choice` options (described by
   relationship type, properties and the far node's label/properties), Jev returns a distribution
   over next hops, and a `noul` "has the goal been reached" rides in the same call. Top-k over
   summed log-probabilities makes it a beam search. Schema is discovered live, not hardcoded.
2. **Subgraph pruning inside GraphRAG.** A September 2026 Towards Data Science article builds a
   dual-engine pattern: a router either has an LLM write Cypher *or* has Jev's `choice` map the
   question onto pre-compiled parameterized Cypher templates, then Jev prunes the candidate
   subgraph with `noul`/`score`, and an LLM only writes the final answer. It reports context-token
   reductions up to 90% `[claimed]`.

Those two patterns are exactly the two places a graph engine hurts: deciding *which hop or query
template is meant*, and deciding *which of the returned candidates matter*. Both are closed,
repeated, machine-consumed judgments — the shape Jev is built for.

### 3.3 What transfers to Gleaph

- **Template routing over free-form query generation** (§4.2) is the viable first surface, and Jev
  is a working, measured instance of "pick one of N prepared options with confidence".
- **Reranking a shortlist by a calibrated yes/no question** is measured: on 40 CLERC legal queries
  with a BM25 shortlist of 30, one `noul` question per pair raised top-1 from 5% to 30% and top-10
  from 38% to 65% (`docs.typesafe.ai/cookbooks/rerank_typesafe`, accessed 2026-11-09) `[claimed]`
  — a vendor cookbook, but the recipe is small and reproducible.
- **Typed decisions make the control flow deterministic**: code branches on values, so the engine
  never parses prose.
- **The contract is portable.** A canister implementation of the System One interface already
  exists (onicai `run_decision`, §5.3), so the value is the interface, not the vendor.

### 3.4 What does not transfer

- **The hosted Jev service cannot run on a canister**: closed weights, US-hosted API. What can run
  there is the *contract* with small open models — at 200–7,000× the per-decision cost (§5.4).
- **Jev must not decide authorization or visibility.** Those stay in the plan (ADR 0075/0078).
  A router may choose *which* prepared query to run; the engine decides what it may return.
- **Jev must not emit values.** It cannot return free text or numbers. Slot values have to come
  from an enum or from an entity-resolution step (`SEARCH`/`text_score`/property index), then be
  bound as typed parameters.
- **Hosting conflicts with the sovereignty story.** Sending the user's question to a US API is
  the same architectural sin Gleaph criticizes in cloud GraphRAG. If the sovereignty claim is the
  product, the router model has to be self-hosted or IC-native.

## 4. NL → GQL: the measured state of the art

### 4.1 Free-form generation is not shippable to untrusted callers

2026 numbers from primary sources except where marked:

| Setting | Best measured exact-match (EX) | Source |
| --- | --- | --- |
| Text→Cypher, single-shot, strong proprietary models | 61.6% | CypherBench, ACL 2025 (arXiv 2412.18702); figure from a third-party reproduction table `[claimed]` |
| Text→Cypher, multi-turn, guided protocol | 64.7% per turn, **4.6% session-exact** | CypherTurn, EMNLP 2026 (arXiv 2609.36987) |
| Text→GQL (ISO 39075), best proprietary, 2-dialect average | 35.4% | GQLBench, ACL 2026 |
| Text→GQL, 8B fine-tune | 45.1% EX, **90.8% grammar** | Text2GraphQuery-Bench, arXiv 2602.11745 |
| Text→Cypher, 8B zero-shot | **18.8% EX, 90.7% executable** | CypherBench third-party reproduction table `[claimed]` |

The Cypher rows come from a third-party reproduction table rather than the papers' own baseline
tables; re-check them against the papers before quoting them in a grant document. The structural
claim does not depend on the exact percentage: executable-but-wrong is the dominant failure mode.

The last row is the decisive one: **small models produce syntactically valid queries that are
usually semantically wrong.** Grammar-constrained decoding raises executability, not correctness.
Multi-turn is worse: chain-error rates of 0.43–0.71 mean one bad turn poisons the next.

### 4.2 Template routing is the viable first surface

The same literature shows the alternative is already practical: a router that picks a prepared
query and fills typed slots. Measured on a distilled JEV-style model: 150-way intent choice at
89.5% top-1 with names only and 93.8% with a one-line description per option, 90–137 ms per
decision, one prefill and no decoding (`autotrust/JEV-27B` model card, accessed 2026-11-09)
`[claimed]`.

That maps onto Gleaph almost without changes:

- the catalog of prepared queries becomes the choice set;
- each prepared query carries a one-line "use when" description (the only intervention that
  measurably improved routing in the cited card);
- graph/role partitioning keeps each decision under the ~256-option cap;
- slot values are enums or entity-resolution results, bound as the existing parameter blob;
- low confidence produces a clarification or a two-candidate confirmation, never a silent guess.

The security argument is stronger than the quality argument: routing **does not move the trust
boundary**. The prepared query's static requirement set still decides authorization, so a bad
routing decision can return the wrong *rows the caller was allowed to see* — not unauthorized rows.

### 4.3 Repair-loop economics, if generation is ever allowed

`LAST-CQ` (EMNLP 2026 REALM workshop, arXiv 2609.12746) is the best evidence available:
execution-grounded repair recovers 91.7% of single-shot failures, the *quality* of feedback barely
matters (raw DB error text performs within 0.2% of LLM-synthesized explanations), spending the
same budget on parallel sampling is 10–11% worse, and the first-attempt cost is unchanged.

So the only defensible generation loop is: generate → parse → validate against the seeded schema →
reject writes → dry-run plan → **one** repair retry with the engine's own error text.

Evaluation must use execution accuracy on result sets. N-gram metrics over-scored 65.9% of the
cases in that study relative to set equivalence, and LLM judges were 9 points more optimistic than
human labels.

### 4.4 The standard is moving

ISO/IEC 39075:2024 was published 2024-04-12; **Cor 1:2026 has already been issued**, and a second
edition (`CD 39075`) is in development `[verified]`. Any fine-tuned "GQL model" is a bet against a
moving grammar; template routing and parser-validated generation track the standard for free.

This also reinforces the repository rule: `gleaph-gql` and `gleaph-gql-planner` stay
general-purpose. NL metadata (template descriptions, slot types, model clients) belongs to the NL
layer, never to the core crates.

## 5. The Internet Computer side

### 5.1 The LLM canister: a gateway, not replicated inference

`w36hm-eqaaa-aaaal-qr76a-cai` exposes a Prompt API and a Chat API and is callable from canisters
through `ic-llm` (Rust) or `mo:llm` (Motoko). Operational facts:

- The crate is at **2.0.0 (2026-10-07)** and exposes six models: `llama3.1:8b`, `qwen3:32b`,
  `llama4-scout`, `qwen2.5:0.5b` (free) and `gemma3:27b`, `z-ai:glm-5.2` (paid). Every `send()`
  attaches 100B cycles, free models refund them, and a caller holding less than 100B traps
  (`github.com/dfinity/llm/rust/README.md`) `[verified]`.
- Limits: at most 10 messages, 10 KiB prompt, 1000 output tokens, no streaming, and it does not
  work on a local replica (a mock is required) `[verified]`.
- The docs page still says "Llama 3.1 8B only", `ic-llm` 1.1.0, and "inference runs onchain (ICP
  nodes), deterministic via the random beacon". The repository and the Candid interface say
  otherwise: prompts are queued and polled by stateless **AI workers**, the Candid
  `backend_config` is `variant { worker; ollama; openrouter }`, and the README warns that
  confidentiality is not guaranteed for prompts `[verified on both sides, conflict recorded in
  §10.4]`.

So the correct mental model is an **off-chain worker gateway operated by DFINITY**, and the trust
model is "trust DFINITY", not "trust consensus". That is fine for an app canister doing slot
extraction or answer synthesis at human latency (the deferred GraphRAG contract already names
`ic-llm`); it is not fine as a per-candidate operation.

### 5.2 Inference inside a canister is real and instruction-bound

Instruction limits that shape every design below (`docs.internetcomputer.org/references/resource-limits`,
accessed 2026-11-09) `[verified]`: 40B instructions per update call, 5B per query call, 300B per
install/upgrade, 4 GiB wasm32 heap (6 GiB wasm64), 500 GiB stable memory, 12 MiB code section,
2 MiB ingress / 10 MiB same-subnet payload. No GPU; SIMD128 only.

Evidence:

- *On-Chain LLM Inference Under Instruction Budgets* (Aerni, Fluck, Becker; DOI
  10.5281/zenodo.20607598, June 2026) `[verified]`: per-call budget `B = 40×10⁹`; the same
  Qwen2.5-0.5B Q8_0 yields **10 tokens/call** on the onicai baseline and **29 tokens/call** on the
  authors' fork; `tok/call ≈ B/(α_eff·2P)` with `α_eff = 1.527` (BCa CI 1.374–1.65) across 11
  models; matmul is 98.7% of decode; the ternary SIMD floor is `α_eff = 1.03` with byte-identical
  outputs across replicas; multi-call sessions pay `43.0M + 0.177M·n_kv` instructions of I/O tax.
  The same artifact measures on-chain embeddings: EmbeddingGemma-300M Q4_0 at ~126 input tok/call
  and concludes that retrieval fits the instruction-bound regime well.
- `onicai/llama_cpp_canister` (★88, pushed 2026-10-29) runs llama.cpp in wasm: chat models
  Qwen3-0.6B (default), Qwen2.5-0.5B, Qwen3-1.7B, LFM2.5-1.2B/2.6B, Gemma-3-270M, with
  `wasm_memory_limit` raised to 3.75 GiB `[verified]`.
- 8B is out of reach: `40e9 / (1.53 × 2 × 8e9) ≈ 1.6` tokens per update, so a 1000-token answer
  needs more than 600 updates — which is exactly why the LLM canister delegates to workers.

Practical ceiling today: roughly **0.6–1.7B for chat, 0.4–0.8B for decision models**, and 4B only
with weight packs in stable memory plus heap exhaustion (below).

### 5.3 "JEV on IC": absent vocabulary, present code

Searches run on 2026-11-09 (Discourse `search.json` and the GitHub Search API; forum queries were
re-run because that search rate-limits):

| Query | Result |
| --- | --- |
| forum `q=JEV` / `q=jev` | 0 posts / 0 topics (two runs; the same session returned one hit for `"decision model"`, so the index was live) |
| forum `q=typesafe` | 3 posts, all about TypeScript tooling, unrelated |
| forum `q="router model"` | 0 |
| forum `q="decision model"` | 1 topic: IC-Imajev |
| `AppitStudio/awesome-jev` README (2,909 lines, ★112) grep "canister / dfinity" | 0 |
| GitHub `q=jev+decision` | 3,110 repos; none IC-related surfaced |
| X/Twitter | not fetchable (JS-rendered) `[unverified]` |

The **name** is absent. The **contract** is not:

1. **onicai `run_decision`.** Since v0.20.0, `onicai/llama_cpp_canister` exposes
   `run_decision` / `run_decision_full`; its README states the format "comes from TypeSafe's Jev
   model and its System One API" and that `output_tokens: 0`. Models: **Julia-1 (144M), Laya
   (421M), Kev-0.8B** `[verified]`.
2. **IC-Imajev.** Forum topic 76058 (2026-10-11), `humandebri/ic-imajev` (Apache-2.0), canister
   `xis3j-paaaa-aaaai-axumq-cai` live on mainnet: Qwen3.5-4B + rank-64 LoRA + a 256×2560 readout
   (255 choices + unknown), a 4.70 GB weight pack in stable memory, all 32 layers executed inside
   the canister. Measured update path: 84 tokens = 121.67B instructions / 27.32 s; 512 tokens =
   1,064.46B / 257.52 s; peak heap 4.2 GB (87 MiB headroom). The model is published as
   `mohit67890/imajev-4b` (★426, Apache-2.0, tagged `jev`, README "Jev's contract, now with
   images"). The numbers are local measurements `[claimed]`; the canister exists `[verified]`.

Ecosystem attention is near zero (forum 113 views, repo ★0): this is capability, not a trend.

The IC retrieval landscape is thin in the same way. Kinic advertises an on-chain vector database
and has pivoted toward an "AI memory layer" `[claimed]`; `supaIC/Cipher-AI-Vault` and
`supaIC/ic-vectordb-graph-template` are proofs of concept explicitly not for production;
Blueband could not be verified. **No public graph-walk retrieval canister was found** — which is
the gap where Gleaph's cheap query-call traversal would sit. Caffeine Inference (early access) is
the nearest IC-orbit analog to a routing layer ($0.2/M input, $0.5/M output) and lists "smart
routing" as *announced, not shipped* `[verified]`.

### 5.4 Cost and latency envelope

1 T cycles = 1 XDR = $1.366430 (docs, 2026-05-22) `[verified]`, so 1B cycles ≈ $0.00137.

| Path | Cycles | ≈USD per decision | Latency | Trust / limits |
| --- | --- | --- | --- | --- |
| Deterministic plan (query) | 0 | $0 | 0.167 s median query | query is single-node and unauthenticated unless certified variables are used |
| Tiny classifier in a query | 0 | $0 | ~100 ms | 5B instruction cap ≈192 tokens |
| Tiny classifier in an update | 0.88B | $0.0012 | ~1 s floor | consensus |
| LLM canister (free models) | 0 (100B attached, refunded) | $0 | unpublished (worker queue) | trust DFINITY workers; 1000 output tokens |
| HTTPS outcall → JEV | ~41M | ~$0.0001 + external | 0.5–2 s | non-replicated for rate-limited APIs → single-node trust; key inside the canister; 2 MB response |
| On-chain decision model, Julia-1 144M | 14.9B | $0.020 | seconds | ~210 tokens/update |
| On-chain decision model, Laya 421M | 34.5B | $0.047 | seconds | 33 tokens/update |
| On-chain decision model, Kev-0.8B | 21–28B | $0.029–0.038 | 2.6–3.3 s | ~1.2 GB heap |
| On-chain 4B (IC-Imajev) | 121.67B | $0.166 (84 tok) / $1.45 (512 tok) | 27–258 s | 4.2 GB heap, 4.7 GB weights |

At 1M questions per month: deterministic path $0; JEV outcall ≈$60–100; on-chain small decision
model $20k–47k; 4B $166k+. That ratio is the architectural fact: **models belong at the edges of
the query path, never in its hot loop.**

## 6. Recommendation

This is a proposal for the next ADR, not a decision. Phases are ordered by
`quality gain ÷ (implementation cost × instruction cost)` and by how little trust they move.

### P0 — engine: deterministic fusion over bounded streams

Add one plan-level operation that fuses two already-authorized ranked streams (text, vector) with
RRF. Proposed contract, to be pinned by an ADR:

1. Both branches execute with their authorization predicates lowered **first**; fusion happens
   after plan-level filtering, never on raw index output.
2. `sourceK` is a literal per branch (bounded, consistent with existing gates); correlated or
   parameterized values fail closed.
3. Fusion is integer fixed-point RRF (`floor(2^20 / (k + rank))`), weights optional, `k` configured.
4. Deterministic order `(fused DESC, element key ASC)`; duplicates preserved.
5. Both branches are required; a missing/not-ready TEXT or VECTOR index is an error, never a
   silent single-source run.
6. No new index kind and no new canister; the merge lives where the Router already merges shard
   results.

A cheap spike precedes it: vector top-K → existing `search_candidates(query, keys)` → RRF over the
vector ranks and the text ranks. That is asymmetric (text only ranks vector candidates) but it
reuses both implemented endpoints and would tell us whether fusion moves recall on real demo data
before we commit to a symmetric operator.

### P1 — NL surface: prepared-query routing, client-side first

- Ship the NL layer **outside the canisters** first: a CLI/SDK helper (`gleaph ask`-shaped) or a
  demo-app service that turns a question into `(prepared query name, typed parameters)` and calls
  the existing prepared-query path with the caller's identity.
- Catalog metadata lives in the NL layer: one-line "use when" per query, slot types, enum values.
  Nothing added to `gleaph-gql`/`gleaph-gql-planner`.
- Slots: enums, or entity IDs resolved through `SEARCH`/`text_score`; never free-form values.
- Confidence gate with an explicit clarification state; no silent best-guess execution.
- Read-only by construction (only prepared read queries are offered).
- Audit log: question, chosen query, slots, confidence, result row count.

Placement options, in order of preference for Phase 1:

| Placement | Sovereignty | Latency | Cost / limits |
| --- | --- | --- | --- |
| Self-hosted JEV-contract model off-IC (operator-run service) | question stays with the operator | ~100 ms–seconds | best fit for the sovereignty claim; quality at 150–400M parameters unmeasured |
| IC-native app canister + `ic-llm` | on ICP, but trust DFINITY workers | seconds (queue) | free during rollout; 1000 output tokens; 100B-cycle hold |
| Hosted JEV API via HTTPS outcall | question leaves the boundary; single-node trust | 0.5–2 s | ~$0.0001/decision; contradicts the positioning |
| On-chain decision model in a canister | perfect | 2.6–44 s | $0.02–0.54/decision (200–7,000× the hosted API); not for the query path (§5.4) |

### P2 — evidence subgraph as a prepared-query pattern

Formalize "seed and expand" as documented prepared queries: a retrieval seed (`SEARCH` or
`text_score`) → bounded traversal → evidence subgraph with provenance (element ids, path
identities, scores). This is the GraphRAG retrieval half that the demo contract already describes,
and it is where the graph engine's differentiation shows with almost no engine work. It also gives
the future generation step a bounded, authorized context.

### P3 — generation, developer-only and read-only

Only if P1/P2 measurements show a need: allow a drafted GQL string on the developer surface,
behind `parse → schema validation → write/DDL rejection → plan dry-run → one repair retry`, with
server-enforced `LIMIT`/candidate/instruction caps and an audit log. Never expose it to anonymous
browser callers; never give a model a general-purpose query tool.

### What each phase does *not* claim

- P0 does not claim graph improves relevance; it claims fusion is cheap and deterministic.
- P1 does not claim the engine understands language; it claims the layer can pick a query the
  caller is already allowed to run.
- P2 does not claim GraphRAG quality; it claims authorized, explainable context.
- P3 does not claim NL→GQL accuracy; it claims a guarded developer tool.

## 7. Explicitly not building

1. A new "hybrid index" kind or a new canister for fusion (Neo4j's own lesson: it is a retrieval
   pattern).
2. On-canister rerankers, ColBERT-style late interaction or SLM rerank (no GPU, non-deterministic,
   candidate-count × forward).
3. LLM-built graphs, community summaries, RAPTOR-style summary trees: Gleaph's graph is authored
   data; the expensive half of GraphRAG does not apply.
4. A learned query router inside the engine: in GQL the query declares its own path.
5. Free-form NL→GQL for untrusted callers (§4.1).
6. Score normalization or convex-combination fusion before `text_score` is true BM25.
7. Any NL-layer authorization logic: duplicating plan authorization is the drift AGENTS.md warns
   about.

## 8. Measurements required before committing

1. Fusion gain on our own data: recall@k / nDCG over the knowledge demo graph, text-only vs
   vector-only vs RRF (no public benchmark covers this).
2. Text `search_candidates` cost at realistic candidate counts (baseline already: 2.79M at 256 keys).
3. Router-side merge cost for two bounded streams per shard, and the effect of `sourceK`.
4. Template-routing accuracy and parameter accuracy on a 150–300 utterance golden set
   (Japanese and English, three abstraction levels), scored with **execution accuracy on result
   sets**, plus abstain rate and misroute severity.
5. LLM-canister latency/quality for slot extraction, if the IC-native placement is chosen, including
   the 100B-cycle hold and the 1000-token output cap.
6. Confidence calibration of the chosen router in Japanese — the published numbers are English.

## 9. Open questions

- Does the authoring hypothesis hold — that Gleaph's graph is curated enough that traversal
  constraints, rather than graph-derived scores, are the whole benefit? If so, "hybrid" is
  two-source fusion plus constraints, not three-source fusion.
- Which prepared-query catalog shape survives growth: one catalog with graph/role partitioning, or
  routing as a two-level decision (graph/role, then query)?
- Is there a defensible on-chain router model at 144M–0.8B that reaches useful accuracy on a
  bounded option set? onicai already ships such models, but nobody has published routing accuracy
  for them; measuring it would be a genuine sovereignty story.
- How should `text_score` score semantics change once BM25 lands, and does that reopen CC fusion?
- Should the NL layer be a CLI/SDK feature or a demo-app feature first? The demo (knowledge graph)
  has fixed scenarios and is the cheapest place to build a golden set.

## 10. Inconsistencies found while reading

Documentation-vs-code (per AGENTS.md, reported rather than silently resolved):

1. `README.md` says "One query plan answers graph traversal, full-text search, and vector search",
   while `design/demo/social-graph-rag.md` says full-text and native hybrid text/vector ranking
   are not implemented. The engine shape is unified; the fusion contract is not. The README
   sentence should either be qualified or backed by P0.
2. `README.md` describes vector search as "SPANN-inspired partitioned index with quantized
   centroids", while `design/index/vector-index.md` marks the vector layer **Partially
   implemented** and keeps PQ/HNSW deferred; the demo's vector scenario runs exact L2² ordering.
   The implementation status line in the README should match the design status.
3. The text index's 2026-08-23 investigation record reports its ranked arm as "bm25 rank", while
   the shipped v1 driver scores `weight × tf` and the fixed-point BM25 formula is deferred until
   the catalog owns per-term weights (`design/index/text-index.md`). The measurement is real; the
   label anticipates the contract. External text that cites "BM25" from that benchmark should say
   "BM25-style scoring (BM25 pending)".

External documentation conflict (affects planning, not this repo):

4. `docs.internetcomputer.org` says LLM-canister inference runs on ICP nodes and is deterministic
   via the random beacon, pins `ic-llm` at 1.1.0, and lists only Llama 3.1 8B. The crate is at
   **2.0.0** (2026-10-07) with six models, four free and two paid; the Candid interface exposes
   `backend_config { worker | ollama | openrouter }`; and `github.com/dfinity/llm` describes a
   queue polled by stateless AI workers, warns that confidentiality is not guaranteed, and states
   a 10 KiB prompt limit. Treat the docs page as a summary and the repository as the operational
   truth until DFINITY reconciles them, and plan for the gateway model (trust DFINITY) rather than
   for replicated on-chain inference.

## 11. References

Primary sources read for this record (accessed 2026-11-09 unless noted):

- Neo4j `SEARCH` clause (introduced 2026.01; full-text from 2026.09):
  https://neo4j.com/docs/cypher-manual/current/clauses/search/
- Neo4j hybrid search (WRRF) guide: https://neo4j.com/developer/genai-ecosystem/hybrid-search/
- Elasticsearch RRF: https://www.elastic.co/docs/reference/elasticsearch/rest-apis/reciprocal-rank-fusion
- Weaviate hybrid search: https://docs.weaviate.io/weaviate/concepts/search/hybrid-search
- Qdrant hybrid queries: https://qdrant.tech/documentation/search/hybrid-queries/
- pgvector (client-side fusion): https://github.com/pgvector/pgvector
- Bruch et al., fusion functions (TOIS 2023): https://arxiv.org/abs/2210.11934
- CypherBench (ACL 2025): https://aclanthology.org/2025.acl-long.438/
- CypherTurn (EMNLP 2026): https://arxiv.org/abs/2609.36987
- GQLBench (ACL 2026): https://aclanthology.org/2026.acl-long.1476/
- Text2GraphQuery-Bench (KDD 2027): https://arxiv.org/abs/2602.11745
- LAST-CQ (EMNLP 2026 REALM): https://arxiv.org/abs/2609.12746
- SynCode (grammar-constrained decoding): https://arxiv.org/abs/2403.01632
- `neo4j-guard` (read-only generated-query guard, 2026-10-28): https://pypi.org/project/neo4j-guard/
- TypeSafe docs (system-one, primitives, models, rerank cookbook):
  https://docs.typesafe.ai/llms.txt , https://docs.typesafe.ai/models ,
  https://docs.typesafe.ai/cookbooks/rerank_typesafe
- Independent JEV write-up: https://jevmigration.com/en/blog/what-is-jev/
- Japanese JEV explainer: https://note.com/humble_jacana238/n/ne8c28562efdb
- JEV graph navigation: https://github.com/jexp/neo4jev ,
  https://systemonemodels.org/examples/projects/jexp-neo4jev/
- GraphRAG with Jev (TDS, 2026-09-27):
  https://towardsdatascience.com/graphrag-with-typesafe-jev-a-system-one-approach-to-scalable-knowledge-graphs/
- JEV-style open model card: https://huggingface.co/autotrust/JEV-27B
- ICP AI inference docs: https://docs.internetcomputer.org/guides/backends/ai-inference/
- IC resource limits: https://docs.internetcomputer.org/references/resource-limits/
- IC cycle costs: https://docs.internetcomputer.org/references/cycle-costs/
- IC HTTPS outcalls: https://docs.internetcomputer.org/guides/backends/https-outcalls/
- LLM canister repository: https://github.com/dfinity/llm
- Canister llama.cpp runtime with the JEV/System One decision contract (`run_decision`):
  https://github.com/onicai/llama_cpp_canister
- IC-Imajev (4B decision model in a canister):
  https://forum.dfinity.org/t/ic-imajev-running-a-4b-decision-model-in-a-canister/76058 ,
  https://github.com/humandebri/ic-imajev , https://huggingface.co/mohit67890/imajev-4b
- Caffeine Inference: https://caffeine.ai/inference
- On-chain inference under instruction budgets (2026):
  https://zenodo.org/records/20607598 ,
  https://github.com/Simlowker/instruction-bounded-inference-artifact
- ISO/IEC 39075:2024: https://www.iso.org/standard/76120.html ; Cor 1:2026:
  https://www.iso.org/standard/93701.html
- 2025–2026 graph-vs-RAG negative results: https://arxiv.org/abs/2506.05690 ,
  https://arxiv.org/abs/2602.03578 ; HippoRAG 2: https://arxiv.org/abs/2502.14802

Repository sources:

- `README.md`; `design/index/text-index.md`; `design/index/vector-index.md`;
  `design/gql/extension-syntax.md`; `design/security/rbac-and-prepared.md`;
  `design/demo/knowledge-graph-demo.md`; `design/demo/social-graph-rag.md`;
  `design/adr/0075-conditional-policies-constant-pushdown.md`;
  `design/adr/0078-authz-aware-vector-search.md`;
  `design/adr/0095-candidate-scoped-nonleading-text-score.md`.
