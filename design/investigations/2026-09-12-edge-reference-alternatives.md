# Bulk edge target validity: alternatives from primary sources

Date: 2026-09-12 14:44:11 UTC +0000
Status: Planned alternatives — all-edge ID candidate declined; none implemented
Source baseline: `95d7910878a5ee8af95af878f2b0df952edd70dc`
Context: [prerequisites](2026-09-12-bulk-edge-set-prerequisites.md),
[sparse reference candidate](2026-09-12-sparse-edge-reference-tokens.md)
Tracking: GAP-2026-09-11-004 in [implementation gaps](../implementation-gaps.md)

Decision follow-up, 2026-09-12 21:35:13 UTC +0000: the all-edge ID candidate was declined on
storage/performance grounds. [Bounded membership witnesses](2026-09-12-bounded-edge-membership-witnesses.md)
explores alternatives without widening adjacency records. No measured throughput regression is claimed.

Direction follow-up, 2026-09-12 22:55:52 UTC +0000:
[First-execution all-match SET](2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting)
is selected for further design, without IDs or reference-validity tables. The comparisons below
are not implementation requirements for that direction.

## Conclusion of the alternative comparison

The inspected alternative stores an immutable, non-reused occurrence ID with each edge and locates the admitted occurrence
within its fixed source/label adjacency. A global `ID -> physical slot` directory is **not required**
for this access pattern. Kuzu's inspected relationship-update code demonstrates a bound-node scan
for a saved relationship ID, rather than requiring that directory for this lookup.

This corrects an over-broad cost assumption in the earlier comparison: lifetime identity requires
preserving the ID through movement, but not necessarily maintaining a separate location index.
It does not make identity free. Gleaph stores 4-byte topology records in both orientations;
adding a u64 to the shared record would triple that record's width. A source-bounded scan also
requires a real work cap, and cannot promise constant-time resolution.

**Recommendation:** do not implement the declined all-edge ID proposal. Its identity precision
and lack of registration/eviction management do not justify record growth under the stated cost
constraints. The selected first-execution direction and its storage accounting are linked above;
the fixed-budget membership-witness option is not selected. Exact implementation contracts and
validation remain to be specified.

## 1. What the primary sources establish

### Kuzu: relationship identity need not be its adjacency position

The following source files are pinned to Kuzu revision `89f0263c`, not an unversioned claim about
all Kuzu releases:

- [`RelTable::reserveRelOffsets`][kuzu-header] allocates from `nextRelOffset` by advancing the
  counter. [`RelTable::serialize` / `deserialize`][kuzu-table] persist that counter. The same file's
  `updateRelOffsets` converts local relationship offsets to committed offsets before publishing
  committed storage. This is allocation state, not a per-source CSR slot number.
- [`RelTableData::findMatchingRow`][kuzu-data] takes a bound node and relationship ID, initializes
  a relationship scan for that node, and compares scanned IDs with the requested relationship
  offset. It returns the matching physical row index and scan source. `update` and `delete_` use
  this method. This is direct source evidence for **identity plus scoped search**; it does not
  establish that Kuzu has no other indexes or that its entire lifecycle contract fits Gleaph.
- Kuzu's [node-group storage design][kuzu-layout] separates aligned neighbor-ID and relationship-ID
  columns from CSR list-header offsets. A [first-party storage discussion][kuzu-cost] confirms
  on-disk internal-ID columns store offsets with a common table ID, and reports forward/backward
  ID columns as a substantial storage contributor. Its measurements are not Gleaph benchmarks.

The transferable idea is the lookup organization. Do not import Kuzu's transaction-local IDs,
MVCC behavior, counter overflow assumptions, or temporary-ID publication into a durable Router
reference contract. Gleaph must expose only IDs already committed with canonical edges, preserve
its own non-reuse invariant, and specify exhaustion explicitly.

### SQLite and PostgreSQL: a locator and a non-reused identity are different

[SQLite AUTOINCREMENT][sqlite] records a high-water mark and prevents reuse of automatically
allocated IDs from committed transactions. Default ROWID allocation can reuse deleted IDs.
SQLite explicitly documents the additional CPU/storage/I/O cost. Its guarantee is about automatic
allocation, not arbitrary explicit writes or manual modification of `sqlite_sequence`.

[PostgreSQL's system-column documentation][postgres] says `ctid` changes on update or
`VACUUM FULL` and must not identify logical rows. It also warns that transaction IDs wrap, so
`xmin` is not a perpetual unique identity. These sources support separating location, version,
and lifetime occurrence; they do not establish Gleaph's target-selection timing.

### Slot maps: generation checking solves reuse, not all compaction concerns

[`slotmap`'s own documentation][slotmap] describes `(value, version)` slots and keys that carry
versions. It also states that:

- versions can wrap after 2^31 deletion/insertion cycles at one underlying slot;
- the basic map must retain vacant-slot generation information;
- dense storage introduces indirection and additional per-element/per-slot metadata.

This is useful evidence for ABA prevention, not a reason to add the Rust crate to stable storage.
Gleaph would still need persistent non-wrapping generations, movement rules, and reclamation
semantics. Replacing the slab allocator with a dense slot map is a much broader storage change.

### OCC and reclamation schemes are not free substitutes

[Silo, sections 4.2–4.4][silo], uses a 64-bit last-writer TID in each record and validates observed
versions at commit. Read tracking need not create shared registration entries, but the metadata
cost moves to records and the contract becomes read-version validation. Its 32-byte record-header
figure is not an 8-byte total-overhead claim, and the paper explicitly ignores rare TID wraparound.
An immutable creation ID is narrower: property updates do not create a different edge occurrence.
Neither design alone supplies Gleaph's durable multi-message replay or chunk atomicity.

[Michael's hazard-pointer paper][hazard] demonstrates safe reclamation/ABA handling through
published references. Applying that approach to resumable canister jobs would require durable
reference publication and release/recovery rules. Preventing reclamation also does not by itself
preserve a mutable bucket-local slot during compaction. That is a larger fit problem than a
short-lived in-process read critical section, not evidence that hazard pointers are generally
incorrect or that all reclamation schemes have unbounded memory.

### ICP's built-in canister version is too broad for this row lifecycle

The [IC System API][ic-system] guarantees `canister_version` increments for successful update
execution, replicated queries, callbacks, timers, and other state changes. It also permits
arbitrary increments. This is attractive as a platform-owned coarse fence, but it is not a
canonical-topology revision.

Even accounting for the resolution call itself, ordinary row completion, journal reads, and
callbacks can invalidate a shared pre-admission version for the unstarted suffix. Tolerating an
arbitrary number of intervening increments would no longer establish that no topology change
occurred. A different bounded single-message execution design might use such a fence, but it is
not a useful drop-in replacement for the retained per-row references considered here. No
`observed_version + 1` protocol or liveness guarantee is proposed.

## 2. Gleaph candidate: immutable occurrence ID and source-bounded lookup

Illustrative internal reference, not an approved wire declaration:

```text
(graph, shard, source_vertex, directed_label, destination_vertex, occurrence_id)
```

Graph allocates a nonzero occurrence ID when a logical directed edge is created. The ID remains
with the canonical edge through property updates and physical movement. A replacement receives
a different ID even if its endpoints, label, properties, and reused slot are identical.
A physical slot may be a lookup hint, but must not become mutation authority.

Example:

```text
admit E1: (source A, label L, destination B, occurrence 100)
compact: E1 moves from slot 4 to slot 2, retaining occurrence 100
execute: find occurrence 100 under (A, L), validate, SET that edge

delete E1; create E2 with identical endpoints, occurrence 101
retry an unstarted admitted row for 100: no occurrence 100, no write
replay an already completed row for 100: use its recorded outcome first
```

Searching for the **saved ID** is not fresh predicate selection. Another parallel edge does not
silently become the target. Initial exactly-one selector resolution and write-time validation of
the selected occurrence remain separate checks. This does not preserve an entire graph snapshot
or add serializable/chunk-atomic behavior.

The baseline [Graph handle lookup](../../crates/graph/src/facade/store/lookup.rs) already enumerates
source/label adjacency and compares physical slot numbers. A corresponding occurrence comparison
could use the same owning storage boundary. This observation is not a performance equivalence:
record widths, scanned columns, early termination, and mutation preflight costs differ.

The lookup must bound actual examined storage work, including tombstones, not only emitted live
edges. A complete negative lookup can establish absence; budget exhaustion cannot masquerade as
absence. Final binding to the physical record must occur after any remapping maintenance, with
no await or remapping between final validation and canonical SET plus scalar completion. Exact
policy/liveness checks and the existing journal-first replay/retention rules remain necessary.

## 3. State representability, ownership, and response loss

Nothing at the baseline implements this occurrence-ID candidate:

| State or capability | Baseline limitation | Required change if selected |
| --- | --- | --- |
| Committed occurrence identity | [`Edge`](../../crates/graph-kernel/src/entry/edge.rs) persists only its 4-byte target; [`GlobalEdgeId`](../../crates/graph-kernel/src/federation/global_edge_id.rs) is a physical locator | An immutable intrinsic edge field, preserved by storage rewrites; not a user-mutable property or a physical-handle-keyed sidecar |
| Non-reuse allocation | No edge-occurrence ceiling exists; [`GraphMetadata`](../../crates/graph/src/facade/stable/metadata.rs) owns bootstrap/routing metadata | A Graph-owned durable checked allocator; illustrative `StableCell<u64>` last-issued ceiling, exact 8-byte value, with its own MemoryId if implemented as a new collection; no MemoryId selected |
| Canonical selection read | General Graph plan query is composite; general update execution rejects read-only plans | A narrow Router-guarded replicated canonical resolver; no registration writes are needed for this candidate, so an ordinary query is a protocol-level option |
| Exact-edge input and admitted targets | Existing mutation-target seed and saved update-target list are vertex-specific | Typed edge input, ordered immutable targets, target-bound scalar identity, exact plan guard and outcome handling in their existing owners |
| Movement and mirror preservation | LARA carries topology records and reconstructs orientations through several paths | Preserve one allocated logical occurrence across scalar/batch insertion, compaction, tree/slab/bypass conversion, and derived reverse repair; never mint a new ID merely for movement or repair |

The allocator cannot reconstruct its ceiling from surviving edges, reset when the graph becomes
empty, or wrap. All logical insertions in a batch need allocation headroom checked before coupled
writes. The ID and canonical creation must commit together without an intervening external call.
Unexpected errors after coupled writes must trap, not return a Rust `Err` that leaves partial state.
There is no new per-reference table, registration receipt, lease, timer, or ID-location index in
this candidate. Sharing a logical ID with a derived reverse copy is derived state, not a second
allocator. Widening one shared record type does not prove every reconstruction preserves the ID.

The cross-canister boundaries are:

1. **Canonical insertion already committed:** a resolver reads an existing ID; it never hands out
   a provisional creation ID. Allocation/creation recovery must continue through existing insert
   journals, not a new allocator workflow spanning awaits.
2. **Graph resolution reply -> Router admission callback:** resolution has no new durable Graph
   side effect to compensate. A lost reply leaves no registration. Before winning admission,
   another resolution may select anew; after admission, the complete saved target is immutable.
   A late callback cannot reopen an aborted/closed job or replace another callback's winner.
3. **Router row reservation -> Graph execution:** persist scalar ownership and fingerprint before
   dispatch. An uncertain response is not proof that Graph did not write.
4. **Graph canonical SET + completion -> derived-index calls / Router callback:** save the outcome
   before external work. Lost replies settle through the existing retained journals. Replaying a
   saved zero must not search again after the edge/policy changes. ID absence alone cannot settle
   an earlier unknown dispatch as never written.

This flow adds no need for a resolution-registration pending state because that remote step is
read-only. It does not remove existing mutation/derived-state recovery obligations.

Any record-width change requires fresh compatible Graph storage and Router target contracts under
the pre-production policy. Reopen must retain the allocator. Resetting Graph while Router still
holds old occurrence references is unsupported; no migration, legacy decoder, or mixed activation
is proposed. This investigation does not redefine the public GQL edge element-ID format.

## 4. Honest cost comparison

For a simple u64 field added to the existing shared topology record:

```text
one stored orientation: 4 -> 12 bytes
forward + reverse:       8 -> 24 bytes per logical directed edge
additional live payload: 16 * E bytes for E logical directed edges
```

At one million such edges this is nominally **16,000,000 additional bytes**, before tombstones,
capacity slack, logs, tree storage, allocator metadata, and Router copies. It is not the overall
canister footprint or a measured allocation delta. The shared `Edge` type and forward/reverse
stores are visible in [memory.rs](../../crates/graph/src/facade/stable/memory.rs).

Storing IDs only in canonical forward records could reduce payload, but the shared format does
not supply that saving automatically. Introducing a separate identity column or orientation-
specific representation needs a justified layout decision and preservation audit. Kuzu's column
compression is evidence of a possible tradeoff, not an available Gleaph optimization. Narrower
IDs would also require an explicit lifetime-insertion capacity and fail-closed exhaustion policy;
no smaller width or compression scheme is selected here.

| Alternative | Extra persistent state | Invalidations unrelated to the selected occurrence | Main cost / contract change |
| --- | --- | --- | --- |
| Resolve+SET on first execution | No new reference-validity state; existing mutation journals still required | Not a pre-admission reference | Changes when the target is selected; zero/many and successful outcomes must all replay without reselection |
| Graph-wide topology revision | One non-reused counter | Any covered topology mutation anywhere on the shard | Broad conflicts; still needs a complete mutation boundary |
| Sparse scope tokens | Bounded map + non-reuse ceiling; [existing accounting](2026-09-12-sparse-edge-reference-tokens.md#6-storage-and-performance-accounting) | Same-scope topology changes and capacity eviction | Observer propagation, registration writes, availability/eviction rules |
| Per-slot generation / stable handle directory | Generation history; extra indirection if locations move | Depends on movement policy | Slot lifetime, compaction, generation non-reuse, retained vacant metadata |
| Occurrence ID + scoped lookup | Per-edge intrinsic ID + allocation ceiling | No invalidation merely for another edge's insertion, registry pressure, or movement | Wider records and bounded adjacency search; all creation/rewrite paths preserve identity |

There is no demonstrated option that preserves pre-admission identity, distinguishes identical
replacement, permits immediate slot reuse, needs no persistent historical discriminator, and
always succeeds on an unchanged target. In particular, two histories with identical visible
endpoints/properties/slots but different edge occurrences cannot be distinguished by checking
only those visible fields. This is a conditional indistinguishability argument, not a universal
storage lower-bound theorem or proof that the candidates above are optimal.

## 5. Architectural assessment and decision gates

- **Encapsulation:** Graph should return an exact internal reference, not expose LARA arrays or
  permit Router to infer validity from postings. Graph owns ID allocation and canonical matching.
- **Separation of concerns:** LARA preserves record contents through storage transformations;
  Graph assigns logical identity and performs SET; Router owns authorization/admission/recovery.
  Portable GQL/parser/planner crates acquire no canister validity mechanism.
- **Invariants:** occurrence-ID uniqueness, non-reuse, and preservation replace the sparse
  candidate's complete invalidation-notification obligation. They still require an exhaustive
  creation/reconstruction audit; this is not a claim that the baseline already meets them.
- **Consistency / SSOT:** canonical adjacency stores the identity. No independent location map
  must be synchronized. Existing property postings/sidecars and reverse adjacency still need their
  existing synchronization; this proposal does not repair or waive their observer obligations.
- **Fitness for purpose:** this is smaller than a general lifetime-ID directory, but widening all
  edges solely for an infrequently used bulk operation may be disproportionate. Sparse tokens
  trade storage density for notification complexity and conservative write-free failures.

Before an ADR/implementation decision, compare actual storage high-water and lookup/topology-write
costs, close every ID-preservation path, specify numeric resolver limits, and settle the exact-edge
wire/policy/error contract. Useful bounded probes would cover same-endpoint recreation, movement
without false staleness, reverse reconstruction, property-free ordered batches, allocator reopen
and exhaustion, work-cap versus absence, and retained 0/1 replay. These are proposed checks, not
executed tests. Reuse existing owner tests and a focused lifecycle fixture if implementation is
authorized; a new full E2E installation fleet is not warranted by this research.

The implementation patch, if authorized, must synchronize ADR 0007, ADR 0057, storage inventory,
plan-format, and any edge-identity contract actually changed. The existing implemented contracts
are not rewritten by this research. The sparse design remains a planned comparison candidate,
not a superseded runtime path.

## Evidence limits

Primary web sources and the pinned local baseline were inspected. No Rust code, runtime API,
MemoryId, dependency, build, test, PocketIC execution, benchmark, or prototype was added or run.
Storage figures are source-derived arithmetic. No independent or security approval is claimed.

## External sources

[kuzu-header]: https://github.com/kuzudb/kuzu/blob/89f0263c/src/include/storage/table/rel_table.h
[kuzu-table]: https://github.com/kuzudb/kuzu/blob/89f0263c/src/storage/table/rel_table.cpp
[kuzu-data]: https://github.com/kuzudb/kuzu/blob/89f0263c/src/storage/table/rel_table_data.cpp
[kuzu-layout]: https://github.com/kuzudb/kuzu/issues/1474
[kuzu-cost]: https://github.com/kuzudb/kuzu/issues/5743
[sqlite]: https://www.sqlite.org/autoinc.html
[postgres]: https://www.postgresql.org/docs/18/ddl-system-columns.html
[slotmap]: https://docs.rs/slotmap/latest/slotmap/
[silo]: https://people.csail.mit.edu/stephentu/papers/silo.pdf
[hazard]: https://research.ibm.com/publications/hazard-pointers-safe-memory-reclamation-for-lock-free-objects
[ic-system]: https://docs.internetcomputer.org/references/ic-interface-spec/canister-interface/
