# Toward a Gleaph-specific decision model: DecisionRAG, Latent Sparse Attention, and what to build first

Last updated: 2026-11-09
Status: **research record** (append-only). Companion to
[2026-11-09-natural-language-query-research.md](2026-11-09-natural-language-query-research.md). It
trains no model and changes no contract.

Markers: `[verified]` = primary source read and says this; `[claimed]` = vendor/blog/author claim or
secondary report; `[unverified]` = no primary source reached.

## Purpose

Two 2026 papers were pointed at as examples for a model built for Gleaph rather than a hosted one:
*Retriever-Free Retrieval: Navigating Documents with Decision Models* (DecisionRAG,
arXiv 2610.35868v1) and *Latent Sparse Attention* (LSA, arXiv 2610.42147v2). This record answers:

1. What do these two papers actually establish, and what is reusable?
2. What should a "Gleaph-specific model" do, given that Gleaph is a graph engine with a schema,
   not a document corpus?
3. What has to exist in the engine before a model is worth training, and what research lines must be
   surveyed first?
4. Build, buy, or defer — and under which triggers?

## Summary

1. **Both papers implement one architecture**: a hierarchy, textual descriptions of candidates, a
   *non-generative* scorer that judges many candidates in one shared context, and beam/top-k
   pruning. DecisionRAG applies it to corpus retrieval; LSA applies the same shape *inside
   attention* (cluster keys, score centroids, tree beam search). The convergence is the strongest
   2026 signal for what to build.
2. **The decision-model retriever wins on quality and cost at once.** Without task-specific
   training, DecisionRAG beats dense retrieval + an 8B reranker by 13.8 R@1 on MultiDoc2Dial and
   17.4 Hit@1 on Natural Questions, at 0.68–1.76 s and 0.22–2.33¢ per query; generation-based
   navigation over the same hierarchy is 34–145× slower and 21–25× more expensive for no consistent
   accuracy gain `[verified]`.
3. **Two mechanisms carry the gain**: judging candidates *jointly in a shared context* (up to +7.7
   points over independent judgments) and *navigating* rather than reranking (+7.0–7.7 Hit@1 over
   reranking a dense top-100) `[verified]`.
4. **In-domain data dominates model size at this scale.** The paper's open-weight judges score 20.5
   R@1 at 1.7B (general training), 40.4 at 8B general, and 63.1 at 8B fine-tuned in domain
   (Appendix G, Table 17) `[verified]`. A domain-tuned *small* scorer is therefore the plausible
   target for Gleaph — not a general retriever.
5. **Gleaph's structure is not a document tree.** Labels and edges are authored and semantic;
   vector partitions are not yet semantic (representative points and closure are planned, not
   implemented). The faithful analogue is therefore a **frontier search over graph expansions**
   (neo4jev-style) with **shared-context joint scoring** (DecisionRAG-style) — not a synthetic topic
   tree.
6. **Build order matters more than the model.** The engine-owned, deterministic pieces — candidate
   *descriptions* derived from schema/catalog, a *frontier enumeration* with budgets and provenance,
   and a *scorer adapter* contract — are required by every model variant (hosted or in-house) and
   can be built and measured without training anything. Train only after shadow-mode data exists.
7. **LSA is not on the critical path.** It buys 22.7–72.4× prefill and 4.6–42.5× decode at 10M
   tokens, on H200/B300 kernels, with "similar" accuracy — none of which matters for a scorer that
   reads 1k–8k tokens of candidate descriptions on CPU. Revisit only if the candidate context grows
   by orders of magnitude.

## 1. The anchor papers

### 1.1 DecisionRAG — retrieval as navigation with decision models

arXiv 2610.35868v1, 2026-10-26, Cornell + ASAPP (Wan, Jünger, Du, Liu, Krishnan, Weinberger).

**Method.** A corpus is organised as a tree (topics → documents → passages, or clusters → pages →
paragraphs). At each level the decision model receives one context containing the query and the
numbered candidate descriptions, plus one yes/no question per candidate ("Is the information needed
to answer the user's question on Page n?"). All questions are evaluated in one forward pass with no
token generation. The top-k candidates are kept, their children become the next level, and pruned
branches are never expanded. A final multiple-choice judgment over the top candidates reorders the
shortlist. Query-vector-free: no first-stage retriever and no precomputed index are required, and
document retrieval needs no task-specific training `[verified]`.

**Two judgment modes.** Parallel binary judgments are independent (probabilities not normalised
across questions, no option cap, split across requests when a level is large); multiple-choice
judgments normalise across options (cap 255 for Jev) and are used only on the final shortlist.
Scores from different requests are *not* comparable, which is why the paper rescores the pooled top
candidates in one request before ranking (MTRAG: top 50 pages rescored together) `[verified]`.

**Results (no task-specific training)** `[verified]`:

| Benchmark | DecisionRAG (Jev) | Best conventional baseline |
| --- | --- | --- |
| MultiDoc2Dial R@1 | 47.4 | 33.6 (dense + Qwen3-Reranker-8B) |
| ConditionalQA Hit@1 | 69.7 | 73.4 (dense + GPT-5 listwise) |
| MTRAG R@5 | 34.2 | 32.5 (dense + Qwen3-Reranker-8B) |
| Natural Questions Hit@1 | 65.4 | 60.8 (dense + GPT-5), 48.0 (dense + 8B reranker) |

Adding dense candidates to the final stage improves R@10 (83.6 → 84.7 on MultiDoc2Dial, 71.9 → 76.3
on ConditionalQA, 86.7 → 88.9 on NQ), i.e. navigation and vector retrieval recover complementary
evidence `[verified]`.

**Ablations that matter for design** `[verified]`:

- Shared context vs independent scoring: +7.7 R@1 on MultiDoc2Dial, +6.5 Hit@1 on NQ; splitting a
  level into separate requests costs top-1 but leaves R@10, so pooled scores suffice for screening.
- Shared-context benefit levels off at ~50 candidates per request.
- Multiple-choice reranking of the top 20: +1.2 R@1 (MultiDoc2Dial), +4.6 Hit@1 (ConditionalQA) for
  0.12–0.15 s and 4–33% more cost.
- Navigation vs reranking the dense top-100: +7.7 Hit@1 (ConditionalQA), +7.0 Hit@1 (NQ).
- Budget: keeping 15% of the top-level clusters matches flat scoring (86.2 vs 84.5 R@10) at a fifth
  of the cost; below 5% recall collapses (63.7 R@10 at 1%).
- Decisions vs generation: GPT-5 generating the same navigation decisions takes 250.8 s and 57.2¢
  on NQ versus 1.73 s and 2.33¢, with comparable accuracy.

**The published training recipe** (Appendix G) `[verified]` — this is the part to reuse if a model is
trained:

- Backbone: Qwen3-Base at 1.7B / 4B / 8B; **language-model head replaced by a scoring head**.
- Each candidate is its own question with options "no"/"yes", appended to the shared context; each
  question starts after the context and cannot attend to other questions, so candidates are judged
  only against the shared context.
- The head projects the hidden state of the question's end token and of each option's end token into
  a common space, scales cosine similarity by a learned inverse temperature (capped at 100), and
  softmaxes over the options; cross-entropy over options.
- General training mixes public multiple-choice and classification data (RACE, QuALITY, SciQ, ARC,
  CommonsenseQA, HellaSwag, MNLI, intent/topic classification, MultiWOZ, xLAM, Mind2Web).
- In-domain training data is generated by simulating the navigation stages (gold page present with
  probability 0.85, negatives from BM25, held-out monitoring split).
- LoRA r=64, α=128 on all attention/MLP projections plus a fully trained head, 1 epoch over a sample
  of in-domain data, rows of 65,536 tokens.
- Results: 1.7B general 20.5 R@1; 4B general 28.5; 8B general 40.4; 8B in-domain 63.1; 8B
  general→in-domain 65.8 (Table 17).

**Limitations** `[verified]`: depends on hierarchy structure and description quality; pruning is
irreversible, so evidence can be lost if an ancestor is dropped; mitigated by larger budgets and
dense candidates.

### 1.2 LSA — the same shape inside attention

arXiv 2610.42147v2, 2026-11-03, Prefrontal AI + ETH Zurich (Kubicek, Caciolai, Peng, Sumant,
Hoover, Greenlee, Battleday, Urban, Hoefler, Whittington).

**Method.** Keys are pooled and clustered (Ward's method) in a *learned* latent space into buckets
with centroids; each query scores bucket centroids (flat) or descends a bucket hierarchy with beam
search (tree), expands the selected buckets into keys, and attends only to those, plus sink and
window keys. D-LSA adds LSA to a pre-trained model by distilling only the index projections and a
per-layer temperature against the frozen layer's attention (forward KL); N-LSA trains the model
natively with next-token prediction `[verified]`.

**Results** `[verified]`: on GLM-5.3-Flash (Qwen-3.8-27B) at 10M tokens, one attention layer is
22.7× faster in prefill (72.4× versus full attention) and 4.6× faster in decode (42.5× versus full
attention), with similar accuracy; N-LSA matches full attention at 128k with 1.6× training speedup;
indexer distillation used 352M tokens versus DSA's 2.1B. Design constants: `k = 4096` long-range
keys, index dimension 64, cluster block 512.

**Limitations** `[verified]`: evaluated on GPU kernels (H200/B300); the win appears at ultra-long
context (the indexer is 98% of sparse-attention prefill time at 10M tokens); it changes the model,
not the retrieval interface.

### 1.3 The convergent pattern, and why it matters

Four independent 2026 systems now implement the same shape at four different levels:

| Level | System | Hierarchy | Non-generative scoring | Beam/top-k |
| --- | --- | --- | --- | --- |
| Corpus retrieval | DecisionRAG | topics → pages → passages | yes (parallel yes/no in shared context) | yes |
| Attention | LSA | key buckets → node buckets | yes (centroid dot products) | yes (tree beam) |
| ANN index | SPANN / IVF | partitions → buckets | yes (centroid distance + pruning) | yes |
| Graph walk | neo4jev | outgoing relationships per node | yes (Jev choice over relations) | yes (beam over log-probs) |

The reusable conclusion is not "use a decision model". It is that **the scarce asset is the
interface between the engine's structure and a cheap scorer**: stable candidate descriptions, a
bounded frontier, comparable scores, and a deterministic fallback. That interface is engine work,
and it is the same for a hosted Jev call, an `ic-llm` call, or an in-house distilled model.

## 2. What a Gleaph-specific model should do

### 2.1 Why the document-tree analogy does not carry over

DecisionRAG depends on a corpus hierarchy whose node descriptions are meaningful (titles, one-line
cluster summaries). Gleaph's equivalents are weaker today:

- **Labels and edges are authored and semantic** — the graph itself is the structure. That is
  stronger than a document tree for *constraint* navigation and weaker for *semantic* navigation.
- **Vector partitions are not yet semantic.** Representative points and closure replication are
  recorded as later slices in `design/index/vector-index.md`, so "partition centroid description"
  is not available as a navigation node today.
- **Text segments are not semantic** either; the text index is a posting-list engine, not a trec
  topic hierarchy.
- **The full-text index and the vector index are separate canisters** reached through the Router, so
  a navigation step that spans them is a Router-orchestrated sequence, not a local tree walk.

So the faithful Gleaph analogue is not "navigate a topic tree" but **search a frontier of graph
expansions**: start from seeds produced by `SEARCH`/`text_score`/a prepared query, describe the
frontier (edge type, properties, neighbour label and summary), judge all frontier candidates jointly
in one context, keep the best, expand again — and stop on a calibrated "enough evidence" question.

### 2.2 What neo4jev is missing, and what DecisionRAG supplies

`jexp/neo4jev` walks one node at a time: the current node's outgoing relationships become `choice`
options, and beam search over summed log-probabilities picks the path. Three gaps for our purpose:

1. **Scores are conditional on a local state**, so branches reached from different nodes are
   compared by accumulated log-probability, not by a common relevance scale.
2. **No shared-context comparability**: each hop's distribution sees only its own neighbours.
3. **No abstain**: the walk always picks a relation; there is no calibrated "the evidence is not
   here" signal that could fall back to a deterministic plan.

DecisionRAG's mechanism fixes exactly these: put the whole frontier in one context, ask one yes/no
question per candidate, and compare the resulting probabilities directly; add a companion yes/no
question for "is the answer already in hand?" and stop on it.

### 2.3 Three tasks, one scorer

| Task | Candidates | Question shape | Engine data needed |
| --- | --- | --- | --- |
| T1 routing | caller-visible prepared queries / templates | choice over catalog (≤255, two-level if larger) + slot questions | catalog with one-line descriptions, slot enums; caller-filtered catalog |
| T2 navigation | catalog entries (graph type, label, index), predicate buckets, and graph frontier (edge/neighbour pairs) | one yes/no per candidate + "goal reached" | schema/catalog facts, property name=value, bounded expansion |
| T3 ranking | fused top-k candidates | one yes/no per candidate, then multiple-choice on top-20 | candidate text/summary + provenance ids |

The same model and the same request wire can serve all three; only the candidate universe and the
question text change. This is the "one scorer, many universes" design: it keeps the model small and
the engine generic, and it avoids a different model per feature.

**Scope limit learned from the index structure (important).** T2 must *not* navigate vector
partitions. Partitions have no text (`design/index/vector-index.md`: centroids are numeric, and
representative points/closure are later slices), leaf scans cost ~4.0B instructions each, and
DecisionRAG's measured 10–15% retention at 10⁸ rows would imply 1.5–2.6×10¹² instructions —
three orders of magnitude over budget. The defensible split is:

- the model chooses **catalog entries and predicates** (label, index, property predicate, text
  query, frontier edges), which lower into `SEARCH ... WHERE` and property-index allowlists, i.e.
  into ordinary plans;
- **ε₂ distance pruning chooses partitions**, because it is a sound distance bound and its
  correctness is already a test (`partition_scan_eps_zero_loses_boundary_recall_that_eps_positive_recovers`);
- a model-based branch choice is a *hint*, must be marked truncated, and must never be described as
  a top-k guarantee.

## 3. What the engine must expose before any model is trained

These are deterministic, cacheable, and authorization-aware. They are useful even with a hosted
model, and they are prerequisites for an in-house one.

### 3.1 Candidate descriptions

A description is a bounded, stable string per candidate, derived from the catalog and index, not
authored by hand:

| Candidate | Description source (today) | Missing piece |
| --- | --- | --- |
| Prepared query | name, parameter surface, one-line "use when" (new catalog metadata) | catalog metadata field |
| Label / vertex type | label name, typed property names, sample values (from `graph_type_catalog`) | bounded sampler |
| Vertex | a schema-designated summary property (title/summary), else label + top property values | summary property convention |
| Edge/frontier entry | edge type, inline scalar properties, neighbour label + neighbour summary | a projection that returns this in one call |
| Text passage | the indexed text itself | chunking convention |
| Vector partition | representative points (planned) | rep points/closure not implemented |

The description layer is where "Gleaph-specific" actually lives: a generic decision model can only
judge what it is shown, and what Gleaph can show is *schema and graph structure*, not just prose.

### 3.2 Frontier enumeration

A bounded API (prepared query or internal endpoint) that, given one or more seed vertices and a
hop budget, returns the frontier as `(candidate id, edge id, neighbour id, description)` rows plus
provenance, with:
- a hard bound on frontier size (comparable to the existing 4096 candidate and 1024 LIMIT gates);
- authorization applied per level through the ordinary plan machinery (ADR 0075/0078), so a model
  never sees an unauthorized candidate;
- deterministic ordering for reproducible contexts.

Four constraints that come from the engine's own contracts:

1. **Caller-filtered candidates.** Prepared-query existence is non-disclosure (missing/unauthorized
   is `NotFound`), and label-bucket restriction (ADR 0089) is fail-closed. The candidate list handed
   to a scorer must therefore be exactly the caller's visible set; otherwise the scorer becomes a
   free existence oracle.
2. **Model output is untrusted.** Returned branch ids are validated against the pre-call snapshot;
   unknown, duplicated, out-of-range or unauthorized ids fail closed. Prompt injection from vertex
   text can then at worst degrade recall or inflate cost, never leak data.
3. **Two phases.** Branch selection happens in *planning* (an update), the chosen branch set is
   persisted as a plan, and execution stays a deterministic query. This keeps ADR 0078's "true
   top-k of the authorized subset" contract out of the model's non-determinism, and gives the audit
   log one stable record per decision.
4. **Captions are derived data.** A partition/vertex caption built from member properties is shared
   across callers, so it cannot be per-caller; sending it to an external model moves user data
   outside the trust boundary. Restrict caption generation to properties declared navigation-safe
   in the index/schema definition, or generate captions only inside the trust boundary (canister or
   an approved outcall). Predicate candidates (`property = value`) avoid the problem entirely
   because the plan authorizes them.

### 3.3 Scorer adapter contract

One request/response wire for all three tasks:

```
request  = { model, state, questions{ id -> {type: choice|noul|score, instructions, criteria} } }
response = { answers{ id -> {choice|noul|score, probabilities, confidence} }, model_version, usage }
```

The adapter is a thin client with no authorization logic; it fails closed on timeout, malformed
answers, unknown model version, or a missing answer. RRF and deterministic plan selection remain the
floor when the adapter is unavailable.

### 3.4 Why this belongs to the engine, not the NL layer

- One source of truth for descriptions: the catalog and index already own labels, properties and
  statistics. Duplicating that in a prompt builder would drift (AGENTS.md DRY rule).
- Authorization must be enforced before the model sees anything (ADR 0078 layer 3), which only the
  plan can do.
- Determinism: the same question and graph state must produce byte-identical candidate lists so that
  routing decisions and audit logs are reproducible.

## 4. Building the model, if the triggers are met

### 4.1 Architecture shortlist

| Option | Shape | CPU cost | Expected quality | Verdict |
| --- | --- | --- | --- | --- |
| 0.1–0.3B bidirectional encoder + typed scoring heads | all candidates scored in one batch (Laya/Julia-1 style) | 0.1B × 2k tokens ≈ 0.4–1 s INT8 | narrow in-domain routing is plausible; general decision index 60.4 at 421M (Laya) | first self-hosted target for routing/ranking |
| 1–4B decoder with pointer/scoring head | shared state, short per-candidate question, KV reuse (Kev/Drex/DecisionRAG) | 4B × 2k ≈ 30 s on 8 vCPU (GPU for serving) | 4B = 76.8, 9B ≈ 82 on system-one-bench; 4B in-domain is the realistic quality target | teacher / batch scorer, not hot path |
| frozen LM + option-letter logits (SemIf) | no training, read option logits | same as backbone | Qwen3.5-4B 0.813 vs Jev 0.883 on authored decisions | 2-week baseline to validate the task before training anything |
| two-stage: encoder screen → 4B rerank | cheap shortlist then high-quality multiple-choice | ~1 + one batch | DecisionRAG's binary→choice shape | use only if latency budget allows two calls |
| sparse-attention decoder (LSA family) | clustered-key attention | GPU only | no primary evidence of a win below 64k context | **do not build** |

### 4.2 One pass over many candidates, not pairwise

The efficiency argument is not subtle: a 33M cross-encoder measured 13.4 ms per pair on 8 CPU
threads, so 300 candidates pairwise is ~4 s, and FLOP-wise the joint pass is ~4.7× cheaper. Adding
typed questions is nearly free because each question costs head dimensions, not a forward pass. The
quality argument is DecisionRAG's shared-context ablation: +7.7 R@1 and +6.5 Hit@1 over independent
scoring, with the benefit levelling off around 50 candidates per request.

Consequence for the design: score the whole candidate set of a stage in one request; split only
when a stage exceeds the context budget, and then *rescore the pooled top candidates together*
before ranking (this is exactly what DecisionRAG does on MTRAG).

### 4.3 Training recipe (published recipe, adapted)

1. **Baseline first, no training.** Run SemIf-style frozen readout on 900 labelled examples. If the
   task cannot be learned at ~0.8, the task definition is the problem, not the model.
2. **Teacher.** Use an open-weight decision model (JEV-9B/27B, Clef-flash, imajev) served locally
   through a Jev-compatible endpoint (llama.cpp implements `POST /v1/systemone`, merged
   2026-10-02) `[verified]`. This keeps data in-boundary and avoids the unresolved question of
   whether hosted-provider outputs may be used as distillation targets.
3. **Student.** Qwen3.5-4B (Apache-2.0) with the LM head replaced by a cosine scoring head and LoRA
   on attention/MLP projections — the DecisionRAG Appendix-G recipe (per-candidate yes/no question
   after a shared context, questions cannot attend to each other, inverse-temperature cap 100).
4. **Calibrated losses.** Label-smoothed cross-entropy plus a proper scoring rule (Brier or squared
   Cramér distance, as Clef and Drex use), then temperature scaling on dev. Report ECE before and
   after.
5. **Option-order consistency.** Train with permuted option orders and/or add a permutation
   consistency loss (Drex: flip rate 6.3% → 4.2%) `[verified]`; at serving time score 3–4
   permutations and average. Option order alone moves ~7% of JEV-style answers `[claimed]`.
6. **Distil to the edge.** Once the 4B student is good, distil it into a 0.1–0.3B encoder with a
   typed multi-head head for CPU serving; keep the 4B as the quality reference.

### 4.4 Data

The benchmark pipelines converge on one recipe: enumerate from templates/schema → execute to prove
non-empty and non-degenerate → naturalise with an LLM → auto-validate → human-verify. Auto-validation
alone is not enough: 97.2% of auto-passing pairs were logically correct in Mind the Query, so ~2.8%
are silently wrong `[verified]`.

- **Questions come from executed provenance, not from op names or doc comments.** The demo already
  shows why: `variable-length-reach` is documented as "from a start Concept" but traverses incoming
  edges, so doc-derived labels would teach the wrong direction.
- **Sizes.** Minimum: 100 positives/op + 100 near-miss negatives/op + 100 out-of-scope = 900 train,
  200 dev, 400 test; recommended 2,000/300/600; navigation evidence pairs 600–1,000. Raw generated
  data should be 2–2.5× the target after filtering.
- **Negatives are typed**: near-op confusion (same entity, different op), direction reversal, depth
  out of range, missing slot, out-of-catalog request, compound intent (clarify), unauthorized op
  (abstain without naming it), ambiguous entity (clarify).
- **Splits** are by paraphrase family, entity, generator and catalog version — never by utterance.
  Contamination is checked with embeddings and an LLM, not n-gram overlap (paraphrases defeat
  n-gram checks). Japanese test items are authored, not machine-translated (MASSIVE precedent).
- **Blocking prerequisite.** Three of the four knowledge-demo prepared queries have literal-fixed
  slots, and the fourth's only parameter is a 768-dimensional `Value::Bytes` embedding that a router
  cannot emit. Slot learning is impossible until the catalog's slot surface is defined
  (enum/entity-class anchors, depth, team), so a 4-way op-selection MVP is the honest first slice.

### 4.5 Compute

Training a 0.5B model on 50k examples of ~1k tokens is `6·N·D ≈ 1.5×10¹⁷` FLOPs, about 0.27
GPU-hours at 40% MFU; a 4B model is 10–40 GPU-hours; single GPU with LoRA and a trained head. The
published anchors are consistent: JEV-9B took ~3 B200-hours on 640k rows, JEV-27B ~9.2, and imajev's
~1M-decision pipeline cost about $676 of GPU time `[verified]`. The bottleneck is labels and
evaluation design, not compute.

### 4.6 Placement

- **Serving**: CPU, INT8, one pass per stage; llama.cpp/ggml or ONNX Runtime; the Jev-compatible
  HTTP contract lets the same adapter talk to a hosted API, a local 4B, or a future encoder.
- **Canister**: not viable for this model class. 0.1B × 8k tokens is ~1.6 TFLOP, five times the
  best-case 40B-instruction update budget and far worse in practice; a canister-side scorer remains
  a research item, not a plan.
- **Router**: no model. If an IC-native path is required, the scorer call is an update-side
  planning step (outcall, 0.5–2 s), with the chosen plan persisted and executed as a deterministic
  query.

### 4.7 Evaluation gates

From the benchmark and routing literature, the non-negotiables are:

- headline metric is **execution accuracy on result sets** against a frozen snapshot, plus strict
  `(op, slots)` top-1; n-gram and LLM judges are diagnostics only (65.9% over-scoring, +9 points
  optimism);
- accepted sets are enumerable by hand, and empty results are never counted as correct;
- abstain is a first-class class: report out-of-scope recall, in-scope false abstain, and the
  risk–coverage curve;
- calibration: ECE/Brier before and after temperature scaling;
- option-order sensitivity: min/mean/σ over 3 permutations, flag differences above ~2 points;
- per-slice tables (op, abstraction level, language, seen/unseen entity and family, catalog version,
  near-op pairs);
- paired bootstrap 95% CI for every comparison.

Suggested gates for a 4-op MVP on a 400-item test: strict top-1 ≥95% (≥90% on unseen families),
execution accuracy ≥90% on answered items, out-of-scope recall ≥90% with ≤20% false abstain,
ECE ≤0.05, order sensitivity ≤2 points.

## 5. What the four surveys change in the plan

### 5.1 The decision-model category is established, and open weights exist

- Multiple vendors now ship typed-decision APIs: Jev 1.14 ($0.042/M input), OpenAI GPT-6 Luna
  Decisions ($0.10/M), Microsoft-Decision-1 (Qwen3.5-9B-based), Cloudflare Clef (Apache-2.0, 27B and
  9B), Perplexity pplx-decider ($0.02/M, 256k), Mercury Decide ($0.02/M), Nace Drex (open weights)
  `[verified]`.
- An independent benchmark (system-one-bench, 2026-11-08, 16 datasets / 3,250 cases) puts the best
  hosted models at 82–85, Jev 1.14 at 79.3, open 0.8B at 63.6, 4B at 76.8 and 9B around 82, with
  ECE varying from 0.7% to 20% `[verified]`. Accuracy and calibration are separate buying criteria.
- Apache-2.0 weights suitable for commercial fine-tuning exist: Clef/Clef-flash, JEV-9B/27B,
  imajev, Laya, Julia-1, Kev-0.8B; RAIL-M (Drex) needs legal review; OpenJev and Drex-DLM are
  non-commercial `[verified]`.
- **Implication**: the "build vs buy" question is not about capability but about data, sovereignty
  and maintenance. A 4B in-domain model reaching 80–88 on our narrow task is plausible; a 0.1–0.3B
  encoder is plausible only with strong in-domain data and for routing/ranking rather than open
  judgment.

### 5.2 Navigation research: what transfers is not the tree

The survey confirms the structure finding in §2:

- training-free navigation is the post-2023 mainstream (MemWalker, RAPTOR, LATTICE, DecisionRAG);
  trained tree routers (DocQN, dense hierarchical retrieval) do not transfer;
- LATTICE accepts ~40 listwise LLM calls per query and reports that navigation only pays off on
  reasoning-heavy retrieval, not ordinary IR `[verified]` — a warning against putting navigation in
  a database's hot path;
- graph walkers (ToG, neo4jev) converge to "one model call per hop" with cost linear in hops ×
  branches, and path scores that are sums of log-probabilities — comparable only with calibration
  or a shared request;
- HippoRAG's PPR is the reference for *deterministic graph expansion without a model*, i.e. the
  fallback when the scorer is unavailable;
- no published system puts a decision model inside an ANN index or as a database plan operator as of
  2026-11 `[unverified absence]`. Neo4j exposes search clauses and leaves fusion to the query
  language; GraphRAG keeps orchestration in the application.

**Implication**: Gleaph's defensible contribution is not a smarter tree. It is the *pairing* of a
deterministic, authorized plan with an optional scorer at the plan's edges — and the description
layer that makes any scorer usable.

### 5.3 Efficiency: sparse attention is out, an encoder is in

- LSA's own numbers show the regime: 2.1× at 512k, 3.5× at 1M, 22.7–72.4× at 10M, all on H200/B300
  with GPU-only kernels; below 1M the win is small, and at 1k–8k it does not exist `[verified]`.
- On CPU, the measured prefill throughput is ~5×10¹¹ FLOP/s, so a pass costs `2·N·T/5×10¹¹`:
  0.1B × 8k = 3.2 s, 1B × 8k = 32 s, 0.1B × 2k ≈ 0.8 s. Attention is only `T/(T+12d)` of that — 10%
  at 1k, 46% at 8k for d=768 — which is the entire theoretical headroom for any sparse-attention
  trick, and the indexer plus gather costs eat most of it.
- The winning shape is a dense bidirectional encoder with typed scoring heads, one pass, INT8; the
  only LSA idea worth borrowing (bucket → centroid → survivors) is already what the vector index
  does.

### 5.4 Data and evaluation are the real project

The surveys agree that the hard parts are: a parameterized catalog (so slots exist), provenance-
derived questions (so labels are true), typed negatives (so abstain is learnable), family/entity/
generator/catalog-version splits (so the test measures generalization), and an execution-based
metric with confidence intervals. None of that is model work; all of it is prerequisites.

## 6. Verdict and phased plan

**Build the interface now, the model later.** Concretely:

- **Phase 0 (weeks, no model).** Catalog metadata ("use when" lines from `///` doc comments, slot
  surface, accepted sets), the description view, the caller-filtered selection/frontier views, the
  scorer adapter contract, and shadow logging. This is useful on its own (documented prepared
  queries, deterministic routing) and is the prerequisite for every model option.
- **Phase 1 (2 weeks).** Frozen 4B readout baseline (SemIf) plus a 900-item labelled set. This
  answers whether the task is learnable and whether the catalog is clear enough. Kill the idea here
  if strict top-1 cannot beat a keyword/heuristic router by a useful margin.
- **Phase 2 (4–8 weeks).** Train a 4B LoRA + scoring head on 2–5k in-domain decisions, teacher
  labels from a self-hosted Apache-2.0 decision model, permuted-option training, temperature
  scaling. Gate on §4.7. Then distil to a 0.1–0.3B encoder for CPU serving.
- **Phase 3 (research, only if Phase 2 succeeds).** Frontier judging over graph expansions; a
  canister-side tiny model; vector-partition captions only after representative points/closure land
  and caption recall is measured against ε₂.

**Triggers to start Phase 1**: the catalog's slot surface is defined; at least one caller needs
natural language; ≥900 labelled decisions are obtainable; a hosted decision model is unacceptable
for sovereignty or cost reasons.

**Kill criteria**: SemIf baseline below ~0.6 on a clean test; teacher labels inconsistent (κ<0.6);
a 4B in-domain student below ~80 strict top-1 after two iterations.

## 7. Risks and open questions

1. **Description quality is the performance ceiling** (DecisionRAG names it as a limitation). For
   Gleaph this means catalog metadata and schema descriptions are model-critical, not documentation
   chores.
2. **Caption derivation and privacy.** Vector-partition captions are shared derived data; they
   cannot be per-caller and may not leave the trust boundary. This is why Phase 0/2 deliberately
   avoids partition navigation.
3. **Irreversible pruning has no soundness bound.** ε₂ does; a model does not. Navigation must stay
   a hint with an explicit `truncated` marker.
4. **Teacher licensing.** Using hosted decision-model outputs as distillation targets is legally
   unresolved in this survey `[unverified]`; open-weight teachers avoid the question.
5. **Catalog drift.** Every catalog change invalidates part of the test set; version test slices by
   `(op source hash, seed hash, model version)`.
6. **Japanese.** No public Japanese NL→GQL/routing benchmark was found; Japanese test items must be
   authored, and Japanese in-domain data is likely required for a small model.
7. **Maintenance vs pre-production simplicity.** A self-hosted model adds a training pipeline, a
   serving service, calibration monitoring and an evaluation set to a repository that currently has
   none. That cost must be justified by a caller requirement, not by capability.

## 8. Inconsistencies found while reading

1. **The NL task assumes a slot surface the catalog does not have.** The knowledge demo declares a
   parameter on one of four prepared queries, and that parameter is a 768-dimensional
   `Value::Bytes` embedding. "Question → (op, typed slots)" cannot be trained until the catalog is
   parameterized or the NL metadata defines the slot surface.
2. **`variable-length-reach` documentation disagrees with its implementation**: the comment says
   "from a start Concept", the query traverses incoming edges. Questions generated from comments
   would produce wrong labels.
3. **`shortest-path` returns only the endpoint concept, not the path**, so an evidence subgraph
   cannot be reconstructed from the prepared query's `RETURN`; a separate provenance query is
   needed for gold.
4. **CypherBench's number differs between the reproduction table and the paper.** The first record
   in this series cites 61.6% from a third-party table `[claimed]`; the paper's own baseline is
   gpt-4o 60.18% EX. Quote 60.2% with the paper as the source, or keep the table attribution
   explicit.
5. **Edge element ids are unstable** (physical handles invalidated by compaction, per the RBAC
   design), while `citation-reach` returns `ELEMENT_ID(e)`. Evidence gold should use
   `(label, source, destination)` tuples, not edge element ids.

## 9. References

Primary sources (accessed 2026-11-09):

- DecisionRAG: https://arxiv.org/abs/2610.35868 (2026-10-26), HTML full text read
- LSA: https://arxiv.org/abs/2610.42147 (v2 2026-11-03), HTML full text read; code
  https://github.com/Prefrontal-AI/latent-sparse-attention
- LATTICE: https://arxiv.org/abs/2510.13217 (v2 2026-05-25)
- MemWalker: https://arxiv.org/abs/2310.05029 ; RAPTOR: https://arxiv.org/abs/2401.18059
- DocQN: https://aclanthology.org/C18-1014/ ; Dense Hierarchical Retrieval:
  https://arxiv.org/abs/2110.15439
- Think-on-Graph: https://arxiv.org/abs/2307.07697 ; ToG-2: https://arxiv.org/abs/2407.10805
- neo4jev: https://github.com/jexp/neo4jev ; HippoRAG: https://arxiv.org/abs/2405.14831 ;
  HippoRAG 2: https://arxiv.org/abs/2502.14802 ; GFM-RAG: https://arxiv.org/abs/2502.01113
- SPANN: https://arxiv.org/abs/2111.08566 ; SPFresh: https://arxiv.org/abs/2410.14452 ;
  learned index: https://arxiv.org/abs/1712.01208 ; Learning to Route: https://arxiv.org/abs/1905.10987
- Decision-model vendors: https://docs.typesafe.ai/models ; https://developers.openai.com/api/docs/guides/decisions ;
  https://learn.microsoft.com/en-us/azure/foundry/foundry-models/how-to/use-foundry-models-microsoft-decision ;
  https://blog.cloudflare.com/clef-decision-models/ ; https://huggingface.co/Cloudflare/clef-flash ;
  https://docs.perplexity.ai/docs/decisions/quickstart ; https://huggingface.co/nace-ai/drex-v1.5
- Open-weight decision models: https://huggingface.co/autotrust/JEV-27B ,
  https://huggingface.co/autotrust/JEV-9B , https://huggingface.co/mohit67890/imajev-4b ,
  https://huggingface.co/convaiinnovations/laya , https://huggingface.co/ggml-org/Julia-1-GGUF ,
  https://huggingface.co/ggml-org/Kev-0.8B-GGUF , https://github.com/vinnylarouge/jevlike ,
  https://github.com/TheoLeeCJ/SemIf-OpenJev
- Independent decision benchmark: https://github.com/alilibx/system-one-bench (2026-11-08)
- DecisionRAG training recipe: DecisionRAG Appendix G (same URL as above)
- Graph-query benchmarks: CypherBench https://aclanthology.org/2025.acl-long.438/ ;
  GQLBench https://aclanthology.org/2026.acl-long.1476/ ;
  Text2GraphQuery-Bench https://arxiv.org/abs/2602.11745 ;
  Mind the Query https://aclanthology.org/2025.emnlp-industry.133/ ;
  Neo4j Text2Cypher https://arxiv.org/abs/2412.10064
- Routing/intent data: CLINC150 https://arxiv.org/abs/1909.02027 ; Banking77
  https://arxiv.org/abs/2003.04807 ; MASSIVE https://arxiv.org/abs/2204.08582 ; SetFit
  https://arxiv.org/abs/2209.11055 ; APIGen https://arxiv.org/abs/2406.18518 ; xLAM
  https://arxiv.org/abs/2409.03215
- Evaluation pitfalls: LAST-CQ https://arxiv.org/abs/2609.12746 ; rephrased-sample contamination
  https://arxiv.org/abs/2311.04850 ; dedup https://arxiv.org/abs/2107.06499 ;
  SemDeDup https://arxiv.org/abs/2303.09540 ; calibration https://arxiv.org/abs/1706.04599 ;
  selective classification https://arxiv.org/abs/1705.08500 ; LLM-judge biases
  https://arxiv.org/abs/2306.05685 ; option-order bias https://arxiv.org/abs/2309.03882
- ICP limits and inference: https://docs.internetcomputer.org/references/resource-limits/ ;
  https://docs.internetcomputer.org/guides/backends/ai-inference/ ; on-chain inference
  https://zenodo.org/records/20607598

Repository sources: `design/index/vector-index.md`, `design/index/text-index.md`,
`design/index/property-index.md`, `design/gql/extension-syntax.md`,
`design/adr/0078-authz-aware-vector-search.md`, `design/adr/0089-permission-aware-label-bucket-restriction.md`,
`design/security/rbac-and-prepared.md`, `design/demo/knowledge-graph-demo.md`,
`crates/prepared-runtime/src/lib.rs` (doc-comment retention).

