# 0116. Query-only general routing

Date: 2026-11-12
Status: accepted (implemented and verified)
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 23:10:00 UTC +0900

## Context

ADR 0114 added the general route as a Router **update** because ADR 0112 charged an
inference-quota for the Router-funded update call, and ADR 0113 installed a tokenizer only to
measure the exact number of question tokens for that charge. ADR 0112's cost model assumed a
replicated inference update.

The query-call model changes that premise: query calls are free (no cycles), execute on a single
replica and are not consensus-backed. The general route is a pure decision over the caller-visible
catalog: it selects a prepared operation, extracts typed slots and abstains. It does not mutate
state and does not execute the prepared query. The expensive, state-changing work is the later
`prepared_query` / `prepared_mutate` update, which remains independently authorized.

Keeping the route on the update path therefore preserved ADR 0112's quota only by paying for a
replicated call and a tokenizer, while a query route would be free and lower-latency.

## Problem

- The route itself needs no consensus and no durable state, yet every call pays update costs.
- The quota protects Router-funded update cycles; query routing has no such cost to protect.
- A query route and an update route cannot coexist coherently: the query path bypasses the quota,
  so the quota is not an effective abuse control.
- The tokenizer exists only to price the quota; removing the quota removes its only consumer.

## Decision

1. **The Router general route is a composite query.** `route_general(graph_name, text)` is an
   `#[query(composite = true)]` method. It builds the same caller-visible context and forwards the
   request to the inference canister's `route_general` query through a bounded, same-subnet
   inter-canister call.
2. **There is no update route and no preview variant.** The update `route_general`, the
   tenant-only `preview_route_general` and their split quota/authority behavior are removed. The
   route is available to callers that pass graph access and the per-graph inference flag; the
   context contains only operations the caller may execute.
3. **No inference quota.** ADR 0112's per-caller/per-graph quota, configuration, admin setters,
   counters and tests are removed. A query cannot mutate quota state, and the route has no
   Router-funded cycle cost to meter.
4. **No tokenizer.** The inference canister no longer installs, stores, reloads or counts the
   HuggingFace tokenizer. The frozen general router uses its own regex tokenizer for features and
   slot extraction; the tokenizer asset was only the quota's token counter (ADR 0113).
5. **Keep a text cap.** The Router and canister keep the 16 KiB question-text cap
   (`MAX_GENERAL_TEXT_BYTES`). It bounds composite-query work; there is no token-based cap.
6. **Execution remains an update.** `prepared_query` / `prepared_mutate` are unchanged and remain
   the authorization, consistency and mutation boundary. A routing result is advisory.
7. **Wire version 4.** The query method surface is unchanged apart from the wire version; the
   tokenizer types and `MAX_QUESTION_TOKENS` are removed. Fresh-install semantics: the Router
   runtime-config shape, canister stable memory and artifacts change without a migration.

## Consequences

- Routing is free and fast for the caller; the Router and inference canister spend no cycles on a
  query route.
- No tokenizer asset (34 MB), tokenizer stable regions, install tooling or token-count plumbing.
- Query responses are single-replica and uncertified. This is accepted because routing neither
  mutates nor executes; the execution call re-authorizes and prepared results carry their own
  consistency contract. Callers that need a consensus-backed routing decision must not treat the
  query result as a proof.
- Abuse protection for query routing is infrastructure-level (ingress/replica limits), not a
  durable quota. Quotas return only if a paid or consensus-verified routing path is introduced.
- The canister has no durable routing state; upgrade behavior is trivial.

## Alternatives

- **Keep the update route and add a query route.** Rejected: the query route bypasses the quota, so
  the update quota is not an abuse control; two routes also duplicated context/validation logic.
- **Keep update-only routing.** Rejected: pays replicated-call cost for a pure read and requires a
  34 MB tokenizer/install flow solely for pricing.
- **Keep a query route plus a paid update route.** Deferred: this is the correct shape only if a
  consensus-verified or paid routing tier is required; no current use case justifies it.
- **Use the frozen router's regex tokens for a free token cap.** Considered; the byte cap plus
  context operation cap is the smaller contract. A regex-token cap can be added later if abuse
  measurements require it.

## Migration

Pre-production fresh-install semantics. The Router runtime config loses `inference_quota`, the
inference canister loses all stable memories, and the artifacts manifest loses the tokenizer asset.
Existing deployments are reinstalled. No compatibility decoder, fallback or legacy endpoint is
kept.

## Design Documentation Impact

- ADR 0112 is superseded: no inference quota exists.
- ADR 0113 is superseded: no tokenizer asset or token-count method exists; the frozen router's
  internal tokenization is unaffected.
- ADR 0114's route is now query-only; the general contract and wire version are updated.
- ADR 0115's graph-owner authority is unchanged; the raw calibration diagnostic remains a query.
- `design/storage/stable-memory-inventory.md`: the inference canister has no stable regions.
- Evidence: [2026-11-12-general-routing-query-only.md](../investigations/2026-11-12-general-routing-query-only.md).

## Validation

- `gleaph-routing-canister --lib`: general route select/extract, OOS/fingerprint rejection, wire 4.
- `gleaph-routing-canister --test general_route_bench`: worst 50 ops/256 tokens cell 262.4M
  instructions, wasm 2,159,129 bytes, memory unchanged at 6,825,173 bytes.
- `gleaph-pocket-ic-tests --test general_router_route`: movie/knowledge/OOS through the Router
  composite query without any tokenizer install; profile threshold still flips abstain; raw
  calibration query still ignores the stored threshold.
- `gleaph-pocket-ic-tests --test general_route_authority`: graph-tenant profile read/write and
  non-tenant rejection.
- `gleaph-router --lib`: 1,156 tests; `gleaph-sdk --features routing`; `gleaph-cli`.

## Review axes

- **Encapsulation:** route decision, execution and mutation remain separate; the Router owns
  context/authorization, the canister owns scoring.
- **Separation of concerns:** no quota/tokenizer in a query-only read path.
- **Invariants:** graph access and inference-enabled checks still precede context construction;
  returned operation must belong to the caller-visible context; execution re-authorizes.
- **Consistency:** one route, one wire shape, one artifact pin; no dual update/query behavior.
- **Fitness for purpose:** free, low-latency routing for the dominant read use case with explicit
  trust limits and a byte-capped workload.
