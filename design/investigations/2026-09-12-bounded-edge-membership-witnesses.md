# Bulk edge SET without widening adjacency records

Date: 2026-09-12 21:35:13 UTC +0000
Status: Planned — first-execution all-match direction selected; not implemented
Source baseline: `95d7910878a5ee8af95af878f2b0df952edd70dc`
Tracking: GAP-2026-09-11-004 in [implementation gaps](../implementation-gaps.md)
Context: [prerequisites](2026-09-12-bulk-edge-set-prerequisites.md),
[sparse tokens](2026-09-12-sparse-edge-reference-tokens.md),
[earlier comparison](2026-09-12-edge-reference-alternatives.md)

## Decision boundary

The all-edge occurrence-ID proposal is **declined on storage/performance grounds**. Do not proceed
with record widening or an ID-location directory. The earlier record-size arithmetic was not a
measured throughput regression; no new performance result is claimed here.

Direction selected on 2026-09-12 22:55:52 UTC +0000: preserve multi-edges and resolve/update all
eligible matching edges at each input row's first Graph execution. Do not pin edge occurrences
before admission. Do not add per-edge IDs, reference registries, or membership-revision arrays.
The exact API, bounds, persistent outcome representation and implementation remain unapproved.
Sections 1–6 below compare the unselected witness mechanisms; they are not implementation tasks
for the selected direction.

## Selected direction and storage accounting

For the bounded, same-Graph slice, Graph selects eligible edges, validates limits, applies SET,
and durably records the result within one local execution without an intervening external call.
Zero matching edges is successful with target count zero; multiple matches update all of them.
Exceeding the row's work/mutation/byte bounds rejects before canonical writes, not after a partial
prefix. This is input-row atomicity, not a chunk transaction. Endpoint resolution/pinning and
policy details still need a concrete contract. Graph-side local preflight must not leave selected
physical handles stale through remapping before the write.

First execution may select edges created after Router admission. Once a terminal outcome is
recorded, retry replays it rather than selecting again. Successful zero and deterministic rejection
need durable replay just as positive counts do; a callback loss is not proof of no write. Existing
vertex bulk-update selection semantics remain unchanged.

| Storage category | Selected direction |
| --- | --- |
| Canonical adjacency | No feature-specific bytes added per edge or vertex; the existing 4-byte topology record is unchanged. |
| Lifetime IDs / reference validity | No new identity column, ID-location map, scope registration, generation array, or movement history. |
| Router/Graph recovery records | Bounded records per retained input row, plus job/chunk receipts and bounded dispatch context; not a persisted list of every matched edge. Reuse existing journal owners, with necessary type/codec changes. |
| User properties and indexes | Normal SET storage effects remain: new/larger properties and index postings can grow with the number of affected edges. |
| Derived-index delivery | Existing durable outbox/repair state can grow with outstanding property/index operations. This is persisted state, not just temporary heap. |
| Working memory | Selection/preflight may retain matched edges or prepared effects during one execution. Bound this work explicitly; do not persist that list merely for retry. |

Let R be retained input-row records, C retained chunks, A outstanding derived-index operations,
and H bounded execution workspace. Recovery overhead is intended to scale with R and C (and
bounded per-row payload), while ordinary index-delivery state scales with A; H is transient.
Neither is a new all-edge identity tax. For example, one accepted input row matching 100 edges
records one row outcome with count 100, not 100 edge-identity records. Updating a property on
those 100 edges still has its ordinary per-edge property/index cost.

Source inspection:

- [Graph journal](../../crates/graph/src/facade/stable/label_stats_delta.rs)::GraphMutationJournalEntryV1
  already has `row_count: u64`; changing the numeric count does not require a variable-length
  edge list. This is not proof that ordinary RETURN-row counts equal mutation-target counts.
- [MutationJournalState](../../crates/graph-kernel/src/plan_exec.rs) has only `Incomplete` and
  `Completed`. It does not by itself encode the selected durable rejection contract. Specify the
  required field/variant and codec changes before claiming replay is representable as-is.
- [Router records](../../crates/router/src/facade/stable/label_stats.rs) retain fingerprint,
  ownership, shard dispatch context and outcome metadata; [chunk receipts](../../crates/router/src/facade/stable/bulk_load.rs)
  are a separate job/chunk record. Input-row progress and matched-edge count must remain distinct.
- [Scalar compaction](../../crates/router/src/facade/store/idempotency.rs)::compact_completed_record
  drops heavy resolved tables and shard context while retaining replay identity/outcome. Extend
  the existing retention owner to preserve all edge-row outcomes while unfinished chunks need them;
  do not discard them merely to reduce storage. A stalled job can prolong that retention.
- [Derived-index outbox](../../crates/graph/src/facade/stable/derived_index_outbox.rs) persists
  unacknowledged operations. Bulk success/reply loss must not lose those effects; quarantine or
  failed delivery can prolong their storage lifetime.

Thus additional recovery storage is NOT zero, and an existing 8-byte count field is NOT the total
cost of a request. Exact Router+Graph bytes, any effect on other journal variants, allocator
high-water, and outbox growth require chosen codecs and measurement. Logical GC does not promise
physical shrinking of allocated stable memory. No new reference-validity MemoryId is required by
this direction; existing journal-format changes still need a stable-layout/activation decision.

## 1. Primary-source findings

| Source | Established mechanism | What is transferable, and what is not |
| --- | --- | --- |
| [TL2, section 2, page 6][tl2] | Per-stripe versioned locks live in a separate fixed array; hashing maps locations to stripes. Unlike per-object locks, this works with unmodified data structures. | Bounded shared revision metadata without widening objects. Not its entire STM, global-clock protocol, locking, or reported CPU speedups. |
| [Transactional Boosting, sections 1–3][boosting] | Conflicts can be determined from abstract operations/keys rather than all physical reads and writes. Different set keys can commute despite touching common list structure. | Separate canonical membership changes from representation changes. Do not import its undo operations: remove+reinsert of an equal value is NOT restoration of the same edge occurrence for this task. |
| [ARIES/KVL, sections 1–2][aries] | Key-value locking and physical latching serve different logical/physical consistency purposes; structural tree operations need not act like changes to every key. | A logical selector need not be tied to page/slot movement. Not a proposal to add a lock manager or ARIES recovery. |
| [RingSTM, section 2.2][ring] | Bloom-filter write histories allow bounded metadata, but validation examines intervening writers and ring overflow forces abort/restart. | A bounded invalidation history is possible, but its expiry and scan cost are a poor first choice for indefinitely resumable rows. |
| [IC multi-canister guidance][ic-multi] | Execution before and after an external await is separate atomic message execution; unknown replies require idempotent outcome recovery. | Moving selection and SET into one local message can eliminate their remote gap. It does not remove Router/Graph journals or make a bulk chunk atomic. |

The candidate below is a **Gleaph-specific deduction** from these mechanisms and inspected code.
It is not an algorithm already implemented or benchmarked by those sources for this edge contract.
No new STM library, locking framework, Bloom-filter dependency, or storage engine is proposed.

## 2. Replace physical-movement tracking with logical-membership tracking

For the restricted local directed-edge slice, let:

```text
P = (fixed source vertex ID, normalized directed label, fixed destination vertex ID)
```

P identifies a group of parallel edges, not an occurrence. Initially require **exactly one
canonical live edge in P**, independently of property/policy filtering. If two canonical edges
exist but only one is visible to the caller, that is insufficient for this proof. Authorization
still applies; raw canonical singleton cardinality is an additional prerequisite, not permission.

A logical membership change means adding or removing an occurrence in P, including replacing one
with another. Count mutation events, **not net degree changes**: delete E1 followed by insert E2
must invalidate a reference even if cardinality, endpoints, and properties finish unchanged.
Changing an edge's endpoints/label, if supported by another path, changes both old and new groups.

A pure compaction, slab/tree conversion, or reverse-only repair does not change P's canonical
membership. Property-only SET likewise does not create a different occurrence.

### Conditional continuity argument

If P contained exactly one occurrence E1 at resolution and no membership change to P occurred
since, its sole occurrence is still E1. Thus a current canonical edge at the saved slot whose
source/label/destination match P must be E1. If the saved slot is missing or contains a different
edge, reject the reference without writing.

This permits a deliberately small first variant:

1. Resolve a canonical singleton P and retain its slot plus membership witness.
2. At execution, validate the witness and endpoint eligibility.
3. Read the saved slot and check the complete P plus applicable residual policies.
4. If it matches, SET that edge and journal the result; otherwise persist a write-free rejection.

**Do not search for a new slot or refresh the witness in this first variant.** A moved edge may
therefore be conservatively rejected. The benefit is avoiding both a required movement observer
and an added occurrence-ID search. Relocation-aware lookup is a different performance decision,
not an automatic fallback. A stale reference does not imply that the logical edge ceased to exist.

The argument fails if initial uniqueness is policy-filtered, any membership-changing writer is
missed, IDs can be resurrected without detection, or counters can wrap/reset. Those are gates,
not assumed implementation properties. This is not snapshot isolation or a lifetime edge-ID API.

## 3. Fixed-size backing instead of a registration map

Illustrative state, not a selected layout:

- One Graph-owned `StableVec<u64>` of a fixed N counters, initialized once, never resized or cleared.
  Reuse the library's fixed-width u64 `Storable`; one collection owns one new MemoryId, not shared
  with another collection. No MemoryId is selected or allocated here.
- A frozen hash/key-encoding contract maps P to a stripe. Do not use process-randomized hashing or
  an unstable compiler/library default as a persistent reference contract.
- Every relevant canonical insertion/deletion advances the covering counter. Check overflow before
  coupled writes and never wrap. A batch can deduplicate stripes within one no-await canonical
  commit, but cannot erase delete+insert events merely because their net count is zero.
- Resolution reads the counter with canonical adjacency in one no-await execution. Execution
  requires equality with the saved counter. Zero is a valid initialized counter value here, unlike
  the earlier registry's nonzero allocated tokens.

The full admitted target includes graph/shard, P, saved slot, and observed revision values. Graph
owns membership and validity; Router retains the exact result and fingerprints it. This is a
validity witness, not a secret or an authorization grant.

There is no registration RPC side effect, token allocator, capacity eviction, TTL, per-job refcount,
cleanup acknowledgement, or invalidation history. Ordinary reads, property-only writes, journal
polls, and membership-preserving maintenance need not update the array. Canonical inserts/deletes
DO incur additional metadata work, including those unrelated to bulk SET.

For example, N=16,384 u64 counters is **131,072 bytes (128 KiB) of logical counter payload**. Under
Graph's inspected default four-64-KiB-page allocation policy, one such new small region has a
**256 KiB bucket-rounded allocation floor**, before any additional manager growth. This is a
source-derived estimate, not measured canister allocation. N is an evaluation value, not approved.
Router's retained witness bytes and collection headers are additional; edge/vertex rows stay intact.

### Hash aliasing is safe only in the conservative direction

An unrelated P' can advance the same stripe, causing rejection of an unchanged E1. It must never
cause acceptance of a replaced E1. Complete writer coverage, persistent monotonicity, and unchanged
hash mapping provide the latter implication; hash collision resistance is not required for safety.
An adversary able to choose colliding keys can still harm availability.

Single-hash aliasing can be **too costly for bulk jobs**, even when the per-row figure looks small.
For N stripes and D distinct unrelated changed keys with independent uniform hashes, the single-
reference false-conflict probability is `1 - (1 - 1/N)^D`. Repeated changes to one key are not D
independent hashes. At N=16,384 and D=100 this is about 0.609% per reference. An illustrative model
of 1,024 independently exposed targets makes at least one false conflict about 99.8%. These are
synthetic probability calculations, NOT measured job failure rates; shared keys and changing
exposure intervals alter the result. They are sufficient to reject an unconditional claim that
one small striped array makes false conflicts negligible.

A possible refinement uses H fixed hashes into the same array: every membership change advances
ALL cells assigned to P; a saved witness can validate if **at least one** of its cells is unchanged.
Why: a real change to P necessarily changes every one of its cells, whereas unrelated keys may
change only a subset. Repeated hash positions provide no extra protection. This is a derived
multi-hash witness, not a claim that standard TL2 implements this acceptance rule.

It trades H counter reads/updates and 8H saved bytes per reference for fewer false conflicts. An
approximate independent-occupancy model at the same N, D and 1,024 targets gives about 14.0% chance
of any false conflict for H=2, versus 0.035% for H=4. Neither is a production prediction or a reason
to select H=4 without measuring write costs. Long pauses/high churn eventually change all cells;
this mechanism cannot guarantee progress of arbitrarily old unstarted rows on unchanged edges.

## 4. Existing owners and the reduced, still-open proof obligation

The [reverse-adjacency owner](../../crates/graph/src/facade/derived_state/reverse_adjacency.rs)
already has a `DirectedEdgeKey` grouping source/target/label and counting parallel copies. It is
not an occurrence ID. If shared with the new canonical membership invariant, move its definition
to the existing canonical adjacency owner rather than duplicate the concept or make canonical
writers depend on a derived-state module. No such move is implemented here.

| Path | Inspected evidence | Candidate requirement |
| --- | --- | --- |
| Scalar directed insertion | [edge_insert.rs](../../crates/graph/src/facade/store/edge_insert.rs) already owns source, target, label and canonical preflight | Advance P's cells with canonical creation, before returning or making an external call. |
| Ordered/mixed batch insertion | [batch_write.rs](../../crates/graph/src/facade/batch_write.rs), orientation commit around lines 909–940 | Cover authored logical memberships, not only scalar fallback or initial sidecar installation; deduplicate metadata work only within a proven local commit. |
| Direct edge deletion | [adjacency.rs](../../crates/graph/src/facade/store/adjacency.rs)::commit_delete_edge_by_handle resolves the canonical record before removal | Advance the deleted edge's P using its pre-removal endpoint and label, with the same commit/trap boundary. |
| Vertex deletion/purge | [vertex_delete.rs](../../crates/graph/src/facade/store/vertex_delete.rs) hides/tombstones endpoints before resumable purge | Deleted endpoints reject the saved reference. Exempting later incident purge requires proving it cannot remove an otherwise eligible live-endpoint occurrence or resurrect vertex IDs. Otherwise notify membership changes. |
| Slot movement, compaction, reverse-only repair | [reverse_adjacency.rs](../../crates/graph/src/facade/derived_state/reverse_adjacency.rs) keeps forward membership authoritative | No revision update for a membership-preserving transformation. An implementation must prove that classification, not infer it from an operation's name. |
| Internal sibling/materialize drains | [deferred.rs](../../crates/ic-stable-lara/src/labeled/bidirectional/deferred.rs) can use no-op observers | Unlike the earlier physical-scope registry, missing pure-movement notification does not itself defeat this proof. Any unaccounted LIVE membership creation/deletion still does. |

This is why logical-key tracking is more promising than simply replacing the old B-tree with an
array. It can remove the requirement to observe every physical move; it does not excuse missing
canonical membership writes or repair existing sidecar/posting synchronization obligations.

For slot access, LARA already exposes `read_out_edge_slots_for_label` in `deferred.rs`, delegating
to selected-slot reads in [traverse.rs](../../crates/ic-stable-lara/src/labeled/graph/traverse.rs).
Inspected slab paths directly address a slot; hybrid paths can reconstruct overflow chains.
These are existing extension points, not a demonstrated bounded/constant-time Graph exact-edge
API. Tree/slab/bypass/overflow coverage and actual work bounds require separate verification.
Initial singleton resolution still needs a bounded canonical scan; result LIMIT alone does not
bound tombstone/overflow work. Budget exhaustion must not be reported as absence.

After final slot binding, no await or remapping maintenance may intervene before SET and durable
scalar completion. Ignoring move notifications between messages does NOT allow a setter to use
an old position after its own local preflight remapped it.

## 5. Durable execution and representation gates

The baseline exact mutation seed and saved bulk-update targets are vertex-specific. This proposal
still requires a typed edge target, exact Graph plan/input guard, full ordered admission comparison,
target-bound scalar fingerprint, real residual policy handling, and explicit stale/budget outcomes.
Those belong in the existing owners, not portable GQL syntax or a second bulk journal.

Cross-canister boundaries remain explicit:

1. Graph's narrow replicated resolution reads adjacency and revisions without registering state.
   A lost response or callback after Abort has no new Graph registration to compensate.
2. Router's winning admission persists the entire immutable target list. Before admission a
   retry may resolve anew; afterward no version refresh or target substitution is allowed.
3. Router reserves the owned scalar identity before dispatch. Graph replays an existing completion
   before revalidating mutable state, then locally validates/SETs/journals a fresh row.
4. Derived RPCs and the Router callback follow canonical completion. Lost positive/zero replies
   settle using the existing retained journals. A changed counter or missing slot is not evidence
   that an earlier unknown dispatch never wrote. Abort and unfinished-child retention stay intact.

Pure validation errors precede writes. Unexpected errors after coupled canonical/revision writes
must trap rather than return an `Err` that leaves partial state. The revision array must reopen
unchanged. Changing its hash, size, encoding, or resetting it while old references survive is
unsupported. Follow the fresh compatible Router/Graph activation policy; no migration or decoder
fallback is proposed. ADR 0007, ADR 0057, plan-format and stable inventory need synchronization only
in an authorized implementation patch, not changes claiming this candidate is already implemented.

## 6. Other alternatives and recommendation

- **Resolve+SET at first row execution:** no new reference-validity storage or instrumentation on
  ordinary insert/delete paths. Router admits the selector, not a selected edge. Graph resolves
  raw zero/one/many, enforces policy, SETs at most one, and persists terminal outcomes without an
  intervening await; rejection outcomes must also replay without reselection. An edge recreated
  before that first execution may be selected. This is an explicit change from pre-admission
  selection, not weaker retry safety, and still needs bounded lookup/local remapping protection.
- **Bounded Bloom/history ring:** does not widen edges, but adds writer-history maintenance and
  validation proportional to intervening history. Overwritten history must fail closed rather
  than assume no deletion. Deprioritize for long-resumable rows; no extra log is selected.
- **Track stable-memory page writes:** might catch physical changes below optional observers, but
  instruments ordinary storage writes, couples validity to shared headers/allocator pages, and
  needs cache-aware read-dependency capture. Too broad to introduce merely to avoid auditing
  canonical insert/delete ownership. This is an engineering deduction, not a measured alternative.
- **Per-edge IDs, delayed reclamation/long-lived pins:** remain outside the preferred direction;
  record growth or reclamation/maintenance delays conflict with the storage/performance goal.

**Disposition:** the witness approaches are not selected. Proceed with the first-execution
all-match direction and storage accounting above; settle its local execution bounds, durable
positive/zero/rejection outcomes, row-versus-edge counts and retention before implementation.
The remaining assessment and probes below explain the unselected witnesses, not extra machinery
to add to the chosen execution path.

Architectural axes: Graph encapsulates canonical membership and validity; Router owns admission,
authorization and recovery; LARA retains storage mapping ownership. Canonical adjacency remains
SSOT, revisions are only historical witnesses, and existing `DirectedEdgeKey` should not be
redefined independently. This is narrower than lifetime IDs or an STM framework, but complete
membership coverage, atomic binding, and practical false-conflict behavior remain approval gates.

Cheap proposed validation: canonical singleton before policy filtering; same-endpoint delete+insert
with zero net count; property-free scalar/batch insertion; deliberate hash collisions; multi-hash
partial/all-cell changes; counter overflow/reopen; pure movement to another slot; endpoint purge;
no counter change for property-only updates; and retained 0/1 reply-loss replay. Then measure the
existing ordinary insert/delete/traversal benchmarks and one focused exact-slot lifecycle, not a
new full E2E fleet. No such implementation tests or benchmarks have run for this investigation.

## Evidence limits

Primary sources, selected local mutation/read paths, and arithmetic were inspected. The background
research service was stopped, so research was performed directly; no independent review is claimed.
No Rust change, new dependency/MemoryId/API, build, runtime test, PocketIC run, or performance
benchmark was performed. Probability calculations are illustrative analysis, not benchmark results.

[tl2]: https://people.csail.mit.edu/shanir/publications/Transactional_Locking.pdf
[boosting]: https://erickoskinen.com/papers/boosting-ppopp08.pdf
[aries]: https://www.vldb.org/conf/1990/P392.PDF
[ring]: https://www.cs.rochester.edu/research/synchronization/2008_Spear_SPAA_RingSTM.pdf
[ic-multi]: https://skills.internetcomputer.org/.well-known/skills/multi-canister/SKILL.md
