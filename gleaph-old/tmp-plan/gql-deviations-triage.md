# GQL Deviations Triage

Classification of all Gleaph–GQL standard deviations by actionability.

---

## Category A: Should Fix — No Benefit to Deviating

Deviations where conforming to the standard costs little and the current
behavior is purely a limitation, not a design choice.

| ID | Issue | Effort | Status | Notes |
|---|---|---|---|---|
| S1 | YIELD projection not enforced | Low | **Done** | `SetOp::Next(Option<Vec<String>>)` — YIELD columns filter seed bindings in `execute_next_pipeline` |
| S2 | BFS ignores label expressions on var-length paths | Medium | **Done** | `LabelExpr` extracted to `types` crate; `BfsConfig.edge_label_expr` filters during BFS traversal |
| S3 | SHORTEST GROUP requires path variable | Low | **Done** | Dead error guards replaced with `.expect()` — `INTERNAL_PATH_VAR` already guarantees path var exists |
| P1 | Edge property hints not pushed to planner index | Medium | **Done** | `chain_has_literal_edge_props` now also checks `chain.edge.where_clause.is_some()` |
| V2 | CURRENT_TIMESTAMP returns 0 on wasm32 | Low | **Done** | Thread-local `CURRENT_TIME_NANOS` injected via RAII guards in bridge; `temporal_now_nanos()` reads it |
| D1 | USE GRAPH — no transparent routing | Medium | **Done** | `execute_gql` endpoint detects `USE GRAPH name NEXT stmt` and routes via `ic_cdk::call` |
| D2 | CREATE/DROP GRAPH not executable as GQL | Low | **Done** | Unified `execute_gql` async `#[update]` endpoint dispatches CREATE/DROP GRAPH to registry |
| P2 | Cost model not calibrated | Low | Skip | Already calibrated with canbench — no action needed |

**Rationale:** These were incomplete implementations or straightforward
bugs. All actionable items (S1–D2) are now fixed. P2 was already addressed.

---

## Category B: Intentional — Deviating Has Merit

Deviations where Gleaph's approach provides clear value over the standard,
typically better DX, expressiveness, or compatibility with existing
ecosystems.

| ID | Extension | Merit |
|---|---|---|
| F1 | Directed-only graph (`-[e]-` for bidirectional) | Simpler storage model (16B edge entries, no undirected flag). Directed graphs cover the vast majority of IC use cases. `-[e:L]-` bidirectional matching provides equivalent query-level expressiveness. |
| ext | `MERGE ... ON CREATE SET / ON MATCH SET` | Upsert is a fundamental operation. GQL has no equivalent. Cypher compatibility is a plus for migration. |
| ext | `STARTS WITH` / `ENDS WITH` / `CONTAINS` | More readable than `LIKE '%str%'` patterns. Widely expected by developers from Cypher/SQL backgrounds. |
| ext | `ILIKE` | Case-insensitive matching without wrapping in `lower()`. Common PostgreSQL convention. |
| ext | `FOR item IN list` | List iteration with full statement body — more powerful than GQL's `UNWIND`-equivalent. |
| ext | `FINISH` | Useful for fire-and-forget mutations. No GQL counterpart. |
| ext | `FILTER` | Streaming row filter without requiring a full `WITH`/`NEXT` pipeline break. |
| ext | `LET x = e IN body END` | Inline computed bindings reduce verbosity in complex expressions. |
| ext | `VALUE { subquery }` | Scalar subquery as an expression — concise and composable. |
| ext | Hex/octal/binary/scientific literals | Developer convenience for bitmask operations, IoT data, etc. |
| ext | `gleaph_weight(e)` / `gleaph_timestamp(e)` | Expose structural edge attributes without shadowing user property names. |
| ext | `PERCENTILE_CONT/DISC` / `STRING_AGG` | Standard SQL aggregates that GQL omits. Widely expected. |

**Rationale:** These are either Gleaph-specific features with no GQL
counterpart, or ergonomic improvements that complement the standard. They
should be kept and documented as extensions.

---

## Category C: IC-Incompatible — Cannot Implement

Deviations rooted in the Internet Computer's execution model that cannot
be resolved without fundamental platform changes.

| ID | GQL Feature | IC Constraint |
|---|---|---|
| F2 | Session management (§7) | IC calls are stateless. There is no persistent session between query/update calls. |
| F3 | Transaction management (§8) | Each update call is atomic. Multi-call `BEGIN`/`COMMIT`/`ROLLBACK` is impossible without a session layer. |

**Rationale:** These require mutable server-side session state across
calls, which the IC does not provide. Documenting them as platform
limitations is sufficient.

---

## Category D: Deferred — Low Priority / High Cost

Deviations acknowledged but deferred due to high implementation cost
relative to demand.

| ID | Feature | Cost | Notes |
|---|---|---|---|
| S4 | Parenthesized sub-path patterns (T2.7) | High | Major parser grammar rework; niche use case |
| S5 | KEEP clause (§16.4) | Medium | Requires path-variable tracking changes; rarely requested |
| V1 | Structured temporal types (DATE/DURATION) | High | Requires new `Value` variants, literal parsing, arithmetic operators, formatting |
| V3 | Byte string type (BYTES/BINARY) | Medium | Requires new `Value::Bytes` variant and serialization support |
| V4 | Full static type system (T6.7) | Very High | Type declarations, union types, property constraints — large surface area |
| D3 | GRAPH TYPE / SCHEMA — property types, edge types | High | Depends on V4; schema ↔ IC namespace mapping design needed |

**Rationale:** These are real gaps but have low demand or require
foundational work (e.g. a full type system) that should be driven by
concrete user needs.

---

## Summary Matrix

```
                  Low Effort    Medium Effort    High Effort
                 ───────────   ──────────────   ────────────
Should Fix (A)    ✅ All done   ✅ All done      —
                  (S1,S3,V2,    (S2,P1,D1)
                   D2; P2=skip)

Intentional (B)   — (keep as extensions) —

IC-Blocked (C)    F2,F3         —               —

Deferred (D)      —             S5,V3           S4,V1,V4,D3
```
