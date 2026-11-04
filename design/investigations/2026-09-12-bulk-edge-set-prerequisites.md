# Bulk edge SET prerequisites

Date: 2026-09-12 12:35:06 UTC +0000
Status: Investigation complete — pre-admission target design not selected
Source baseline: `95d7910878a5ee8af95af878f2b0df952edd70dc`
Tracking: [GAP-2026-09-11-004](../implementation-gaps.md#gap-2026-09-11-004--bulk-load-edge-property-update-needs-replicated-resolution-and-retry-safe-target-validity)

Design follow-up, 2026-09-12 13:38:31 UTC +0000:
[Sparse edge-reference tokens](2026-09-12-sparse-edge-reference-tokens.md) explores bounded
registration without all-vertex layout growth. It is an unaccepted candidate; internal maintenance
observer coverage, bounded resolution, and capacity-driven invalidation remain decision gates.

Web research follow-up, 2026-09-12 14:44:11 UTC +0000:
[Edge-reference alternatives](2026-09-12-edge-reference-alternatives.md) compares that registry
with occurrence IDs and bounded adjacency lookup. Lifetime IDs need not require a separate
ID-to-location directory; their all-edge storage cost remains a material tradeoff.

Direction follow-up, 2026-09-12 22:55:52 UTC +0000:
[First-execution all-match SET](2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting)
is selected for further design, not implemented. The pre-admission singleton-reference requirements
examined below do not constrain that direction; their source findings remain relevant to physical handles.

Outcome follow-up, 2026-09-22 09:41:27 UTC +0000:
[Durable outcomes and retry ownership](2026-09-22-bulk-edge-set-outcomes.md) develops the selected
all-match direction using existing journals and explicit retirement. Positive and zero success,
definitive rejection, unknown dispatch, counting and Abort are proposed separately from the
unselected singleton design below. Exact wire/layout and whole-row admission remain unimplemented;
the follow-up also records an existing scalar Router/Graph retention mismatch.

## Conclusion for the pre-admission design

Repairing the withdrawn composite-query pre-read is insufficient. Two prerequisites are distinct:

1. Resolve exactly one directed edge from canonical adjacency through a replicated-call-safe API.
2. Preserve the identity of that occurrence, or reliably reject a stale target, between resolution,
   admission, and its first canonical write. A saved physical slot alone does neither.

Existing edge property index queries are callable from update execution at the IC protocol level.
They return physical posting handles, not endpoint-pair lookup results or lifetime-stable edge IDs.
No SET-only implementation plan is ready: the target-validity representation and enforcing write
boundary must be decided first. Bulk edge SET remains absent from the public API and CLI.

This is source/specification research, not a new runtime reproduction, independent review, or
approval of a storage change. Existing tests cited below were inspected, not executed.

## Existing APIs

| Surface | Verified behavior | Limitation for bulk edge SET |
| --- | --- | --- |
| Graph `execute_plan_query` | Router-guarded composite query | Cannot be called from replicated update execution; the withdrawn attempt failed with IC0527 as recorded in the GAP. |
| Graph `execute_plan_update` | Router-guarded update; checks Update mode | A read-only plan is explicitly rejected. Setting a write flag without DML is also rejected. It is not an existing resolution-only route. |
| Property Index `lookup_edge_equal_page` / `lookup_edge_range_page` | Ordinary query methods, guarded for Router/attached shards; Router paging clients exist | Property-keyed physical hits do not prove exactly one canonical edge for `(from, label, to)`. |
| Graph `index_export_page` | Replicated update reading canonical storage | Index-build scope authorizes one frozen puller. This is not a general Router adjacency API; repurposing its scope would change its ownership and admission contract. |
| Graph ordered edge/mixed batch updates | Existing journaled insertion execution | Not an exactly-one resolution-only API or an exact-edge property-update input. |

Primary sources:

- [Graph endpoints](../../crates/graph/src/lib.rs): `execute_plan_query`, `execute_plan_update`,
  ordered batch endpoints, and `index_export_page` (lines 80–125 and 356–367).
- [Graph wire guard](../../crates/graph/src/plan_wire_guard.rs):
  `validate_plan_bundle_write_flag`, `ensure_execution_mode_matches_plans` (lines 24–63).
- [Graph handlers](../../crates/graph/src/canister/handlers.rs): execution mode checks
  (lines 310–324), `execute_plan_impl` (line 1509), scope-bound `index_export_page` (line 1915).
- [Index endpoints](../../crates/graph-index/src/lib.rs) (lines 263–313),
  [Router index client](../../crates/router/src/index_client.rs) (`lookup_edge_equal_page`, line 120),
  [Router paging owner](../../crates/router/src/index_lookup.rs)
  (`collect_edge_equal_hits_paged`, `collect_edge_range_hits_paged`).
- [IC interface specification](https://docs.internetcomputer.org/references/ic-interface-spec/),
  checked 2026-09-12: “Inter-canister calls issued while evaluating an update call can call update
  and query methods (just like update calls).” Its following sentence permits query and composite
  query methods for calls issued during query execution. This establishes protocol legality of
  the ordinary index query route, not a successful bulk-edge runtime test.

`LookupEdgeEqualPageRequest` and `LookupEdgeRangePageRequest` select indexed property values;
`EdgePostingHit` contains only `(shard_id, owner_vertex_id, label_id, slot_index)`. In particular,
there is no destination vertex or occurrence generation in the hit. An edge without the selected
indexed property need not appear. Even converged property postings are not an endpoint-pair
adjacency index. The [Graph edge-index scan](../../crates/graph/src/plan/query/executor/scan/edge_index.rs)
rehydrates a handle from canonical adjacency and checks endpoint visibility. That execution step
cannot be skipped by treating the posting count as canonical edge multiplicity.

Source: [kernel index request/hit types](../../crates/graph-kernel/src/index.rs), lines 953–981
and 1159–1164. Initial endpoint vertex resolution may reuse the vertex index workflow; it does
not establish edge multiplicity or occurrence identity.

## Why the saved edge ID is insufficient

The implemented `GlobalEdgeId` is a 16-byte query-time physical handle:
`(shard_id, owner_vertex_id, label_id, edge_slot_index)`. Adding the label prevents cross-label
collisions; it does not add an occurrence generation. `EdgeSlotIndex` is LARA's tombstone-inclusive,
bucket-local position, not a monotonic allocation ID.

Sources: [GlobalEdgeId](../../crates/graph-kernel/src/federation/global_edge_id.rs), lines 1–30;
[EdgeSlotIndex](../../crates/graph-kernel/src/entry/edge/id.rs);
[EdgeHandle](../../crates/graph/src/facade/store/handle.rs);
[ADR 0090](../adr/0090-edge-element-id-label-attribution.md), label-attribution rationale.
The 16-byte width is verified against code, not inferred from ADR 0005's superseded 12-byte layout.

Two concrete storage behaviors invalidate lifetime identity:

- **Slot reuse:** `unordered_scalar_insert_reuses_interior_slab_tombstone` removes the edge to
  target 20 at slot 1, then inserts target 21 into that same slot. The insertion path also reuses
  tombstones in unordered tree buckets.
  [LARA insert implementation/test](../../crates/ic-stable-lara/src/labeled/graph/insert.rs),
  lines 315 onward and 1346 onward.
- **Slot movement:** Graph maintenance moves canonical edges and relocates their sidecars and
  derived property postings. The old slot is not an enduring alias to the moved occurrence.
  [GraphSidecarMoveObserver](../../crates/graph/src/facade/store/helpers.rs), lines 24–49;
  [relocate_edge_properties_for_move](../../crates/graph/src/facade/store/sidecar.rs), line 36;
  [swap_compaction_rekeys_inline_scalar_index_to_new_slot](../../crates/graph/src/facade/store/tests.rs),
  line 1861, plus the preceding order-preserving compaction test.

Counterexample before a row's first scalar dispatch:

1. Resolve edge E at `(source, label, slot)` and retain that handle in a pending chunk.
2. Delete E and insert E′ into the reusable slot.
3. Dispatch SET by the saved handle. It addresses E′, not E.

Checking the destination would reject the cited different-target example, but not deletion and
reinsertion with the **same endpoints**. Multi-edges are legal, so identical endpoint/property
values cannot establish occurrence identity either. This same-endpoint case is a deduction from
the allocation/identity model, not a newly executed regression.

A scalar mutation journal protects a dispatched mutation's replay. It cannot identify an edge
that was replaced before an unstarted ordinal obtained its scalar reservation. Persisting an
encoded handle or hashing endpoints/properties does not repair that gap.

## Existing recovery owners can be reused, but not unchanged

[ADR 0057](../adr/0057-router-operation-api-and-durable-bulk-load.md) remains the lifecycle contract:
Router owns admission, per-row reservation, prefix settlement, and completed-child receipt replay;
Graph owns canonical SET and the scalar completion journal. No second bulk journal is justified.

The concrete representations are still vertex-specific:

- [SeedBindingsWire](../../crates/graph-kernel/src/plan_exec.rs), line 2029, has
  `mutation_target: Option<SeedVertexBinding>`. The [exact-input Graph guard](../../crates/graph/src/plan_wire_guard.rs)
  admits one labeled vertex SET/REMOVE plan, not an edge-binding plan. Ordinary edge seed entries
  remain physical posting inputs, not durable exact mutation authority.
- [BulkLoadChunkReceiptRecordV1](../../crates/router/src/facade/stable/bulk_load.rs) retains
  `resolved_update_vertex_ids`; it does not represent an edge validity witness. An eventual
  extension needs an explicit typed contract, not edge bytes hidden in a vertex field.
- [admit_bulk_load_update_child](../../crates/router/src/facade/store/bulk_load.rs), line 532,
  compares the complete admitted target list before granting pending dispatch. An edge extension
  must preserve this winning-admission invariant, including previously unreserved suffix rows.
- [bulk_load_update_replay_required](../../crates/router/src/facade/store/idempotency.rs), line 203,
  links scalar retention and expiry to the owning unfinished child. Its data ownership is reusable;
  a new update variant must be checked through admission, compaction, GC, expiry, and completion.
- [classify_update_row / settle_outcomes](../../crates/router/src/bulk_load.rs), lines 491 and 595,
  require durable evidence, not an error-string guess. An edge exact-input lane would need an
  equally explicit eligible-row count and terminal stale-target outcome. Ordinary projected GQL
  row counts are not a substitute for that contract.

These are extension points, not claims that existing enum variants already support edge updates.

## Alternatives before implementation

| Approach | Benefit | Cost / reason not to implement immediately |
| --- | --- | --- |
| Resolution API plus saved physical handle | Smallest apparent API change | Reject: slot reuse/movement permits retargeting. Destination checks still miss same-endpoint replacement. |
| Graph-owned stale-target witness, such as a durable bucket generation | Could fail closed without a lifetime-ID directory; permits rejecting after compaction rather than following movement | Candidate only. Needs a representable persistent generation and complete invalidation at every relevant insert/delete/reorder/maintenance boundary, atomic verification before SET, non-reuse/overflow rules, and a terminal rejection that Abort can settle. Conservative invalidation may reject unrelated writes in the same bucket. |
| Durable logical occurrence ID with physical-location resolution | Could follow an existing edge through movement while distinguishing replacement | Larger storage decision: allocation, move/delete consistency, lookup cost, canonical/reverse ownership, persistence, and index integration. Not justified solely by fixing the read call. |
| Resolve exactly one and SET atomically inside the first per-row Graph update | Avoids retaining a pre-read physical target across calls | Different admission semantics: it selects at first row execution instead of pinning all chunk targets before admission. Requires explicit acceptance of that behavior, zero/many rejection, authorization, and canonical journaling. Not a drop-in preservation of the proposed contract. |

The bounded next decision should compare stale-target rejection against movement-surviving logical
identity. Do not assume a global edge-ID directory is necessary. Conversely, do not approve a
bucket generation until every slot-changing path has an enforcing owner; an omitted path permits
ABA reuse. Existing index catalog epochs, projection watermarks, and storage physical-span pins
are not demonstrated occurrence-validity tokens and must not be repurposed by name alone.

A stateless Router-guarded non-composite Graph read is a plausible resolution API **proposal** if
it performs only bounded local work. A future design must specify scan/work/response caps and
fail closed on exhaustion: seeing one match before a scan cap is not proof of exactly one.
Introducing that API alone would leave the target-validity prerequisite open.

## Required design and validation gates

Before drafting an executable SET plan:

1. Choose whether a still-existing edge must survive slot movement, or whether a durable stale
   rejection is acceptable. Pin endpoint IDs once; do not re-resolve renamed endpoint properties.
2. Specify the Graph-owned target representation, every invalidating/relocating write path,
   persistence and fresh-state boundary, and authorization on the real Router lowering path.
   Keep storage validity out of portable `gleaph-gql` / `gleaph-gql-planner`.
3. Specify the bounded replicated resolution contract, zero/many errors before chunk admission,
   concurrent-admission equality, and a no-await validity-check/canonical-SET boundary. A new
   stateful reservation at resolution would additionally need lost-response cleanup and Abort
   ownership; it cannot be an untracked remote side effect.
4. Define native counterexamples for delete/reinsert with different and identical endpoints,
   compaction/order changes, unstarted suffix rows, and saved positive/zero outcomes. Add actual
   conditional-grant and graph-isolation coverage; an admin-only success does not prove policy
   enforcement. Define how later multiplicity changes affect an already admitted occurrence.
5. Require a focused normal PocketIC lifecycle run for the selected RPC route, response loss,
   pending-child retention beyond scalar TTL, identical retry, and Abort settlement. Reuse an
   existing installation where feasible. Benchmark the selected read/validation cost and any
   added storage-write cost, not only the happy-path SET.

An ADR-level decision is required if target identity, persistent validity metadata, or admission
semantics changes. No new module, map, dependency, lock, memory region, wire variant, or migration
is selected by this investigation. Edge REMOVE/DELETE, public edge-ID selection, cross-shard
edges, and undirected edges are outside the proposed directed SET slice.

## Evidence limits

No Rust changes, builds, tests, PocketIC runs, or benchmarks were performed for this investigation.
The earlier IC0527 result remains historical evidence in the GAP, not a rerun. The ordinary-query
callability finding is backed by the IC specification and endpoint annotations; no new runtime
success is claimed. The active bulk-load contract is unchanged.
