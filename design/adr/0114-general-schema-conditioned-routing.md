# 0114. General schema-conditioned routing as the Router's routing contract

Date: 2026-11-12
Status: accepted (2026-11-12). Implemented end to end: Rust parity, canister benchmark, Router ingress/profile, SDK/CLI general text path, and fixed-intent neural pack retirement. Wire version is 3.
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 18:30:00 UTC +0900
Implementation evidence: [2026-11-12-general-routing-benchmark.md](../investigations/2026-11-12-general-routing-benchmark.md),
[2026-11-12-general-routing-router-integration.md](../investigations/2026-11-12-general-routing-router-integration.md),
[2026-11-12-general-routing-sdk-cli-switch.md](../investigations/2026-11-12-general-routing-sdk-cli-switch.md),
[2026-11-12-general-routing-neural-retirement.md](../investigations/2026-11-12-general-routing-neural-retirement.md),
[2026-11-12-general-routing-calibration.md](../investigations/2026-11-12-general-routing-calibration.md)

## Context

ADR 0111/0113 made the Router own one deployment inference canister and proxy its fixed six-intent
routing model; the model is a small INT8 encoder with positional `<mask>` intents. That model cannot
read a graph's operation catalog, so every new intent needs a retraining/release cycle.

Glev now has a schema/catalog-conditioned alternative (`glev: experiments/general_routing/`):
a 29-feature conditional-logit reranker over operation cards plus a deterministic typed slot
extractor. Measured on the pinned gates:

- routing: synthetic 100% top-1; real-paraphrase 86.1%; `demo/movies` 80%; `demo/knowledge` 90.9%;
  independent `wild-handwritten` knowledge holdout 88.8% (top-5 100%, OOS refusal 96.2%);
- slots: synthetic 100% exact; real-paraphrase explicit-English 95.5%; manually annotated demo dev
  90.9% exact / 95.5% complete;
- scale: 6/10/20/50 operation catalogs keep 99% top-1 and 99–100% top-5 with all-card scoring;
  lexical top-k retrieval is not needed at 50 cards and caps accuracy by recall;
- OOS refusal is catalog-size sensitive (0% at 6/10 cards with a 6-card threshold, 100% at 50), so
  the abstain threshold must be calibrated per graph/catalog.

The model is a table of feature weights and slot rules, not a neural pack. Its cost is negligible
beside the tokenizer; it does not need the 9 MB INT8 encoder pack.

## Problem

The fixed-intent neural routing contract cannot satisfy the product requirement that each graph
choose its own prepared-query catalog. The general model can, but it is currently a host-side
Python experiment with no execution contract: no context ownership, no wire, no authorization
placement, no model versioning, and no retirement path for the fixed-intent pack.

## Existing architecture assessment

- Router owns the graph catalog, prepared-query metadata (`PreparedOperation` with typed
  parameters), grants, quota and the per-graph `inference_enabled` flag (MemoryId 5).
- The inference canister owns the tokenizer and token IDs (ADR 0113); its model pack is deployment-wide.
- Prepared-query registration is keyed by name only and authorized by the global `PREPARE_REGISTER`
  capability (ADR 0061); graph-owner authoring is not available. This ADR does not change that
  governance surface.
- The wire contract was version 2 (fixed-intent neural text/token routes); the general path replaces it with version 3.

## Alternatives

- **A. Extend the fixed-intent neural pack with more intents.** Rejected: every graph catalog would
  still be a training/release cycle, and the 50-card scale does not require a neural encoder.
- **B. Keep the general model host-side and let clients call it.** Rejected: grants, quota and
  catalog scoping belong to the Router; a client-supplied operation is not an authorization boundary.
- **C. Put the general model in the Router.** Rejected for this slice: the tokenizer lives in the
  inference canister (ADR 0113), and duplicating tokenization in the Router would create a second
  tokenizer/feature implementation.
- **D. Inference canister executes the general model; Router supplies the canonical graph context
  and enforces execution authority (chosen).** Keeps one model runtime, one tokenizer, and keeps
  catalog/authorization with the Router.
- **E. Replace the fixed-intent pack now and delete the neural path immediately.** Chosen as the
  target, with an explicit parity slice: the neural path stays only until the general path passes
  the same gates, then is deleted (pre-production, no compatibility shims).

## Decision

1. **The general schema-conditioned router becomes the routing contract** for graphs whose routing
   is enabled. Its output is one prepared operation plus typed parameter values, or an abstain. The
   fixed six-intent neural routing path and its pack are retired after the parity slice; no dual
   production routing path remains.
2. **The Router owns the routing context.** For one request it builds
   `GraphRoutingContext` from:
   - the graph-type summary (labels, properties, edges) from the Router graph catalog;
   - the graph's prepared-operation metadata: name, description, kind, required/optional typed
     parameters and result shape;
   - only operations the caller may execute (grants/PUBLIC/ownership), so the model cannot select a
     forbidden operation;
   - the per-graph routing policy (abstain threshold, OOS rule version, catalog version).
   Clients never supply the context. The context is ephemeral: it is derived from the catalog on
   each request, not persisted as a duplicate catalog.
3. **The inference canister executes feature extraction, routing and slot extraction.** It receives
   `(text, GraphRoutingContext)` and returns the selected operation, probability vector, typed slot
   values with spans, `incomplete`/`ambiguous` flags, the context fingerprint and the model version.
   The general model uses its own deterministic regex tokenization (matching the frozen Python
   features), so it does not need the BPE tokenizer; the tokenizer stays owned by the canister for
   the separate token-count/quota path (ADR 0113). The Router re-validates that the returned
   operation exists in the context/graph and re-checks `authorize_prepared_execute`; the canister
   never grants execution.
4. **The model artifact is versioned and tiny.** Feature weights, feature-spec version, cue-lexicon
   version and slot-rule version are embedded in the inference-canister wasm and reported by a
   `routing_model_version()` query. The tokenizer remains the uploaded 34 MB asset from ADR 0113.
   If model changes later need independent rollout, a follow-up ADR can move the weights to an
   uploaded artifact; this slice deliberately avoids new artifact plumbing.
5. **Wire surface.** The additive general route contract is versioned by
   `GENERAL_ROUTE_VERSION = 1` in `gleaph-routing-wire`; the existing `WIRE_VERSION` stays 2 until
   the Router/SDK switch, so the fixed-intent path keeps compiling during the benchmark slice. New
   Candid types:
   - `RoutingParameter {name, type, required}`;
   - `RoutingOperation {id, description, parameters, result}`;
   - `GraphRoutingContext {schema_version, graph, operations, abstain_threshold, policy_version,
     context_fingerprint?}` (the fingerprint is computed by the Router and checked by the canister
     against the serialized context);
   - `RouteRequest {text, context}`;
   - `RouteResult {operation: Option<String>, probabilities: Vec<f32>, slots: Vec<SlotValue>,
     incomplete: bool, abstain: bool, context_fingerprint: Vec<u8>, model_version: u32}`.
   Limits: at most 64 operations, 32 KiB serialized context, 16 KiB text, 256 question tokens.
   The neural `route_and_extract_text`/token methods and the pack-upload methods are removed;
   wire version 3 exposes tokenizer, token-count and general-route types only.
6. **Slot values are surface values with spans, not resolved graph identities.** The Router/executor
   validates them against the prepared parameter schema; entity linking and vector-parameter
   construction remain separate concerns. Missing required or ambiguous parameters produce a
   fail-closed abstain/clarify result, never an execution.
7. **Per-graph policy is required.** The scale result shows abstain behaviour is catalog-size
   sensitive, so `GraphRuntimeConfig` (MemoryId 5) carries `general_routing`:
   `abstain_threshold_bps` and `policy_version`; `inference_enabled` remains the enable bit, and the
   context fingerprint is derived per request rather than stored. `None` uses the documented default
   (2,000 bps / policy 1). Writes are controller/governance gated until the graph-owner
   authorization ADR (0115) lands.
8. **Quota unchanged.** ADR 0112 pricing remains based on the exact question token count from
   `tokenizer_token_count`; context bytes are bounded and not charged. The routing call is cheap
   relative to the token-count call and model forward, so no new quota dimension is introduced.

## Consequences

- A graph can route over its own prepared-query catalog without retraining the model; adding a
  catalog operation is a catalog/metadata change, constrained to the trained operation shapes.
- The neural 9 MB pack and its fixed intent list leave the deployment path; inference canister code
  shrinks to tokenizer + feature router + slot extractor.
- The Router becomes the source of truth for graph routing context and per-graph calibration; the
  context must stay bounded, and every request carries it to the canister.
- The general model's quality is measured, not assumed: the fixed gates from the Glev experiments
  are the acceptance gates for the Rust/canister parity slice.
- New operation shapes outside filter/search/similar/shared/reach/access/path still need Glev
  training data and a release; this ADR does not make arbitrary new semantics trainable at runtime.

## Trade-offs

- Carrying the context per request costs bytes and tokenization of card text, but avoids a second
  catalog state and stale per-graph caches; bounded at 64 cards / 32 KiB.
- The feature model is less fluent than an LLM and has hand-authored cue lexicons and an OOS rule;
  the alternative is a neural model that is harder to audit and impossible to run at the same cost.
- Embedding the weights in the wasm ties model updates to canister upgrades; acceptable while the
  model is small and pre-production, and reversible with an artifact ADR.
- Per-caller contexts mean the same question can route differently for two callers (different
  grants); this is intended: routing must only offer operations the caller may execute.

## Migration

Pre-production fresh-install semantics: graphs are reinstalled with a routing profile; the neural
pack has been removed; the general path replaces the neural text route and wire version 3 exposes
only tokenizer/token-count/general-route methods. No compatibility shims or dual routing remain.

## Design Documentation Impact

- ADR 0111/0113: fixed-intent neural routing and pack upload are superseded by the general path;
  the tokenizer ownership decision (ADR 0113) stays.
- ADR 0112: quota unchanged; the general path reuses the exact token count.
- `design/storage/stable-memory-inventory.md`: MemoryId 5 gains the general routing profile; the
  inference canister's pack cache/regions are retired with the neural pack.
- SDK/CLI docs: text routing returns operation + typed slots, not neural probabilities/spans.

## Benchmark plan and acceptance gates

The general path is accepted only when the same fixed Python suites and new Rust/canister
benchmarks pass. No benchmark is retuned between the Python and Rust runs.

### Quality gates (host Python, fixed data)

| Suite | Command | Gate |
| --- | --- | --- |
| synthetic routing | `python3 experiments/general_routing/reranker.py --seed 7 --epochs 800` | test top-1 100%, top-5 100% |
| real-paraphrase routing | `python3 experiments/general_routing/gate.py --seed 7 --epochs 800` | real-test top-1 ≥ 80% |
| independent paraphrase holdout | `gate.py --gate-file real_paraphrase_holdout.jsonl` | top-1 ≥ 90% |
| movie catalog | `real_catalog_eval.py --movie-eval ...` | test top-1 ≥ 70%, OOS ≥ 90% |
| knowledge catalog | `real_catalog_eval.py --knowledge-blind ...` | test top-1 ≥ 65%, OOS ≥ 90% |
| knowledge wild holdout | `real_catalog_eval.py` / holdout run | top-1 ≥ 75%, OOS ≥ 90% |
| slot extraction | `slot_eval.py` | synthetic exact 100%/complete ≥ 90%; gate exact ≥ 75%; demo exact ≥ 70% |
| 50-operation scale | `scale_eval.py` | all-card top-1 ≥ 95%, top-5 ≥ 99% |

### Parity gates (Rust library vs Python)

- Same exported weights/cues/thresholds and the same fixed fixture rows.
- Operation match 100%; probability absolute difference ≤ 1e-6; slot values/spans exact.
- The Rust runner must reproduce the Python quality table above from checked-in fixtures, not from
  a live model server.

### Canister cost benchmark

A PocketIC benchmark installs the general-path canister, uploads the pinned tokenizer, and calls
`route_general(text, context)` with generated contexts of 6/10/20/50 operations and question text
of 32/96/256 tokens. Each cell records:

- `performance_counter(0)` delta for the whole call (tokenizer + features + slot extraction);
- wasm code size and heap delta;
- context bytes and operation count.

| Metric | Target | Hard cap |
| --- | ---: | ---: |
| `route_general`, ≤16 KiB text + ≤64 cards | ≤ 1B instructions | 5B (query budget) and 40B (update) |
| 50-card / 256-token cell vs 6-card / 32-token cell | ≤ 10× instructions | no super-linear blow-up |
| wasm code section | well under 12 MiB | 12 MiB |
| transient context/tokenizer allocation | measured and bounded | 64 MiB |

If the 32KiB-context / 64-card cap cannot meet the target, the fallback is lexical top-k retrieval
before feature extraction; the scale benchmark already shows retrieval is not needed at 50 cards,
so this fallback is measured, not assumed.

### Reporting

The benchmark report is checked in beside the implementation (quality JSON, Rust parity report,
PocketIC instruction table). A benchmark regression blocks acceptance even when the Rust parity
passes.

## Implementation status

All implementation slices are complete: Rust parity (756/756 cases, all quality gates), the general
canister method with a PocketIC instruction benchmark (worst cell 361.2M, target 1B; wasm 3.77 MB),
the Router context builder plus `route_general` ingress (PocketIC E2E: movie and knowledge catalogs,
operation+slot results, OOS abstain), controller-governed per-graph threshold/policy, `gleaph ask
--graph` using `route_general` without a local tokenizer, and removal of the fixed-intent neural pack
and its upload/artifact wiring. Calibration tooling and the graph-owner governance ADR (0115)
remain. See the evidence records for the exact grids and environment.

## Implementation slices

1. **Rust feature router + slot extractor crate with parity.** Port `features.py`/`reranker.py`/
   `slots.py`; JSON fixtures from Python; byte/float parity on the fixed quality table; no canister
   change. The parity runner is also the quality-gate runner.
2. **Inference canister general API.** Add wire v3 context/request/result types, tokenizer-based
   feature extraction, slot extraction, model version query; keep the neural path behind a
   temporary test/parity feature until slice 4. Add the PocketIC cost benchmark from the benchmark
   plan and record the 6/10/20/50 × 32/96/256 table.
3. **Router context builder and ingress.** Build the per-caller context from graph type + prepared
   metadata; enforce size limits; send context; validate the returned operation; per-graph threshold
   in MemoryId 5; quota/auth order preserved.
4. **SDK/CLI switch and neural retirement.** Text routing consumes operation + typed slots; remove
   pack upload, fixed-intent methods and neural artifacts; update release manifest/canister wasm.
5. **E2E and gates.** `demo/movies` and `demo/knowledge` route through the general path; fixed
   routing/slot gates, the 50-card scale benchmark, Rust/Python parity and the canister instruction
   benchmark all pass; independent wild holdout recorded.
6. **Separate governance ADR (0115).** Graph-owner-authored catalog/profile under graph-scoped
   authorization; not required for the control-plane-managed profile in this ADR.

## Review axes

- **Encapsulation:** Router owns catalog, authorization and context; canister owns tokenization,
   model execution and slot extraction; clients never supply the operation.
- **Separation of concerns:** routing policy is data in the graph profile; execution authority stays
  with the Router; prepared query execution stays with the graph/executor.
- **Invariants:** returned operation must be in the caller-visible context and pass
  `authorize_prepared_execute`; incomplete/ambiguous slots never execute; context fingerprint must
  match; context/card/text limits enforced.
- **Consistency:** one routing model version, one context schema version, one OOS policy version.
- **Fitness for purpose:** 50-card catalogs pass; model/slot quality gates are the measured Python
  baselines, not new claims.

## Open questions

- Context caching by fingerprint in the canister: worthwhile only after measuring a real deployment;
  not in the first slice.
- Entity linking for parameter values (titles vs IDs) and vector-byte parameters; deferred to the
  executor/Router parameter-resolution work.
- Whether the OOS rule stays a hand-authored policy or becomes a learned gate; the rule is versioned
  and per-graph calibrated for now.
- Graph-owner authoring is decided in [ADR 0115](0115-graph-owner-routing-authority.md):
  global names stay, graph tenants may author operations bound to their own graph, and they may
  read/write the per-graph profile and run the calibration diagnostic.
- Independent real-user traffic is still missing; the wild-handwritten holdout is the current
  external check and is not pristine.
