# Sparse edge-reference tokens

Date: 2026-09-12 13:38:31 UTC +0000
Status: Not selected — sparse-reference candidate not implemented
Source baseline: `95d7910878a5ee8af95af878f2b0df952edd70dc`
Prerequisite evidence: [Bulk edge SET prerequisites](2026-09-12-bulk-edge-set-prerequisites.md)
Tracking: GAP-2026-09-11-004 in [implementation gaps](../implementation-gaps.md)

Web research follow-up, 2026-09-12 14:44:11 UTC +0000:
[Edge-reference alternatives](2026-09-12-edge-reference-alternatives.md) identifies an
occurrence-ID-plus-scoped-lookup design that needs no global ID-to-location directory, but would
increase per-edge storage.

Decision follow-up, 2026-09-12 21:35:13 UTC +0000: the all-edge ID proposal was declined on cost
grounds. [Bounded membership witnesses](2026-09-12-bounded-edge-membership-witnesses.md) examines
fixed-size metadata and logical rather than physical invalidation. Neither it nor this registry
is approved.

Direction follow-up, 2026-09-12 22:55:52 UTC +0000:
[First-execution all-match SET](2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting)
is selected for further design. It does not require this registry; the comparison and gates below
apply only to the unselected pre-admission reference approach.

## Assessment of the unselected candidate

Evaluate this bounded Graph-owned reference registry against the logical-membership alternative,
not as an already preferred layout. A reference selects an edge before chunk admission; a stale
reference is never refreshed for an admitted row. This preserves target selection, not a chunk-wide
transaction snapshot.

The registry state model can distinguish deletion/recreation without a per-edge lifetime ID or a
long-lived lock. It does **not** yet justify implementation: internal LARA maintenance can bypass
Graph's existing observers, bounded canonical resolution needs a work-counting contract, and
capacity-driven invalidation needs an explicit availability decision. No SET implementation plan,
new MemoryId allocation, public endpoint, or independent approval is supplied by this document.

## 1. Proposed state and ownership

One registry per Graph shard:

| Datum | Proposed representation | Owner and invariant |
| --- | --- | --- |
| Reference scope | `EdgeReferenceScope { source: u32, directed_label: u16 }` | Graph canonical forward adjacency; `(source, label)` order. Scope does not depend on a physical bucket descriptor's lifetime. |
| Live registration | `StableBTreeMap<EdgeReferenceScope, EdgeReferenceToken>` | One token shared by all references to an unchanged scope. Missing entry means invalid, never generation zero. |
| Token | Nonzero `u64` newtype | Equality witness, not a secret, permission grant, edge ID, timestamp, or physical location. |
| Allocation ceiling | `StableCell<u64>` | Last token issued by this Graph instance; checked monotonic allocation, never recomputed from live entries. Zero is the initial ceiling, not a valid token. |

Candidate fixed `Storable` encodings are a 6-byte key (`source` LE followed by the normalized
directed storage-label word) and an 8-byte token (LE), both exact-width `Bound::Bounded` values.
The value is one scalar, not a vector/history wrapper. Key ordering is defined by its typed fields,
not lexicographic comparison of LE bytes. Invalid widths or zero token values must not be accepted
as valid persisted registrations. These codecs do not exist at the baseline.

The two collections require **two separate MemoryIds**. No collection is placed on another
collection's region, no reserved historical hole is reused, and no table entry is disguised as a
counter. Exact allocations require the ADR 0007 registry/inventory decision. Existing Graph
bootstrap metadata, projection sequences, index-build scopes, and property sidecars do not own
reference validity and should not be repurposed for it.

The Router retains an immutable, typed target containing Graph/shard placement, source, directed
label, slot, destination, and token. Destination and endpoint liveness remain write-time checks;
they are not substitutes for the token. The existing Graph key/placement contract supplies scope
isolation. Reinstalling Graph while retaining old Router targets is unsupported: activate the new
contract with fresh compatible Router/Graph state. Reopening the same layout preserves both map
and ceiling. Restoring/resetting the ceiling independently of surviving references is forbidden.

The registry owns only reference validity. Canonical adjacency owns edge existence, Router owns
caller authorization/admission, and the existing scalar journals own mutation outcomes.

## 2. Safety argument

Proposed rules:

1. Registration examines canonical adjacency and creates/reuses its scope token in one local,
   no-await message execution. A token is returned only with a successfully resolved exact edge.
2. Every committed change that can replace a selected occurrence or change its slot correspondence
   invalidates the affected scope in the same message execution. Conservative extra invalidation
   is allowed. Unexpected errors after coupled writes trap; returning Rust `Err` is not rollback.
3. An admitted target's scope, slot, destination, and token are immutable through retry.
4. Validation requires `registry[scope] == saved_token`, live endpoints, the expected canonical
   edge/destination, and applicable residual policy checks. Validation through SET and scalar
   completion persistence has no external call or maintenance that can invalidate the binding.
5. A removed token is never reissued, even after eviction, bucket recreation, or restart. Allocation
   exhaustion rejects new registration before any eviction/counter/table write; it does not wrap.
6. Completed scalar results replay before mutable target validation, under the existing caller and
   graph authorization boundary. Stale/missing target validation persists the exact lane's
   write-free outcome in the existing journal, rather than returning an unrecorded transient error.

Consequently, if a reference still validates, there has been no invalidating change since its
registration. That implication is conditional on complete invalidation coverage; it is not a
proof that the baseline implementation has the required hooks.

Example:

```text
resolve E1 -> scope S has token 100; save (S, slot, destination, 100)
delete E1 -> remove S
create E2 -> no old token is restored
resolve S again -> token 101
execute saved target 100 -> stale, no write; persist zero outcome
```

Identical endpoints and reused physical slots do not revive token 100. Capacity eviction has the
same safety effect as removing S; it cannot grant a delayed request permission to write.

## 3. Resolution, admission, and response loss

A proposed Router-guarded **update** RPC performs bounded local resolution and registration. An
ordinary replicated query is no longer sufficient when a missing token must be persisted. This
is a new narrow capability, not a relaxation of the existing read-only-plan/update-mode guard.

Execution order:

1. Router authenticates, checks graph/job/operation permission, resolves fixed endpoint vertex IDs,
   and prepares supported policy predicates through the existing authorization/lowering owner.
2. Router calls Graph to resolve the authored edge selectors. Graph validates mode/placement,
   endpoint liveness, labels, supported predicates, and zero/one/many cardinality. Zero/many,
   unsupported policy, payload overflow, or work exhaustion rejects before registration writes.
3. After all read checks, Graph computes the complete distinct scope set, capacity changes, new
   token count, counter headroom, and exact response size. It then commits only registration state
   and replies, without an await. No graph property is modified by resolution.
4. Router's callback attempts the existing winning-child admission transition. A competing
   admission with different ordered targets/tokens conflicts; it cannot overwrite the winner.
5. Each row reserves its existing owned scalar identity before Graph dispatch. The fingerprint
   includes its complete fixed target. Graph validates and SETs that target, records 0/1, and only
   then performs any derived-index RPC. Router records the committed prefix through its owner.

Registration can succeed remotely while its reply/callback is lost, or while Abort closes Router
admission. This leaves a bounded reusable registration, **not** an edge lock or a pending mutation.
There is no completion/compensation acknowledgement to recover. Before any child was admitted,
a fresh resolution attempt may select again; once admitted, retries use only the saved target.
A late resolution callback still passes the durable admission gate and cannot reopen a closed job.

A registration may be shared by different jobs or several rows in one scope. Completing or aborting
one job therefore does not delete it. No per-job refcount is required because registrations do not
keep jobs alive. Orphan registrations are subject to the bounded replacement policy below. No parent-job backpointer, second lifecycle
journal, or Graph-to-Router query is needed for registry cleanup.

Saved positive and zero outcomes retain the existing unfinished-child GC/expiry protection. A
zero outcome remains zero after a token is replaced or the edge becomes eligible again. Unknown
remote execution still requires the existing fail-closed settlement logic; a missing registration
alone does not prove that an earlier dispatched row never wrote.

## 4. Capacity proposal

Use a fixed maximum of **4,096 retained scopes per shard as an initial evaluation value**, not a
measured production limit. It is unrelated to graph cardinality and is not a limit on the number
of edges. The baseline bulk API's `MAX_ATOMIC_INSERT_OPERATIONS` is 1,024, independently of the
registry capacity. A future resolver's numeric work/byte limits must be specified separately
before implementation.

No TTL, timestamp field, timer sweep, LRU index, per-job refcount, or unbounded history is proposed.
On registration pressure, evict deterministically in scope-key order, excluding **all scopes in
the current resolution request**, not just those already processed. Reuse live registrations for
requested scopes. This prevents a large request from invalidating its own earlier results.

Let `N` be retained entries, `S` the request's distinct scope count, `M` the number missing, and
`K` the capacity. With `N <= K` and `S <= K`, evict `E = max(0, N + M - K)` non-request entries.
Enough victims exist because `N - (S - M) >= E`. A key-order scan finds them after examining at
most `E + S` entries, and `E <= M <= S`. Collect victims before writing, then evict and insert in
one message. Check counter headroom for all `M` tokens before the first write.

This bounds live entry count and victim-search work without a second index. It does not promise
fairness: repeated authorized resolutions can evict other jobs' references, causing write-free
terminal failures even without topology changes. That availability cost must be accepted or the
candidate revised before approval: the registry is not a transparent performance cache.
Authorization and request/cycle admission precede registration; knowing a token replaces neither.

## 5. Invalidation coverage: existing owners and the open gap

The registry belongs in Graph, but its safety depends on complete storage-change notifications.
Extend existing mutation/maintenance ownership, not portable GQL syntax and not a property-index
hook that disappears when no property is indexed.

| Change path | Inspected owner | Required treatment |
| --- | --- | --- |
| Scalar local directed insert | `store/edge_insert.rs::insert_directed_edge_with_inline_property_bytes_and_properties` | Conservatively invalidate the inserted scope; account for any other scopes whose slot correspondence changes during capacity preparation. |
| Ordered edge/mixed clean-slab insert, aggregate-only and location-capture modes | `facade/batch_write.rs::try_insert_batch_edges_clean_slab_with_mode_and_intents` | Invalidation must surround canonical orientation commit independently of initial sidecar presence. Scalar fallback hooks alone do not cover this path. |
| Direct edge delete | `store/adjacency.rs::commit_delete_edge_by_handle` | Invalidate the deleted canonical scope with the removal; process returned slot moves. |
| Timer, post-insert, finalize and delete maintenance | `store/maintenance.rs::run_maintenance_best_effort`; `GraphSidecarMoveObserver` | Invalidate every moved canonical forward scope, including sidecar-free/unindexed edges. Reverse-only moves do not change a directed forward reference. |
| Resumable vertex purge | `GraphDeleteEdgeObserver::on_delete_edge`; `store/vertex_delete.rs` | Invalidate canonical removed scopes. Immediate endpoint tombstone checks reject references before their incident edges are physically purged; no whole-job scan is needed. |
| Inline/sidecar property-only SET | `store/edge_profiles.rs`; property store | Preserve registrations when edge slot correspondence is unchanged. Property-byte allocation or rewriting a bucket descriptor is not by itself occurrence replacement. |
| Reverse-only repair | `facade/derived_state/reverse_adjacency.rs` | Preserve forward-reference validity if only the derived reverse orientation changes. |
| Slab/tree/bypass transitions and synchronous internal drains | LARA insert/materialize/maintenance owners | Must either preserve logical occurrence-to-slot mapping or emit complete invalidation/move information; coverage is not established. |

**Concrete gap:** `ic-stable-lara/src/labeled/bidirectional/deferred.rs`:
`drain_materialize_inline_property_stream_for` and `drain_sibling_work_items` call
`self.maintenance(...)`. That entry point installs `NoopEdgeSlotMoveObserver` and
`NoopDeleteEdgeObserver`. The sibling drain can process unrelated queued work. A Graph callback
only at its explicit maintenance call, or invalidation only for the authored insert's source,
is therefore not a demonstrated complete observer path. This is a blocker for this candidate's
proof, not a new runtime reproduction of corruption in the existing product.

Prefer extending propagation of the existing move/delete observers through internal drains before
inventing a new event log. The exact interface change and all layout-transition cases need an
owning-layer audit/probe. Do not compensate by silently adding a second global invalidation scheme.

Two tempting but insufficient hooks are explicitly rejected:

- `GraphStore::with_graph_mut`: LARA also mutates through shared `&self` methods, and Graph's
  maintenance uses `GRAPH.with_borrow`; this wrapper is not a complete mutation boundary.
- Sidecar cleanup/initial-sidecar installation: property cleanup is not always edge deletion, and
  `commit_ordered_edge_batch_initial_sidecars` returns immediately for property-free inserts.

## 6. Storage and performance accounting

Logical registry payload is `14 * K + 8` bytes at full capacity: 6-byte scope, 8-byte token, plus
one 8-byte ceiling. At K=4,096 that is **57,352 bytes**, before B-tree nodes, allocator headers,
slack, memory-manager extents, and Router's retained target/envelope copies. It is not the total
stable-memory allocation.

The Graph memory manager in `facade/stable/memory.rs` defaults small unlisted regions to **four
64-KiB pages**. Under that unchanged policy, initializing two separate small regions has a combined
bucket-rounded allocation floor of **512 KiB**. This is source-derived arithmetic, not a measured
new-store benchmark. The existing memory manager is shared; no second manager is proposed.
Deleting registrations does not promise shrinking the canister's allocated stable memory. Repeated
fill/evict/refill high-water behavior must be measured.

For comparison, widening the shared 21-byte `LabeledVertex` format by a u64 would add 8 logical
bytes per orientation row, nominally 16 per logical vertex across two orientations, before capacity
slack. Sparse registration avoids that all-vertex change but is not necessarily smaller on a small
graph. A shard-wide epoch has less state but a much broader invalidation domain. Lifetime edge IDs
would support movement-surviving identity at the cost of allocation, record preservation, and
lookup. A separate ID-to-location directory is optional: the linked alternatives investigation
examines bounded source/label lookup instead.

New registration performs counter/table writes; an existing registration need not write. Relevant
topology writes incur registry lookups/removals even for callers not using bulk SET. An empty-store
fast path may use the stable collection's canonical length, not a heap-only "enabled" flag.
No throughput, constant-time, or storage-saving ratio is claimed without measurement.

## 7. Validation and decision checklist

No Rust test, build, PocketIC run, benchmark, or prototype was executed for this design.
The prerequisite-probe and eventual SET implementation plans should cover these contracts;
this list is not evidence that the proposed behavior already exists or passes:

- Registry owner tests: same-scope reuse; distinct scopes; delete/reinsert at identical endpoints;
  eviction/reissue; empty registry with nonzero retained ceiling; reopen; overflow-before-write;
  all-request-scope protection at capacity; failed resolution leaves registry/counter unchanged.
- Graph mutation tests: scalar and batch insert, no sidecars/indexes, delete, slot reuse, move,
  purge, tree/slab/bypass transitions, and a synchronous sibling drain affecting a different scope.
  Property-only SET and reverse-only repair are positive non-invalidation controls.
- Exact-input execution tests: stale target persists 0; live target applies 1; same-key replay of
  both precedes new target reads; no scan fallback; non-tenant residual policy enforcement.
- Router owner tests: concurrent target/token admission, unstarted suffix, Abort between resolve
  and admission, no token refresh after admission, and retained positive/zero outcomes past TTL.
- One focused PocketIC lifecycle using an existing federation where feasible: real update RPC,
  registration reply loss, canonical reply loss, deletion/recreation, upgrade/reopen, retention,
  and Abort. Fixture constructors/installs and exact commands must be specified in the later plan.
- Focused canbench: empty/hit/miss/invalidate registry, capacity replacement, real topology writes
  with the registry empty/populated, and fill/evict/refill allocation high water. Keep setup and
  identity assertions outside measurement. Compare with shard-wide/per-vertex candidates only
  under the same observable reference contract.

Before approval, close observer coverage, choose numeric resolver work/response limits and their
counting owner (a live-result LIMIT does not bound tombstone scanning), specify exact-input policy
shape/error encoding, accept or revise capacity-driven invalidation, and verify actual allocation.
Then obtain an ADR-level decision and synchronize ADR 0007, ADR 0057, the plan format, and stable
inventory in the implementation patch. No existing design is changed to claim this is implemented.

Architectural assessment: Graph encapsulates validity; Router keeps workflow/authorization;
canonical adjacency remains the only edge source; outcomes stay in existing journals. The new map
is justified only as bounded live-reference validity, not a duplicate adjacency or per-job journal.
The unclosed observer path and eviction availability cost prevent an unconditional approval.

## Source references

All source paths below are relative to `crates/graph/src/` unless otherwise stated:

- [Memory allocation and policies](../../crates/graph/src/facade/stable/memory.rs),
  [stable inventory](../storage/stable-memory-inventory.md),
  [ADR 0007](../adr/0007-stable-memory-layout.md).
- [Scalar insert](../../crates/graph/src/facade/store/edge_insert.rs),
  [canonical delete](../../crates/graph/src/facade/store/adjacency.rs),
  [batch commit](../../crates/graph/src/facade/batch_write.rs), especially lines 583–601 and 818–941.
- [Maintenance/observers](../../crates/graph/src/facade/store/maintenance.rs),
  [move/delete callbacks](../../crates/graph/src/facade/store/helpers.rs),
  [sidecar coordination](../../crates/graph/src/facade/store/sidecar.rs),
  [vertex purge](../../crates/graph/src/facade/store/vertex_delete.rs).
- [Inline updates](../../crates/graph/src/facade/store/edge_profiles.rs),
  [reverse repair](../../crates/graph/src/facade/derived_state/reverse_adjacency.rs),
  [Graph lookup wrapper](../../crates/graph/src/facade/store/lookup.rs),
  [Graph scan interface](../../crates/graph/src/facade/store/edge_scan.rs).
- [LARA internal drains and maintenance entry points](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs),
  lines 1829–1886 and 3443–3497;
  [LabeledVertex](../../crates/ic-stable-lara/src/labeled/record.rs), lines 855–880.
- [Router winning admission](../../crates/router/src/facade/store/bulk_load.rs), line 532;
  [bulk API limit](../../crates/bulk-load-api/src/lib.rs), line 19;
  [existing lifecycle contract](../adr/0057-router-operation-api-and-durable-bulk-load.md).
- IC guidance checked 2026-09-12:
  [canister security](https://skills.internetcomputer.org/.well-known/skills/canister-security/SKILL.md),
  [stable memory](https://skills.internetcomputer.org/.well-known/skills/stable-memory/SKILL.md),
  [multi-canister execution](https://skills.internetcomputer.org/.well-known/skills/multi-canister/SKILL.md).
