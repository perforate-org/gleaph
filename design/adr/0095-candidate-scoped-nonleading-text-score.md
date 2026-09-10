# 0095. Candidate-scoped non-leading text_score

Date: 2026-09-09
Status: **implemented (plan 0344) — awaiting independent review**
Evidence anchor: 2026-09-10 UTC — planner/text-canister/router libs focused-green,
`non_leading_text_candidate_topk_lifecycle` PocketIC green, candidate canbench arm
2.79M instructions (denser below). Policy-denied-scorer and 100M/16MiB headroom
remain explicitly deferred (see §Residual).

## Context and problem

`crates/gql-planner/src/text_scan.rs:141–246` lowers leading top-k only;
`crates/router/src/gql_text_scan.rs:335–429` rejects non-leading scans. The required query
traverses User → Project → Document and ranks only documents reached by that traversal.
Global TEXT top-100 followed by a graph filter cannot implement this contract.

`text_score` looks like a scalar, but its existing supported uses are index-backed search
lowerings, not a total per-row function (`design/gql/extension-syntax.md`, TEXT_SCORE).
The proposed extension explicitly preserves that distinction: nonmatching/unindexed documents
produce no search hit and are excluded, not assigned zero or NULL to fill LIMIT. General scalar
placement remains unsupported. This is a deliberate, review-required language contract, not
ordinary scalar evaluation disguised as an optimization. Existing leading behavior is unchanged.

## Proposed decision

For one graph, live shard, same-subnet deployment and one top-level non-leading score:

1. Router authorizes the user plan and lowers policies through the existing path. Graph executes
   the entire candidate-producing prefix, including all policy predicates, without the user's
   ranking LIMIT. Candidate membership comes from Graph, never TEXT.
2. Graph returns an internal projection of `ELEMENT_ID(d)` and the already-authorized ordinary
   result columns, using existing plan execution and GQL row wire. Router retains row multiplicity;
   only the TEXT request's document keys are deduplicated. Do not project a whole vertex merely
   to obtain identity: that materializes labels and all properties.
3. A controller-guarded TEXT query accepts bounded candidate keys and returns **all matching
   candidate scores**, using the existing analyzer, posting/tombstone readers and score parts.
   The proposed operation is `search_candidates(query, keys) -> Result<Vec<TextHit>, String>`:
   no request record, cursor, new stable state, or ranking formula. The stored controller is the
   Router in this deployment; the guard is not the provision dictionary-relay guard.
4. Router associates scores with retained prefix rows, drops rows without a hit, sorts by score
   descending then TEXT document key ascending, preserves duplicates and applies the user's
   **row** LIMIT. Integer scores become Float64 exactly. Equal-key duplicate rows retain prefix
   order; the initial projection scope makes those rows identical.
5. Return retained projected values rather than re-fetching them. This uses two sequential remote
   calls (Graph, TEXT), avoids score re-evaluation in Router and eliminates post-ranking hydration
   loss. Any later design using a second Graph hydration must fail on missing/changed identities
   or cardinality loss; it must never label a short answer a complete top-k.

Successful results are complete over the fully obtained authorized prefix and the TEXT index's
visible flushed state. No atomic snapshot across canisters, read-your-writes, certification, or
fresh-property/score snapshot equivalence is promised. All steps are read-only; there is no
irreversible mutation or durable callback state to reconcile.

## Bounds and failure ownership

The implementation plan specifies numeric admission limits and mandatory pre-implementation
measurement gates. Router owns prefix rows and bytes; TEXT owns the distinct candidate API cap
and query validation. Over-limit, incomplete prefix, malformed/out-of-set/duplicate hits, decode,
transport, readiness and authorization errors fail the query. There is no successful partial-window
fallback, silent clamp, paging service or budget-exhaustion success.

The existing unguarded `search(query,k)` remains an acknowledged pre-existing information surface,
not proof that a more powerful candidate-membership oracle may be public. The new operation must
reject anonymous, external and dictionary-relay callers before inspecting keys. General hardening
of existing TEXT endpoints is separate; do not claim this slice makes the whole canister private.

## Existing architecture and alternatives

- **Minimum change, selected:** existing Graph projection/wire, Router orchestration and retained
  projected rows, TEXT candidate restriction. Variable projection bytes require a strict cap.
- **Moderate change:** identity-only prefix, candidate search, then seed-based Graph projection.
  Smaller prefix payload, but a third remote call, strict identity/cardinality checks and a second
  observation of canonical state. Not selected as a hidden fallback when retained bytes exceed cap.
- **Larger redesign:** Graph pauses execution at a remote scoring operator or persistent paginated
  candidate sessions. Adds orchestration/persistence boundaries without a demonstrated need.
- **Rejected:** global-top-k post-filter; ADR0092 deepening unchanged. ADR0092 intentionally counts
  authorization survival independently of prefix joins and preserves vector join multiplicity
  without a final row cap. Neither its sparsity stop nor receipt establishes this candidate set.

The TEXT kernel orders equal scores by docid, while the Router contract orders by key. Returning
all bounded candidate hits avoids losing equal-score key winners at an intermediate k boundary.
Existing scoring is `Σ(WEIGHT_BASE + capped_tf)` over deduplicated analyzed query terms
(`text-canister/src/state.rs:1549–1632`), not BM25 and not candidate-normalized statistics.

## Required axes and consequences

- Encapsulation: Graph owns traversal/projection; TEXT owns keys/postings/scoring; no property export
  for Router re-analysis and no raw docid knowledge in Graph.
- Separation: Router composes existing results; portable GQL crates receive no ICP limits or policy
  rules. Existing TextScan placement can represent the boundary without a new persistent schema.
- Invariants: complete prefix before scoring; request-only dedup; exact row LIMIT; candidate-only
  hits; explicit failure on all incomplete paths.
- Consistency: canonical Graph and flushed derived TEXT state retain their existing distinct roles;
  retained projection avoids an additional Graph read, not all cross-canister skew.
- Fitness: bounded exact candidate matching is sufficient for the concrete query; arbitrary
  projections, nested/multiple search, aggregates, DISTINCT, OFFSET, extra sort keys and fan-out
  are out of scope. The cost is materializing bounded prefix rows before LIMIT.

## Migration and documentation

No stable-layout changes, migration, compatibility decoder, new dependency or new module is planned.
The candidate query adds a callable Candid method but reuses the hit result shape. Ship compatible
Router/TEXT artifacts together; do not add a fallback for an old TEXT canister missing the method.
On implementation, synchronize `design/index/text-index.md` and
`design/gql/extension-syntax.md` (including capability tables and limitations). They are deliberately
unchanged by this proposal. ADR0034/ADR0092 vector semantics are not superseded.

## Review gates

Approve explicitly: search-hit rather than total-scalar semantics; retained projection over third-call
hydration; conservative caps and preflight measurements; stored-controller access for candidate reads.
The draft implementation checklist is `plans/0344-nonleading-text-score-candidate-topk.md` (ignored
local planning artifact, not an implemented capability). No independent approval is recorded.
