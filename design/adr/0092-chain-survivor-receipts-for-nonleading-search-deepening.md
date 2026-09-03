# 0092. Chain-survivor receipts: authorization-aware deepening for NonLeading SEARCH

Date: 2026-08-26
Status: proposed
Last revised: 2026-08-26

## Context

The Router's NonLeading SEARCH deepening loop (`MATCH prefix ... SEARCH b IN (...)`) cannot
currently distinguish **why** candidate rows were lost, and two committed ADRs demand opposite
behavior from the same signal:

- **ADR 0034 Slice 5** governs NonLeading SEARCH semantics: `LIMIT k` bounds the *global vector
  top-k*, the join runs once over the top-k subjects, join multiplicity is preserved, and a
  globally-nearest hit that joins to nothing yields an **empty, non-truncated answer**.
- **ADR 0082 / ADR 0078 §3** require that when the authorization chain (grant coverage, ReBAC
  exists-traversal, policy predicates — the *lowered chain*) filters ANN candidates, the
  deepening loop fetches more candidates until `k` **authorized** rows exist
  ("deepening preserves k").

The only signal the deepening loop observes today is the joined `row_count`, which conflates the
two cases: an authorization-filtered candidate and a sparsely-joining candidate both reduce the
joined row count. Raw ANN hits cannot substitute — the vector canister holds no grants, so its
hits are pre-authorization. History: `49f10d461` applied the Leading deepening contract to
NonLeading (breaking ADR 0034 multiplicity), `a903ca6e1` converged on raw hit count (breaking
ADR 0082 recovery), `dee612131` reverted to joined-row convergence and escalated the conflict.
The consequence today is that `non_leading_search_where_global_top_k_consumes_unlinked_qualifying_
vertex` (ADR 0034) is red while ADR 0082 holds.

The missing concept is an **execution fact**: the graph executor already evaluates the lowered
chain against dispatched candidates; it simply never reports the outcome.

## Decision

1. **Execution receipts extend to search-chain survival** (the ADR 0029 concept family —
   "execution reports facts as receipts"; watermark receipts and Phase 4 mutation receipts are
   the existing members). A graph execution whose plan carries a SEARCH binding returns a
   **chain-survivor receipt**: per shard, how many dispatched search candidates passed the
   lowered authorization/policy chain.

2. **Survivor definition.** A dispatched candidate **survives** iff its searched-binding vertex
   passes the lowered authorization chain at the SEARCH binding (grant coverage, ReBAC exists
   traversal, policy predicates, label membership). Survival is **independent of the prefix
   join**: a survivor may join to zero prefix rows (sparsity) and is still a survivor. The
   executor evaluates the chain per candidate seed **before** prefix matching — this is also the
   efficiency-correct order (a rejected seed never joins) and matches the ADR 0082 intent that
   "the chain filters ANN candidates exactly like a scan".
3. **Wire shape.** The graph→router query result (`GqlQueryResult`, graph-kernel `plan_exec`)
   gains:

   ```text
   search_chain_receipt : opt vec record {
       shard_id        : nat32;
       dispatched      : nat64;   // seeds evaluated this round
       chain_survivors : nat64;   // seeds that passed the lowered chain
   }
   ```

   Per shard (multi-shard graphs aggregate in the Router), per deepening round. `opt` so
   non-SEARCH queries carry `None`. **Ephemeral**: the receipt is in-memory deepening-control
   metadata — never persisted, never stored in stable memory, never exposed to clients, never
   part of certified read guarantees (watermark remains the consistency authority). The vector
   canister is untouched. The client-visible API is unchanged. A SEARCH-bearing execution MUST
   carry the receipt — both sides ship from one tree, so an absent receipt is a bug and fails
   closed at decode rather than degrading the convergence signal.

4. **NonLeading deepening semantics** (replaces the row-count-only rule; cumulative across
   rounds, per shard aggregated to totals):

   - cumulative `chain_survivors` ≥ k → **Converged**: complete answer, join output is **never
     capped** (ADR 0034 Slice 5 multiplicity).
   - cumulative survivors < cumulative dispatched → **authorization loss observed** → deepen
     (fetch the next candidate range) while budget allows.
   - cumulative survivors == cumulative dispatched and survivors < k → **sparsity, no chain
     loss** → stop, **complete, non-truncated**: the global top-k was consumed and the join was
     sparse; an empty or short result is the correct ADR 0034 answer. No further rounds. This
     branch is in practice a corollary of candidate exhaustion (a first round dispatches ≥ k
     candidates, so `survivors == dispatched < k` implies the universe held fewer than k
     candidates) — it exists to pin the **truncated-marker rule**: a fully-consumed candidate
     universe is a complete answer even when fewer than k survivors exist, so exhaustion is
     **non-truncated** for NonLeading receipts; only budget exhaustion is truncated. (Leading
     keeps its existing exhaustion marker; aligning it is a separate question for the ADR 0078
     owner.)
   - budget exhausted with survivors < k → stop, **truncated** (more candidates exist beyond
     the budget).
   - Leading SEARCH: unchanged (rows map 1:1 onto hits; the receipt is not required there).

5. **Single SEARCH binding per query** in v1: the receipt is undefined for plans with multiple
   SEARCH clauses; the planner asserts one. Multi-SEARCH receipts would need per-binding keys —
   deferred until a real query needs them.

## Why all three contracts now hold

- **ADR 0082** (authz loss): round 1 dispatches the globally-nearest hidden vertex →
  `{dispatched: 1, survivors: 0}` → loss observed → deepen → the caller's own doc survives →
  cumulative survivors ≥ k → complete with it. ("Deepening preserves k.")
- **ADR 0034 Slice 5 (sparsity)**: dispatched 1, survivor 1, joined 0 → no loss observed,
  survivors < k → **sparsity stop, complete, empty**. ("Global top-k before join.")
- **ADR 0034 Slice 5 (multiplicity)**: survivor ≥ k → converged → join output uncapped → one
  hit joining two prefix rows returns two rows.

## Consequences

- **graph stream**: executor evaluates the lowered chain per candidate seed at the
  search-binding stage (early seed rejection — dead seeds never join; strictly less work than
  today's post-join filtering) and reports the receipt. The grant evaluation used for the chain
  MUST be the single-source evaluation (authz stream charter / ADR 0089 correction) — the
  receipt inherits the tenancy-specialization consistency rule.
- **vector stream (router deepening)**: receipt-based NonLeading convergence with the
  sparsity/loss distinction; a SEARCH-bearing result without a receipt fails closed at decode;
  `deepening_stop` / Leading branch unchanged.
- **ADR 0034 §Search** is amended: Slice 5's "no deepening" is superseded by receipt-based
  deepening; multiplicity and no-cap are unchanged; the empty-join contract now holds only when
  no chain loss was observed (which is exactly the sparsity case).
- **ADR 0082** unchanged. **ADR 0078 §3** deepening rationale extends verbatim to NonLeading
  with the receipt as the loss signal.
- **Costs**: counting survivors is negligible (the executor already evaluates the chain); the
  sparsity stop *reduces* work versus today's over-deepening. Receipt bytes are bounded by
  shard count.
- **Non-goals**: persistence (receipts are ephemeral), client exposure, multi-SEARCH receipts,
  vector-canister changes, label-stat reporting (kept out; `label_stats_seq` in the watermark
  type is a separate concern).

## Verification plan

- Unit: receipt aggregation (multi-shard sums), convergence matrix (loss / sparsity / converged
  × budget states).
- PocketIC: `adr0082_rebac_exists_traversal` (authz loss → deepen) stays green;
  `adr0034_non_leading_search_where.rs::global_top_k_consumes_unlinked_qualifying_vertex` (the
  escalated OPEN-CONFLICT test) turns green; `adr0034_non_leading_search_join` multiplicity
  stays green; `adr0034_search_where_*` families stay green.
- Cross-replica determinism: same stable data + seeds → identical receipt counts.