# Hybrid feature + neural decision routing, and execution beyond prepared queries (2026-11-16)

Question: Glev's general router (ADR 0114) is close to rule-based and generalizes poorly. Can it be
combined with a lightweight on-chain neural model so that, given a graph type, the caller's grants
and a question, it decides well enough to (a) route more robustly, (b) optionally execute queries
that are not prepared, and (c) grow into DecisionRAG-style navigation?

Status: investigation record (append-only), 2026-11-16. No code, test, model or contract was changed
and nothing was measured for this record; every number is quoted from an earlier record or derived
from one. It decides nothing: an ADR is required before any slice below is built.

Anchor timestamp: 2026-11-16 00:56 UTC.

Markers: `[measured]` = quoted from a dated record in this repository; `[repo]` = read in the code or
a current contract; `[derived]` = arithmetic or reasoning from the previous two; `[proposal]` = not
built, not measured.

## Summary

1. The direction holds, with two changes to the proposal as stated: **grants select the candidate
   set and are not model input**, and **execution beyond prepared queries is a selection over typed
   candidates, not GQL generation**.
2. A question-only neural encoder already fits the query budget with a wide margin (136.13M
   instructions against 5B) `[measured]`. What does not fit is reading candidate text at request
   time: cost scales with tokens times layers.
3. The cheapest slice that addresses the measured weakness (lexical gaps patched by hand-authored
   cue lexicons) is one scorer, not two routers: precomputed card embeddings, one question encoding
   per request, and the similarity as additional features of the existing conditional logit.
4. The blocking item is evaluation data, not the model: the clean holdouts are 12 and 22 questions
   labelled by one developer, and no gate measures held-out graph types.

## 1. Where the routing stack stands

- The fixed six-intent INT8 neural pack was retired on 2026-11-12; the inference canister now runs
  the frozen `glev-general-router` artifact: a 29-feature conditional-logit reranker over operation
  cards plus a deterministic typed slot extractor (ADR 0114, ADR 0116) `[repo]`.
- The pack was retired because intent identity was positional (`[bos] + question + one <mask> per
  route option`) and the student read no option text, so each new intent needed retraining
  (`2026-11-12-per-graph-routing-customization.md` §1.1) `[measured]`. It was not retired for
  accuracy.
- The Router builds `GraphRoutingContext` per request from the graph type and only the prepared
  operations the caller may execute, and re-authorizes on execution (ADR 0114 decisions 2–3,
  `authorize_prepared_execute` in `crates/router/src/rbac.rs`) `[repo]`.
- Ad hoc GQL has its own gate and plan-time grant enforcement (`authorize_adhoc_gql`,
  `design/security/rbac-and-prepared.md`) `[repo]`.

### 1.1 The measured weakness is lexical

On the MovieLens real-query gate, bare genre tokens and plot-theme phrases "produced zero feature
overlap, so the model tied at zero or abstained"; the fix each time was a rule or cue-lexicon
addition (English decades, bare genres, `set in`, `with <Name>`), and the cue lexicon reached
version 7 (`2026-11-12-movie-real-query-gate.md`) `[measured]`. The first untouched holdout scored
16/22; the later clean holdout scored 12/12 but contains one complete item `[measured]`. This is the
concrete form of "close to rule-based": coverage grows by editing lexicons against observed
failures.

## 2. What on-chain inference costs

All figures are canbench on wasm32 `+simd128`, INT8, from
`2026-11-10-canister-inference-optimization-research.md` and
`2026-11-10-model20-canister-inference-feasibility.md` `[measured]`.

| Shape | Input | Instructions | Pack | Accuracy (dev / hand / blind) |
| --- | --- | ---: | ---: | --- |
| 14 layers, 1-layer head, options inline | ~136 tokens | 16.02B | 278.6 MB | not reported / 93.4 / 93.3 |
| 14 layers, compact prompt | 56 tokens | 5.94B | — | all floors pass |
| 6 layers, compact prompt | 56 tokens | 3.10B | — | gates pass |
| 1 layer, hidden 768, ffn 768, question only | 16 question tokens | 136.13M | 11.6 MB | 97.0 / 89.6 / 90.0 |
| 1 layer, hidden 640, question only | 16 question tokens | 105.84M | 9.0 MB | 88.0 / 91.5 / 90.0 |
| 1 layer, hidden 512, question only | 16 question tokens | 79.17M | 6.7 MB | 89.0 / 87.7 / 88.3 (seed-sensitive) |

Budgets: 5B instructions per query, 40B per update `[repo]`.

Three consequences:

- **A question encoder is nearly free.** The hidden-768 one-layer model uses about 3% of the query
  budget and is the only width where the Python and Rust INT8 results agree across seeds `[measured]`.
- **Candidate text at request time is what costs.** One hidden-768 layer measured 1.32B at 141 tokens,
  of which attention was 226M `[measured]`. Scaling the linear part by length and attention by length
  squared gives roughly 19B for one layer at 1,000 tokens (about 50 cards of 20 tokens); the later
  tiled kernels (16.02B for 14 layers plus head) lower that to about 15B `[derived]`. One layer of
  shared-context scoring over a 50-card catalog is therefore outside the query budget and inside one
  update call.
- **Generation is out.** The reference llama.cpp canister generated 12 tokens per update call at
  0.5B parameters (`2026-11-10-onchain-inference-comparison.md`) `[measured]`. A model that emits GQL
  text token by token is not a request-path option.

`2026-11-09-gleaph-decision-model-research.md` §4.6 records "Canister: not viable for this model
class". That estimate assumed 0.1B parameters over 8k tokens and predates the measurements above; it
still holds for that class and does not hold for a question-only encoder.

## 3. Assessment of the proposal

### 3.1 Graph type and question as input: yes

Schema conditioning is what ADR 0114 already does with features, and it is what made per-graph
catalogs possible without retraining. A neural component must keep that property: it has to score
against whatever cards the context holds, never against trained intent positions.

### 3.2 "A model trained on GQL and the Gleaph extensions": put that knowledge in the lowering

A one-to-six-layer encoder does not hold a query language. What it can learn is the mapping from a
question to schema elements: which label anchors it, which edges it follows, which property it
constrains, whether it wants text or vector search. GQL grammar and the `SEARCH`/`text_score`
surface are exact rules with an existing owner (parser, planner, `design/gql/extension-syntax.md`);
encoding them a second time in weights would be a duplicated rule that drifts. The training target is
therefore `(question, visible graph type, candidates) → selection`, and the measured lever is
in-domain data rather than size: DecisionRAG's judges went from 20.5 R@1 (1.7B, general) to 63.1
(8B, in-domain) (`2026-11-09-gleaph-decision-model-research.md` §1.1) `[measured]`.

### 3.3 Grants as model input: no — grants filter the candidates

- The context already contains only caller-executable operations, so the model cannot select a
  forbidden one, and execution re-authorizes `[repo]`.
- Model output is untrusted by contract (decision-model record §3.2). A model that reasons about
  grants moves part of the authorization decision into weights and makes the scorer an existence
  oracle for what the caller cannot see.
- Row-level grant predicates are enforced at plan time (ADR 0075); a model cannot express them and
  does not need to.

The rule that follows: the Router hands the scorer the **caller-visible** graph type and candidate
set, and every selected plan passes the ordinary authorization gate again. This keeps one owner for
authorization and is the same shape the prepared route has today `[proposal]`.

### 3.4 Execution beyond prepared queries: typed candidates, deterministic lowering

Since generation is out (§2), an unprepared query has to be assembled from decisions:

1. Enumerate candidates deterministically from the caller-visible graph type and index catalog:
   anchor label, edge steps, property predicates, index use (text, vector, property), projection.
2. Score them with the same scorer as the prepared cards.
3. Assemble the selections into a bounded typed form and lower it to GQL with deterministic code.
4. Run it through the ad hoc path, where `authorize_adhoc_gql` and plan-time grant enforcement apply
   as for any user-authored query.

This is T2 "catalog entries and predicates" in the decision-model record (§2.3), restricted the same
way: no vector-partition navigation, and a model-chosen branch is a hint marked as truncated, never
a top-k guarantee. Reasonable initial limits are read-only, a fixed set of query shapes, and a
per-graph enable bit beside `inference_enabled` `[proposal]`. Slot values remain surface values
validated against the parameter schema (ADR 0114 decision 6, ADR 0117 value domains).

### 3.5 DecisionRAG proper

DecisionRAG's gain has two parts: joint scoring of candidates in one shared context (up to +7.7
points) and navigation instead of reranking (+7.0 to +7.7 Hit@1) `[measured, decision-model record]`.
By §2, faithful shared-context scoring over a 50-card level is an update-path operation. That
matches the record's own constraint that branch selection is planning (an update) and execution
stays a deterministic query (§3.2 item 3). Frontier judging over graph expansions is the last slice,
not the first.

## 4. Composition

| Slice | What is added | Request-path cost | State |
| --- | --- | --- | --- |
| S1 | Card embeddings computed when the catalog or graph type is written; one question encoding per request; similarity added to the existing feature vector | about 136M per request (the measured question-only shape); card encoding on the update path | `[proposal]` |
| S2 | Typed candidates from the graph type, scored by the S1 scorer, lowered to GQL (§3.4) | S1 plus enumeration and lowering | `[proposal]` |
| S3 | Frontier judging over graph expansions with shared-context scoring | update path | `[proposal]` |

Design constraints for S1:

- **One scorer.** ADR 0114 removed the dual routing path on purpose. The neural signal enters as
  features of the conditional logit; the feature reranker and the slot extractor stay as the
  auditable floor, and abstain stays one calibrated decision.
- **Derived state has an owner.** Card embeddings are derived from catalog metadata. They need the
  model version in their key and must be invalidated by the catalog write that changes the card;
  otherwise they are a second copy of the catalog. ADR 0114 left context caching by fingerprint as an
  open question, and this slice forces that decision.
- **Wire and artifact.** The context limits are 64 operations, 32 KiB context, 16 KiB text, and the
  artifact is embedded in the wasm with no uploaded asset (ADR 0114 decisions 4–5, ADR 0116) `[repo]`.
  An 11.6 MB pack brings back an uploaded artifact and a tokenizer, both of which were removed on
  2026-11-12; ADR 0114 decision 4 already names the follow-up ADR for that.
- **Joint scoring at query cost** is an open design: a single cross-attention layer in which the
  question tokens attend over the stored card vectors would give listwise scoring over about 80
  positions instead of 1,000 tokens. Nothing measures whether it recovers the shared-context gain
  `[proposal]`.

## 5. What blocks this, and when to stop

1. **Evaluation size.** The clean movie holdouts are 22 and 12 questions with single-annotator
   labels `[measured]`. A neural feature cannot be accepted or rejected on that.
2. **Generalization must be measured on held-out graph types.** Accuracy on schemas seen in training
   is indistinguishable from the retired fixed-intent model.
3. **Description-driven scoring has one negative measurement here.** With a frozen 421M decision
   model, hand-tuned catalog descriptions lifted dev top-1 from 72.5% to 82.5% and dropped test from
   75.9% to 68.8% (`demo/knowledge/eval/routing/README.md`) `[measured]`. That was a zero-shot
   general model and tuned descriptions, not an in-domain encoder, so it does not predict S1; it does
   show that card wording can be overfitted and must be selected on held-out data.
4. **INT8 fragility.** Hidden 512 swung between 64.5% and 89.0% dev across seeds in the Rust INT8
   forward; hidden 768 is the stable width `[measured]`.
5. **Japanese.** No public Japanese routing benchmark exists and test items must be authored
   (decision-model record §7).

Suggested stop rule for S1: build the held-out-schema gate and enlarge the real-query holdout first;
if the neural feature does not beat the cue-lexicon model there, do not ship a pack `[proposal]`.

## 6. Open questions

- Is the question encoder trained from the existing teacher soft labels, or does it need new
  schema-conditioned data generated across synthetic graph types?
- Can the client-side embedding provider (ADR 0120/0122) stand in for an on-chain question encoder?
  Not examined in this record.
- Which typed query shapes does S2 admit first, and where does the typed form live so the planner
  crates stay general-purpose GQL?
- Does S2 need its own abstain calibration, given that abstention is catalog-size sensitive
  (ADR 0114 context)?

## 7. Sources

Repository records: `2026-11-09-gleaph-decision-model-research.md`,
`2026-11-10-canister-inference-optimization-research.md`,
`2026-11-10-model20-canister-inference-feasibility.md`,
`2026-11-10-onchain-inference-comparison.md`,
`2026-11-12-general-routing-neural-retirement.md`,
`2026-11-12-per-graph-routing-customization.md`, `2026-11-12-movie-real-query-gate.md`;
ADRs 0075, 0110, 0114, 0116, 0117, 0118; `design/security/rbac-and-prepared.md`;
`demo/knowledge/eval/routing/README.md`; `crates/router/src/rbac.rs`;
`crates/gleaph-routing-wire/src/lib.rs`.

No external source was fetched for this record.
