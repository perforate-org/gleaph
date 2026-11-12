# Per-graph routing customization: intent subsets, indirection and graph-local intents (2026-11-12)

Question: how can a graph administrator customize the routing query set or contents for one graph
without duplicating the deployment-wide model? The cases: (1) a subset of trained intents, (2) a trained
intent mapped to a graph-local prepared query with a different name/body, (3) graph-local intents never
trained, (4) per-graph slot schemas — with (5) quota, authorization and fail-closed behavior intact.

Status: investigation record (append-only), 2026-11-12. No code, test or configuration was changed.
Claims are marked `[repo]`, `[measured]`, `[derived]`, `[cited]` or `[proposal]`.

## 1. Current constraints

### 1.1 The model

- One deployment-wide INT8 pack (~9 MB wire, **~328 MB loaded heap**) per Router deployment; per-graph
  copies were rejected in ADR 0111 (fixed cost, no isolation). `route_options` is a **pack-level list
  of six labels**: five demo intents plus `none-of-these` (`glev/experiments/routing_schema.py`). `[repo]`
- **Intent identity is positional, not textual.** `forward_question` builds `[bos] + question + one
  <mask> per route option` and softmaxes a shared scorer's logits at those positions
  (`glev-infer/src/model.rs`). The shipped student takes no option text (`marker_pos` +
  `type_emb(qtype)`); the "6 inline options" shape in the 2026-11-10 note is the *teacher*. So
  **renaming a non-abstain intent is free; reordering or adding one moves positions and needs retraining.**
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
- Prepared queries are per graph and the metadata schema is typed: name, description, kind,
  parameters (`SemanticType`, `not_null`), result schema, sorts; execution is **default deny**
  except public-or-explicit grants (`prepared.rs`, `rbac.rs`). `[repo]`
  Caveat: a typed parameter schema does not mean the demo registrations populate it. The demo
  prepared sources declare no `@param` documentation and the catalog fixtures carry empty parameter
  lists, so a profile validator must not assume required parameters are already known; it needs the
  registration/codegen path to populate them or must read the GQL contract. `[repo]`
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

### 1.4 Governance and catalog scoping (blocks "graph admin edits it" today)

- Prepared queries are keyed by **name alone** (`PreparedPlanKey { name }`, MemoryId 8,
  `prepared_catalog.rs`); each record carries a `graph_id` but the key does not, so two graphs cannot
  register the same query name. ADR 0061 records the resulting wrong-graph read as acceptable only for
  name-distinct deployments. `[repo]`
- `prepare` takes no graph argument and is authorized by the **global** `PREPARE_REGISTER` capability,
  not graph ownership; graph ownership roots *execution* authority only. Inference enablement is
  likewise controller-only (ADR 0111). `[repo]`
- Consequences: a per-graph profile can be data only if some actor may write it — "the graph owner
  edits it" needs a graph-scoped capability or a platform-managed profile — and intent labels cannot
  equal graph-local query names across graphs unless ADR 0061's key becomes `(GraphId, name)`.

### 1.5 IC envelope that bounds the design

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

- **Subsetting changes abstention.** Renormalizing over a subset gives `p'(y) = p(y)/Σ_{s∈S} p(s)`,
  hence `p'(none) ≥ p(none)`: with a fixed threshold every subset abstains more. Gating on
  `argmax ∉ S` instead leaves `p(none)` untouched and just refuses the question. `[derived]`
- **Mask the six logits; do not shorten, and mask once after fusion.** Marker count equals
  `route_options.len()` and the encoder is bidirectional, while window probabilities combine by
  element-wise max with `none` averaged (`forward_question`). `[derived]`
- **A profile is not an authorization boundary.** Authority is checked on the resolved prepared
  operation against grants, so a client that ignores the profile cannot escalate; it is policy, not a
  second ACL. `[derived]`

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

Decoupling "which action" from "what the action does" is standard: MCP tools are a **name plus an
input schema** with caller-side invocation ([MCP][mcp]); `ic-llm` branches on `function.name`
([ic-llm][icllm]); Azure CLU orchestration selects a child project by router intent ([orchestration][orch]).

Gleaph already has both halves: stable labels from the pack, and a per-graph prepared catalog with
typed parameters and grants. The mapping is one bounded table lookup per graph — no model or
calibration change. The only loss is coverage: an unmapped winning intent abstains, the correct
behavior when the graph has no such query. O1 answers (1), (2), (4), (5), not (3). `[proposal]`

### O2 — masked softmax

Renormalizing a subset is the exact conditional `p(y | x, y ∈ S)` only if `p` is calibrated for that
population. Calibration parameters are fitted to a distribution: temperature scaling is a post-hoc fit
([Guo et al.][cal]), and in-domain calibration does not guarantee out-of-domain calibration ([Desai & Durrett][pretrained-cal]).
OpenMax makes the same point for a closed-set softmax with no "unknown" mass ([Bendale & Boult][openmax]).

Here `none-of-these` *is* the unknown class, so renormalization moves exactly the quantity the abstain
policy thresholds, and those thresholds were fitted on the full six-option distribution across four
length/CJK regimes; the 93.3% OOS refusal and 91.1% coverage are full-set numbers. `[derived]` `[measured: full set only]`

Verdict: unmeasured; it needs a subset bench and per-profile thresholds.

### O3 — per-graph adapters or heads

LoRA injects low-rank matrices into a frozen base with no added inference latency ([Hu et al.][lora]).
Serving many adapters is a systems problem — S-LoRA needs a paged memory pool and heterogeneous
batching on a GPU ([Sheng et al.][slora]). A canister has one update thread, a 4 GiB heap and a 40 B
budget, and adapters need chunked install, versioning and rollback. The blocker is not only memory but
**data**: a graph administrator has a catalog and questions, not labelled intents. `[derived]`

Verdict: reject for now; revisit if per-graph labelled data becomes a product.

### O4 — reserved graph-local slots

With no textual option input, a reserved slot cannot learn what a graph means by it; *k* is fixed at
training time and the mapping creates no discrimination. `[derived]`

Verdict: reject. If a future model consumes descriptions, O5 subsumes it.

### O5 — description / embedding semantic router

The reference implementation is direct prior art: a route is a **name plus utterances**, an encoder
embeds them, and `fit()` calibrates a threshold from labelled queries including `None`
([semantic-router][sr]). Fixed sentence-encoder detectors are cheap ([Casanueva et al.][use]).

The repository's measured evidence is **negative for description-driven routing here**: "use when"
descriptions lifted dev top-1 from 72.5% to 82.5% and dropped test from 75.9% to 68.8%; rewriting them
moved 40-item slices by up to 45 points and broke other intents; `catalog.json` is a rejected prototype
(`demo/knowledge/eval/routing/README.md` §Measured results). `[measured]`

Structurally the pack ignores descriptions (§1.1), so O5 is a **second model** with an encoder and
per-graph calibration data — and a graph administrator has no `None`-labelled queries. Verdict: defer.

### O6 — LLM-based routing

`ic-llm` is a gateway to off-chain AI workers: the model is a per-call string, every `send()` attaches
100 B cycles, and its workflow is "LLM returns a tool call, your code executes it" ([ic-llm][icllm]). The
repository's IC reality check records that this path is off-chain, not consensus-replicated and not
confidential (`2026-11-09-natural-language-query-research.md` §5.1). Routing through it would be
non-deterministic across replicas — incompatible with the Router's fail-closed, quota-charged shape.
`[repo]` `[derived]` Verdict: reject for the request path; usable only as an offline authoring aid.

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

1. **Pursue O1 now; hold O2 until measured.** Add a `GraphRoutingProfile` beside the prepared catalog:
   `intents: { trained_label → LocalBinding }` with `LocalBinding { operation, slots: { tag → parameter } }`
   and `unmapped_intent: Abstain`. Resolution is `label → binding → operation`; an absent label abstains,
   and no renormalization keeps the measured thresholds valid.
   Before choosing the profile's storage/editing surface, decide the governance fork in §1.4:
   either the profile is **platform-managed and graph-owner selectable** (no new authorization), or
   prepared registration becomes **graph-scoped** (`(GraphId, name)` key plus a graph-owner
   `PREPARE_REGISTER` path), which is the only way a graph owner can truly author its own catalog.
2. **Validate when the profile is written, not per request.** A profile edit (same `PREPARE_REGISTER`
   privilege as `prepare`) must reject: a missing operation; a tag not in `slot_tag_names`; a required
   parameter neither mapped nor defaulted; two intents claiming one operation with incompatible maps.
   Fetch `route_options` and slot names through the Router at write time.
3. **Keep authority where it is.** After resolution the Router still calls `authorize_prepared_execute`
   on the graph-local operation and still charges quota in the existing order. The profile is policy,
   not an ACL, and must not be documented as one.
4. **Delete the duplicated per-op tables** (`declared_tags`, `bind_spans`, CLI `execution_plan`) so
   tag→parameter binding has one owner; otherwise a graph-local op is data-expressible but unserviceable.
5. **Reject O3, O4, O6, O7; defer O5 behind one experiment.**
6. **Measure O2 before considering it.** On the 400-item test split (five intents, 20% OOS), evaluate
   every non-empty proper subset under both policies (gate vs renormalize): conditional top-1, abstain
   rate and OOS refusal per length/CJK regime, using `demo/knowledge/scripts/routing-bench/`. `[proposal]`

End state: the **pack owns intent identity and tags**, the **graph owns the mapping, enabled subset and
prepared bodies**, the **Router owns enablement, quota and authorization**. Requirement (3) stays out of
scope until a model can read descriptions or a graph can supply labels.

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
- If the Router returns resolved bindings, `WIRE_VERSION` must bump (currently 2, ADR 0113) and the SDK's
  abstain policy moves or is mirrored; which side owns policy is unresolved.
- Does a per-profile threshold compose with the length/CJK policy or replace it? Two calibration
  dimensions on 400 test items is likely underpowered.
- Demo docs are stale: `test.jsonl` has five intents (64 positives each) and `prepared/` has five files,
  while the routing README still says "four prepared queries" and "720 (180 × 4)". `[repo]`

## 8. Sources

Primary (fetched 2026-11-12): [ICP resource limits][rl]; [MCP Tools specification][mcp];
[ic-llm README][icllm]; [Azure CLU orchestration workflow][orch]; [Semantic Router][sr];
[Guo et al., calibration][cal]; [Desai & Durrett][pretrained-cal]; [Bendale & Boult][openmax];
[Casanueva et al.][use]; [Hu et al.][lora]; [Sheng et al.][slora].

Repository: ADRs 0061/0111/0112/0113; `design/investigations/2026-11-10-canister-inference-optimization-research.md`,
`2026-11-10-model20-canister-inference-feasibility.md`, `2026-11-09-natural-language-query-research.md` §5.1–5.2;
`crates/router/src/{prepared.rs,rbac.rs,api/inference.rs,facade/stable/{memory.rs,prepared_catalog.rs}}`,
`crates/gleaph-routing-wire/src/lib.rs`, `crates/gleaph-routing-canister/src/lib.rs`,
`sdk/client/rust/src/routing/{client.rs,slots.rs}`, `crates/cli/src/ask.rs`;
`glev` `experiments/{routing_schema.py,slot_schema.py,qonly_slot_train.py}`, `crates/glev-infer/src/model.rs`;
`demo/knowledge/eval/routing/README.md`, `demo/knowledge/prepared/*.gql`, `demo/knowledge/eval/routing/{test.jsonl,catalog.json}`.

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
