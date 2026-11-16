# 0124. A neural similarity feature for general routing

Date: 2026-11-16
Status: proposed (2026-11-16). **Nothing in this ADR is implemented.** No encoder checkpoint, pack,
wire change or canister change exists, and acceptance is conditional on the gates in
"Acceptance gates". Until then ADR 0114 and ADR 0116 are the routing contract as written.
Last revised: 2026-11-16
Anchor timestamp: 2026-11-16 01:02:27 UTC

Evidence: [2026-11-16-hybrid-neural-decision-routing.md](../investigations/2026-11-16-hybrid-neural-decision-routing.md)
(slice S1 of that record). Related: ADR [0110](0110-glev-model-repository-boundary.md),
[0111](0111-router-bound-inference-canister.md),
[0114](0114-general-schema-conditioned-routing.md),
[0116](0116-query-only-general-routing.md), [0117](0117-parameter-value-domains.md),
[0120](0120-optional-embedding-provider.md).

## Context

The general router (ADR 0114) scores operation cards with 29 hand-specified features: lexical and
description overlap, numeric/year/decade cues, schema and parameter overlap, and shape/parameter
match flags (`glev: experiments/general_routing/features.py`). A question that shares no surface
token or cue with a card contributes nothing to that card's score.

Measured on the MovieLens real-query gate (`2026-11-12-movie-real-query-gate.md`):

- bare genre tokens and plot-theme phrases "produced zero feature overlap, so the model tied at
  zero or abstained";
- each failure class was closed by a rule or lexicon edit (English decades, bare genres, `set in`,
  `with <Name>`), and the cue lexicon was at version 7 by 2026-11-13;
- the first untouched holdout scored 16/22 against a target of 85%; the later clean holdout scored
  12/12 but holds one complete item.

Separately, the on-chain inference program measured that a one-layer, hidden-768, question-only INT8
encoder costs 136.13M instructions and an 11.6 MB pack, and is the only width at which the Python
and Rust INT8 results agree across seeds
(`2026-11-10-canister-inference-optimization-research.md`). The query budget is 5B.

## Problem

Coverage grows by editing lexicons against observed failures. Each edit is a Glev release, is
English- or Japanese-specific, and is selected on the same small sets it is reported on. The scorer
has no signal for a question and a card that mean the same thing in different words.

Facts: the zero-overlap failures, the lexicon version count and the holdout scores above.
Assumption, not yet measured: a learned similarity between question and card closes that gap on
graph types the model has not seen.

## Existing architecture assessment

- **Router** owns the catalog, the per-caller `GeneralRoutingContext`, the per-graph profile
  (`GraphRuntimeConfig.general_routing`, MemoryId 5) and execution authority. None of that is
  insufficient, and this ADR changes none of it.
- **Inference canister** owns model execution. Since ADR 0116 it holds no durable state, no
  tokenizer and no uploaded artifact; the model is feature weights embedded in the wasm.
- **The scorer** is one conditional logit with a versioned feature spec. A new signal is
  representable as additional features of that scorer without a new model, a new router or a new
  decision owner.
- **ADR 0114 decision 4** already names this moment: "If model changes later need independent
  rollout, a follow-up ADR can move the weights to an uploaded artifact."

What the existing design cannot absorb as-is: an encoder is megabytes, needs a tokenizer, and makes
card vectors a piece of derived state that some canister must hold. The decision below is mostly
about who owns those three things.

## Alternatives

- **A. Keep extending cue lexicons and slot rules (minimum change).** No new state or artifact.
  Rejected as the end state because it is the process that produced the problem; it remains the
  path if the gates below fail.
- **B. Client-supplied question embedding through the ADR 0120 provider, card vectors authored
  off-chain and stored as catalog metadata.** No on-chain inference. Rejected: ADR 0120 decision 5
  keeps `ask`/`interpret` model-free, and it would make a text question unanswerable for a client
  without a 130 MB model (ADR 0122). A client vector is not an authorization risk — it can only pick
  among operations the caller may already execute — so this stays open for a client that already
  holds a provider, but it cannot be the routing contract.
- **C. An on-chain encoder whose similarity enters the existing scorer as features (chosen).**
- **D. A cross-encoder that reads the question and the cards together.** One hidden-768 layer over
  about 1,000 tokens is roughly 15–19B instructions (derived in the investigation record): over the
  query budget, so it would reverse ADR 0116's query-only route. Rejected for this slice.
- **E. A second, neural router beside the feature router, combined by voting or fallback.**
  Rejected: ADR 0114 removed the dual routing path, and two abstain decisions cannot be calibrated
  as one.
- **F. One inference canister or pack per graph.** Already rejected in ADR 0111; nothing here
  changes that.

Inside C, the encoder family is a measured choice between two candidates, not a decision of this
ADR: a **static token table with mean pooling** (no transformer forward; the lightweight-embedding
record calls it the only family that is structurally canister-friendly, and measured it weakest on
movie retrieval, nDCG@10 0.162 against 0.537 for Bekko on 23 queries) and the **one-layer
hidden-768 transformer encoder** above. Retrieval over synopses is not routing over cards, so
neither result transfers; both run through the same gate.

## Decision

1. **One scorer.** The neural signal is a small fixed set of similarity features (question vector
   against card vector) added to the conditional logit. The feature-spec version rises and the
   weights are retrained in Glev. The existing features, the slot extractor and the single
   calibrated abstain decision remain. There is no second routing path and no fallback between
   scorers.
2. **The encoder contract is `text → fixed-length quantized vector`**, with one encoder for
   questions and cards. Dimension, quantization and tokenizer are part of the encoder fingerprint.
   Which family implements it is whichever passes the acceptance gates at the lower cost.
3. **The inference canister owns card vectors**, as a content-addressed store keyed by
   `(encoder fingerprint, SHA-256 of the canonical card text)`.
   - The canonical card text has one owner: the function in `glev-general-router` that already
     renders a card for the lexical features. The Router does not render or hash cards.
   - A changed card has a different key, so no invalidation rule exists to get wrong; a caller-
     filtered context is a subset of the same keys.
   - Rejected sub-alternative: Router-stored vectors carried in the context. At 64 cards a 768-byte
     vector is 49 KiB, over `MAX_GENERAL_CONTEXT_BYTES` (32 KiB), and the Router would persist data
     only another canister can compute or validate.
4. **Card vectors are written on the update path, never on the request path.** The canister gains a
   controller-guarded update `embed_cards(context)` that encodes and stores the vectors missing for
   that context. It is idempotent and bounded per call; the Router repeats it until the context is
   covered. The Router calls it from the updates that change a graph's cards (`enable_ask`, prepared
   registration bound to the graph, graph-type change) and exposes one idempotent refresh for the
   graph tenant or controller (ADR 0115 authority).
5. **A missing card vector fails the request closed.** `route_general` verifies that every card in
   the context has a vector under the active encoder fingerprint and returns an error otherwise;
   the Router reports it as a failure, not as `Unsupported`. Presence in the store is the only
   source of truth, so no `Pending` status is added to the Router: a registration whose
   `embed_cards` call failed leaves the operation registered and `ask` failing until the refresh
   succeeds. `prepared_query` is unaffected.
6. **The route stays a composite query (ADR 0116).** A request encodes the question once, reads the
   stored card vectors and scores. Nothing is persisted on the request path.
7. **The model becomes an uploaded release set.** Encoder pack, tokenizer and the embedded feature
   weights are one compatible set identified by `routing_model_version`; the wasm carries the
   fingerprints it expects and refuses to route against any other pack or tokenizer. Installation is
   a Router-forwarded controller step in chunks under the ingress limit, as ADR 0111 had before the
   pack was removed. Activating a new set clears the card store.
8. **Wire version 8.** `GeneralRoutingContext`, `GeneralRouteRequest` and `GeneralRouteResult` keep
   their shape: the canister derives card keys from the context it already receives. Added: the
   install methods, `embed_cards`, and the missing-vector error. Fresh-install semantics; no
   compatibility path.

### State representability

| Claim | Representable today | Needs |
| --- | --- | --- |
| Similarity features in the scorer | yes: feature spec is versioned | new spec version, retrained weights |
| Card vector store | no: the canister has no stable regions | new stable region(s) in the inference canister |
| Encoder pack and tokenizer | no: removed on 2026-11-12 | install methods, stable regions, release manifest entries |
| "Card vectors not current" | yes: an error on the route, surfaced through `RouterError` | optionally a dedicated variant |
| Per-graph enablement and threshold | yes: `GraphRuntimeConfig` | nothing |

Cross-canister effects: `embed_cards` after a catalog write is the only new `await` following a
committed local effect. It is safe to fail because the write it follows does not depend on it, the
call is idempotent, and decision 5 makes the incomplete state observable rather than silent.

## Consequences

- A question can match a card it shares no token with; lexicon edits stop being the only lever.
- Adding a prepared operation stays a catalog change, with one extra bounded update to embed it.
- The inference canister is stateful again: stable regions, an install flow, a tokenizer load on
  install and on every upgrade, and a card store to bound.
- ADR 0116's "no tokenizer asset" and "no durable routing state" stop being true and ADR 0114
  decision 4's "no model or tokenizer asset is uploaded" is replaced.
- S2 and S3 of the investigation record (typed candidates, frontier judging) can reuse the encoder
  and the store, but neither is decided here.

## Trade-offs

- **Cost of the artifact.** The stock tokenizer measured 9.85B instructions to load its 34 MB JSON
  on install and again on each `post_upgrade`, under 1.1M per encoded text, in a 3.6 MB wasm
  (`2026-11-12-onchain-tokenizer-measurements.md`). That stack was deleted four days ago and comes
  back.
- **Card encoding can approach the update budget.** With the transformer candidate, a 60-token card
  is about 0.5B instructions (scaled from 1.32B at 141 tokens), so 64 cards are about 32B against a
  40B update. This is why `embed_cards` is bounded per call. The static candidate does not have this
  cost.
- **Model activation interrupts `ask`.** Clearing the store means every enabled graph fails closed
  until refreshed. A staged activation that keeps the old set serving would avoid it at the price of
  two live encoder versions; pre-production, the interruption is accepted and the operator tool
  refreshes enabled graphs.
- **Auditability.** A similarity feature is less inspectable than a cue match. It stays one term of
  a linear scorer, and `debug_interpret` can expose it beside the probabilities.
- **INT8 fragility is a release risk.** Hidden 512 ranged from 64.5% to 89.0% dev across seeds in
  the Rust forward while Python stayed near 90%; a gate that checks one seed is not evidence.
- **Card wording becomes model input.** Hand-tuned descriptions once moved dev from 72.5% to 82.5%
  and test from 75.9% to 68.8% with a frozen general decision model
  (`demo/knowledge/eval/routing/README.md`). Descriptions must not be tuned on a set that is then
  reported.

## Acceptance gates

No gate may be retuned after its data is frozen. The Glev host-side experiment runs first; no
canister work starts until it passes.

**Data, frozen before any training:**

- a held-out graph-type gate: graph types and catalogs that appear in no training row;
- a real-query holdout enlarged per the movie record's next collection pass (150 or more questions,
  a second annotator on at least 30% with agreement reported, a second graph);
- authored Japanese items, not translations.

**Quality, against the current pinned artifact:**

| Gate | Requirement |
| --- | --- |
| held-out graph types, top-1 | improvement with a paired-bootstrap 95% interval excluding zero |
| untouched real-query holdout | same test; out-of-scope abstain at or above 90% |
| existing ADR 0114 suites | no gate regresses |
| seed robustness | every one of at least three training seeds passes in the Rust INT8 forward |
| Rust/Python parity | operation match 100% on the fixed fixtures |

**Cost, PocketIC on the ADR 0114 grid (6/10/20/50 cards × 32/96/256 question tokens):**

| Metric | Target | Hard cap |
| --- | ---: | ---: |
| `route_general` worst cell | ≤ 1B instructions | 5B |
| `embed_cards`, one call | recorded | 40B |
| install and `post_upgrade` | recorded | 40B / 300B |
| wasm code section | recorded | 12 MiB |
| card store bytes per graph | recorded and bounded | — |

**Stop rule.** If neither encoder family passes the quality gates, this ADR is rejected and
alternative A continues. A pass on training-time graph types alone is not a pass.

## Migration

Pre-production fresh install: wire version 8, new inference-canister stable layout, new release
manifest entries. Enabled graphs need their cards embedded once after install. No compatibility
branch, no dual route, no legacy decoder.

## Design documentation impact

On acceptance, in the same change as the implementation — not now, while the status is proposed:

- ADR 0114: decision 4 and the feature count; ADR 0116: the tokenizer and durable-state
  consequences; ADR 0110 and 0111: the pack and its Router-forwarded install return.
- `design/storage/stable-memory-inventory.md`: inference-canister regions for the pack, tokenizer
  and card store.
- SDK/CLI docs: the refresh operation and the missing-vector failure.

## Review axes

- **Encapsulation:** the canister owns the encoder, the card text rendering and the vectors derived
  from them; the Router owns the catalog and never sees a vector.
- **Separation of concerns:** authorization and execution are unchanged; grants still decide which
  cards reach the scorer and are never model input.
- **Invariants:** every scored card has a vector under the active encoder fingerprint, or the
  request fails; the pack, tokenizer and weights in use are one release set; nothing is written on
  the request path.
- **Consistency:** card vectors are derived state with a content-addressed key, so the only
  consistency operation is "compute what is missing".
- **Fitness for purpose:** the change adds one signal to one scorer for one measured failure. It is
  not accepted on capability; it is accepted only if the held-out gates move.

## Open questions

- Does the canonical card text include ADR 0117 domain values and aliases? They are the most
  semantic part of a card and the most frequently edited.
- Store bound and eviction: a hard cap with explicit clearing, or least-recently-used? An evicted
  vector is a failed request under decision 5.
- Can the tokenizer be stored in a preprocessed form so `post_upgrade` does not pay 9.85B each
  time?
- Is the encoder trained from the existing teacher's soft labels or from new schema-conditioned
  data generated across synthetic graph types?
- Joint scoring at query cost — the question attending over stored card vectors in one small layer
  — is unmeasured and out of scope here.
