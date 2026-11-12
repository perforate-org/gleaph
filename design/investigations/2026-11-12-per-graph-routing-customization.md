# Per-graph routing customization: intent subsets, indirection and graph-local intents (2026-11-12)

Question: how can a graph administrator customize the routing query set or query contents for one
graph, without duplicating the deployment-wide model? Four cases must be separated: (1) selecting a
subset of the trained intents, (2) mapping a trained intent to a graph-local prepared query with a
different name/body, (3) adding graph-local intents the model never saw, (4) per-graph slot schemas —
while (5) quota, authorization and fail-closed behavior stay intact.

Status: investigation record (append-only), 2026-11-12. No code, test or configuration was changed.
Claims are marked `[repo]`, `[measured]`, `[derived]` (follows from code or arithmetic), `[cited]`
(external primary source) or `[proposal]`.

## 1. Current constraints

### 1.1 The model

- One deployment-wide INT8 pack (~9 MB wire, **~328 MB loaded heap**) per Router deployment; per-graph
  copies were rejected in ADR 0111 (fixed cost, no isolation). `route_options` is a **pack-level list
  of six labels**: five demo intents plus `none-of-these` (`glev/experiments/routing_schema.py`). `[repo]`
- **Intent identity is positional, not textual.** `forward_question` builds `[bos] + question + one
  <mask> per route option` and softmaxes a shared scorer's logits at those positions
  (`glev-infer/src/model.rs`). The shipped student takes no option text (`marker_pos` +
  `type_emb(qtype)`); the "6 inline options" shape in the 2026-11-10 note is the *teacher*. So
  **renaming is free; reordering or adding an intent moves positions and needs retraining.**
  `[repo]` `[derived]`
- Slots are pack-level: six types (`anchor, hops, concept, depth, team, topic`) → 13 BIO tags stored
  as `slot_tag_names` (`glev/experiments/slot_schema.py`). `[repo]`
- Abstain is not a separate head: `none-of-these` is the sixth softmax class, thresholded at
  0.23 / 0.25 / 0.30 / 0.50 with temperature 2.5 or 1.0 by token count and CJK fraction
  (`sdk/client/rust/src/routing/client.rs`, `AbstainPolicy`). `[repo]`
- Default checkpoint (full six-option set): blind accuracy 91.7%, coverage 91.1%, precision 100%, OOS
  refusal 93.3% (`demo/knowledge/eval/routing/README.md`); no subset is measured. `[measured]`

### 1.2 Per-graph state and enforcement today

- `GraphRuntimeConfig` (MemoryId 5) holds the element-id key, index group size, index cluster, next
  shard id and **`inference_enabled`**; routing has exactly one per-graph bit
  (`crates/router/src/facade/stable/memory.rs`). `[repo]`
- Prepared queries are per graph and already typed: name, description, kind, parameters
  (`SemanticType`, `not_null`), result schema, sorts; execution is **default deny** except
  public-or-explicit grants (`prepared.rs`, `rbac.rs`). `[repo]`
- Router ingress order: graph exists → tenancy → `inference_enabled` → 256-token cap → quota → proxy
  (ADR 0112). Quota is per `(caller, graph)`, charged before the model call, not refunded on failure.
  `[repo]`

### 1.3 The intent→action mapping is currently code, in four places

1. the pack's `route_options` strings (labels only);
2. the SDK `AbstainPolicy` + `route`, returning the winning **label**, not an operation;
3. the SDK `declared_tags(op)` / `bind_spans` per-op match, hardcoding slot whitelists, defaults
   (depth 3, hops 2), ranges (1..=3) and the `concept` → `from`/`to` split
   (`sdk/client/rust/src/routing/slots.rs`);
4. the CLI `execution_plan(operation, bindings)`, hardcoding per-operation names and clamps
   (`crates/cli/src/ask.rs`).

A sixth prepared operation is therefore an SDK **and** CLI change today, not graph data — the
duplication a per-graph profile must remove rather than copy a fifth time. `[repo]`

### 1.4 Governance and catalog scoping (blocks "graph admin edits it" today)

- The prepared catalog is a Router stable map keyed by **query name alone**
  (`PreparedPlanKey { name }`, MemoryId 8, `crates/router/src/facade/stable/prepared_catalog.rs`;
  ADR 0061). Each record carries a `graph_id`, but the key does not: a second graph cannot register
  the same query name, and `list_prepared`/resolution scan the caller's visible graphs and fail closed
  on ambiguity (`crates/router/src/prepared.rs`; ADR 0061 "wrong-graph read" note). `[repo]`
- Registration takes no graph argument and is authorized by the **global** `PREPARE_REGISTER`
  capability (`authorize_prepared_catalog_change`, `crates/router/src/rbac.rs`), not by graph
  ownership. Graph ownership is the implicit root for *execution* authority
  (`authorize_prepared_execute`), not for catalog writes. `[repo]`
- Inference enablement is likewise controller/governance-only today:
  `admin_enable_graph_inference` calls `require_controller`; ADR 0111 says graph-owner delegation is
  "a later widening". `[repo]`
- Consequences:
  1. A per-graph profile can be *data* only if some actor is allowed to write it. "The graph owner
     edits it" requires either a new graph-scoped capability/key or a platform-managed profile that
     the graph owner selects from; it is not a property of adding a `GraphRuntimeConfig` field.
  2. Intent labels cannot equal graph-local query names across graphs. Either prepared names stay
     globally unique (profile maps to a prefixed name), or ADR 0061's key changes to
     `(GraphId, name)` — a real catalog/registration change, not a profile detail.
  3. A profile that maps to a prepared query the Router cannot resolve for that graph is unusable no
     matter how well it validates; activation must check graph existence and the caller's grant path,
     not just that the operation name exists globally.

### 1.4 IC envelope that bounds the design

| Limit | Value |
| --- | --- |
| Ingress payload / same-subnet request / replicated response | 2 MiB / 10 MiB / 2 MiB |
| Instructions per update / query / install-upgrade | 40 B / 5 B / 300 B |
| Wasm heap (wasm32) / stable memory per canister | 4 GiB / 500 GiB |
| Update execution threads per canister | 1 |

Source: [ICP resource limits][rl]. The 1.9 MB pack chunk is the 2 MiB ingress limit minus overhead;
with a 328 MB model, a 34 MB tokenizer and one update thread, per-graph replicas or adapters are
memory- and provisioning-bound, not CPU-bound. `[cited]` `[derived]`

## 2. Mechanism facts that constrain every option

- **Subsetting changes abstention.** Masking removed classes and renormalizing gives
  `p'(y) = p(y)/Σ_{s∈S} p(s)`, hence `p'(none) ≥ p(none)`: with a fixed threshold every subset
  abstains more. Gating on `argmax ∉ S` leaves `p(none)` untouched and just refuses the question.
  The difference is whether an out-of-profile question is forced into a weaker in-profile intent.
  `[derived]`
- **Mask, do not shorten.** Marker count equals `route_options.len()` and the encoder is bidirectional,
  so dropping markers changes every representation; mask the six logits instead. `[derived]`
- **Mask once, after window fusion.** Window probabilities combine by element-wise max with `none`
  averaged (`forward_question`); renormalizing per window then maxing is a different function. `[derived]`
- **A profile is not an authorization boundary.** Authority is checked on the resolved prepared
  operation against grants, so a client that ignores the profile still cannot escalate; the profile is
  policy and UX, not a second ACL. `[derived]`
## 3. Options considered

- **O1 — Profile + indirection, no model change.** Per-graph data: enabled trained intents,
  `intent → prepared operation`, `slot tag → parameter`. Unmapped intent ⇒ abstain; no renormalization.
- **O2 — Masked softmax over the subset.** Same profile, but removed intents are masked and the
  remainder renormalized, with per-profile thresholds.
- **O3 / O4 — Adapters, heads, or reserved intents.** Shared encoder plus per-graph LoRA/head, or *k*
  generic extra intents that each graph assigns meaning to.
- **O5 — Description/embedding semantic router.** Per-graph routes with utterances or query
  descriptions, embedded and matched against a calibrated threshold.
- **O6 — LLM-based routing.** Ask an LLM (e.g. `ic-llm` tool calling) which action applies.
- **O7 — Per-graph model canister.** One inference canister or pack per graph.

## 4. Evidence per option

### O1 — profile and indirection

Decoupling "which action" from "what the action does" is the standard shape: MCP tools are a **name
plus an input schema** with caller-side invocation ([MCP][mcp]); `ic-llm` branches on `function.name`
in caller code ([ic-llm][icllm]); Azure CLU orchestration is the two-level version, where a router
intent selects a child project ([orchestration][orch]).

Gleaph already has both halves: stable labels from the pack, and a per-graph prepared catalog with
typed parameters and grants. The mapping is one bounded table lookup per graph — no model or
calibration change. The only loss is coverage: an unmapped winning intent abstains, the correct
behavior when the graph has no such query. O1 answers (1), (2), (4), (5), not (3). `[proposal]`

### O2 — masked softmax

Renormalizing a subset is the exact conditional `p(y | x, y ∈ S)` only if `p` is calibrated for that
population. Calibration parameters are fitted to a distribution: temperature scaling is a post-hoc fit
([Guo et al.][cal]), and in-domain calibration of transformer classifiers does not carry out-of-domain
([Desai & Durrett][pretrained-cal]). Open-set recognition adds the other half: a closed-set softmax has
no mass for "unknown", so OpenMax recalibrates activations ([Bendale & Boult][openmax]).

Here `none-of-these` *is* the unknown class, so renormalization moves exactly the quantity the abstain
policy thresholds, and those thresholds were fitted on the full six-option distribution across four
length/CJK regimes; the 93.3% OOS refusal and 91.1% coverage are full-set numbers. `[derived]` `[measured: full set only]`

Verdict: plausible but unmeasured; it needs a subset bench and per-profile thresholds.

### O3 — per-graph adapters or heads

LoRA injects low-rank matrices into a frozen base with no added inference latency ([Hu et al.][lora]).
Serving many adapters is a systems problem — S-LoRA needs a paged memory pool and heterogeneous
batching on a GPU ([Sheng et al.][slora]). A canister has one update thread, a 4 GiB heap and a 40 B
budget, and adapters need chunked install, versioning and rollback. The blocker is not only memory but
**data**: a graph administrator has a catalog and questions, not labelled intents. `[derived]`

Verdict: reject for now; revisit if per-graph labelled data becomes a product surface.

### O4 — reserved graph-local slots

With no textual option input, a reserved slot cannot learn what a graph means by it; *k* is fixed at
training time and the mapping creates no discrimination. `[derived]`

Verdict: reject. If a future model consumes descriptions, O5 subsumes it.

### O5 — description / embedding semantic router

The reference implementation is direct prior art: a route is a **name plus example utterances**, an
encoder embeds them, and `fit()` calibrates a confidence threshold from labelled queries including
`None`, returning `None` when nothing clears ([semantic-router][sr]). Fixed sentence-encoder intent
detectors are competitive and cheap ([Casanueva et al.][use]); an embedding pass would fit a 5 B query
call on IC.

The repository's measured evidence is **negative for description-driven routing of this catalog**:
"use when" descriptions lifted dev top-1 from 72.5% to 82.5% and dropped test from 75.9% to 68.8%;
rewriting them in three styles moved 40-item slices by up to 45 points and broke other intents;
`catalog.json` is recorded as a rejected prototype (`demo/knowledge/eval/routing/README.md`
§Measured results, §catalog). `[measured]`

Structurally the pack ignores descriptions entirely (§1.1), so O5 is not a configuration feature but a
**second model** with an encoder and per-graph calibration data — and a graph administrator has no
`None`-labelled queries, which is what `fit()` needs. Verdict: defer, not a v1 answer.

### O6 — LLM-based routing

`ic-llm` is a gateway to off-chain AI workers: the model is a per-call string, every `send()` attaches
100 B cycles (and traps below that), and its workflow is "LLM returns a tool call, your code executes it"
([ic-llm][icllm]). The repository's IC reality check records that this path is off-chain, not
consensus-replicated and not confidential (`2026-11-09-natural-language-query-research.md` §5.1).
Routing through it would be non-deterministic across replicas and dependent on an external operator —
incompatible with the Router's fail-closed, quota-charged update shape. `[repo]` `[derived]` Verdict:
reject for the request path; acceptable as an offline authoring aid.

O7 (one inference canister or pack per graph) was already rejected in ADR 0111 (9 MB pack, 328 MB
heap, fixed cost per graph, no isolation benefit); nothing measured since changes that. `[repo]`

## 5. Comparison

| Option | Model change | Retrain | Extra memory | Per-graph data | Calibration risk | Graph-local intents | Fail-closed | Size |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| O1 profile + indirection | none | no | none | profile record | none | no (mappable only) | yes | small |
| O2 masked softmax | none | no (mask only) | none | profile + thresholds | **high, unmeasured** | no | yes | small |
| O3 adapters/heads | adapter per graph | yes, per graph | MBs per graph | adapter + labels | medium | yes | yes | large |
| O4 reserved slots | k extra intents | once | none | mapping | medium | weakly | yes | medium |
| O5 semantic router | second encoder | yes | embedding model | utterances + calibration | **high** (measured instability) | yes | yes | large |
| O6 LLM routing | external call | no | none | prompt/config | n/a | yes | **no** | medium |
| O7 per-graph model | none | no | ~328 MB per graph | whole model | none | no | yes | very large |

## 6. Recommendation

1. **Pursue O1 now; hold O2 until measured.** Add a `GraphRoutingProfile` beside the per-graph
   prepared catalog: `intents: { trained_label → LocalBinding }`,
   `LocalBinding { operation, slots: { tag → parameter } }`, `unmapped_intent: Abstain`. Resolution is
   `label → binding → prepared operation`; an absent label abstains. No renormalization, so the
   measured thresholds stay valid.
   Before choosing the profile's storage/editing surface, decide the governance fork in §1.4:
   either the profile is **platform-managed and graph-owner selectable** (no new authorization), or
   prepared registration becomes **graph-scoped** (`(GraphId, name)` key plus a graph-owner
   `PREPARE_REGISTER` path), which is the only way a graph owner can truly author its own catalog.
2. **Validate when the profile is written, not when a request arrives.** A profile edit (same
   `PREPARE_REGISTER` privilege as `prepare`) must reject: a binding to a missing operation; a tag not
   in the pack's `slot_tag_names`; a bound operation whose required parameter is neither mapped nor
   defaulted; two intents claiming one operation with incompatible slot maps. Fetch `route_options`
   and slot names through the Router at write time so the pack stays the single source of truth.
3. **Keep authority where it is.** After resolution the Router still calls `authorize_prepared_execute`
   on the graph-local operation and still charges quota in the existing order. The profile is policy,
   not an ACL, and must not be documented as one.
4. **Delete the duplicated per-op tables** (`declared_tags`, `bind_spans`, CLI `execution_plan`) so
   tag→parameter binding has one owner; otherwise a graph-local op is data-expressible but unserviceable.
5. **Reject O3, O4, O6, O7; defer O5 behind one experiment.**
6. **Measure O2 before considering it.** On the existing 400-item test split (five intents, 20% OOS),
   evaluate every non-empty proper subset under both policies (gate vs renormalize): conditional top-1,
   abstain rate and OOS refusal per length/CJK regime, using `demo/knowledge/scripts/routing-bench/`;
   CPU-only and offline, not an on-chain benchmark. `[proposal]`

End state: the **pack owns intent identity and tags**, the **graph owns the mapping, enabled subset and
prepared bodies**, the **Router owns enablement, quota and authorization**. Requirement (3) stays out of
scope until a model can read descriptions or a graph can supply labels; it must not be approximated by
reserved slots or by text the model cannot see.

## 7. Open questions

- Where does the profile live: a new `GraphRuntimeConfig` field (MemoryId 5) or the prepared catalog
  manifest? The tie breaker is editability under `PREPARE_REGISTER` without a second auth path.
- Governance fork: do we widen prepared registration to graph owners (graph-scoped key and
  capability) or keep registration platform-admin and let graph owners only select a platform
  profile? The former serves the product request; the latter avoids a new authority root but is not
  "the graph admin changes the queries" in the full sense.
- If a graph-local operation has a required parameter no tag can fill, is it *unroutable* (registration
  error) or *routable with a clarification step*? The SDK today returns `SlotError::Missing`.
- May two intents map to one operation (aliases), and may a profile disable `none-of-these`? Never
  abstaining inverts the fail-closed contract and should be rejected.
- If the Router returns resolved bindings instead of labels, `WIRE_VERSION` must bump (currently 2,
  ADR 0113) and the SDK's abstain policy either moves or is mirrored. Which side owns the policy is
  unresolved.
- Does a per-profile threshold compose with the length/CJK policy or replace it? Two calibration
  dimensions on 400 test items is likely underpowered.
- Demo documentation is stale: `test.jsonl` has five intents (64 positives each, 20% OOS) and
  `prepared/` has five files, while `demo/knowledge/eval/routing/README.md` still says "the four
  prepared queries" and "720 (180 × 4)". `[repo]`

## 8. Sources

Primary (fetched 2026-11-12): [ICP resource limits][rl]; [MCP Tools specification][mcp];
[ic-llm README][icllm]; [Azure CLU orchestration workflow][orch]; [Semantic Router][sr];
[Guo et al., calibration][cal]; [Desai & Durrett][pretrained-cal]; [Bendale & Boult][openmax];
[Casanueva et al.][use]; [Hu et al.][lora]; [Sheng et al.][slora].

Repository: ADRs 0111/0112/0113;
`design/investigations/2026-11-10-canister-inference-optimization-research.md`,
`2026-11-10-model20-canister-inference-feasibility.md`,
`2026-11-09-natural-language-query-research.md` §5.1–5.2;
`crates/router/src/{prepared.rs,rbac.rs,api/inference.rs,facade/stable/memory.rs}`;
`crates/gleaph-routing-wire/src/lib.rs`, `crates/gleaph-routing-canister/src/lib.rs`,
`sdk/client/rust/src/routing/{client.rs,slots.rs}`, `crates/cli/src/ask.rs`;
`glev` `experiments/{routing_schema.py,slot_schema.py,qonly_slot_train.py}`,
`crates/glev-infer/src/model.rs`; `demo/knowledge/eval/routing/README.md`,
`demo/knowledge/prepared/*.gql`, `demo/knowledge/eval/routing/{test.jsonl,catalog.json}`.

[rl]: https://docs.internetcomputer.org/references/resource-limits/
[mcp]: https://modelcontextprotocol.io/specification/2025-06-18/server/tools
[icllm]: https://github.com/dfinity/llm/blob/main/rust/README.md
[orch]: https://learn.microsoft.com/en-us/azure/ai-services/language-service/orchestration-workflow/overview
[sr]: https://github.com/aurelio-labs/semantic-router
[cal]: https://arxiv.org/abs/1706.04599
[pretrained-cal]: https://arxiv.org/abs/2003.07892
[openmax]: https://arxiv.org/abs/1511.06233
[use]: https://arxiv.org/abs/2003.04807
[lora]: https://arxiv.org/abs/2106.09685
[slora]: https://arxiv.org/abs/2311.03285
