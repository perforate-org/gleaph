# 0030. Cross-shard uniqueness via Router-coordinated TCC reservation

Date: 2026-06-22
Status: accepted (partially implemented)
Last revised: 2026-06-22

> **Status note:** The decision is **accepted**. Implementation is **partial: catalog/DDL only;
> enforcement inactive.** Slice 1 has landed the logical constraint catalog (Router-owned
> `ConstraintNameId` + `ROUTER_UNIQUE_CONSTRAINTS`) and `CREATE`/`DROP CONSTRAINT` parsing and
> storage, under the declare-on-empty contract. Write-path TCC enforcement (reservation table,
> unique-effect outbox, fenced Try/Confirm/Cancel, recovery) is **not yet built**, so the public
> GQL dispatch refuses `CREATE`/`DROP CONSTRAINT` with `NotImplemented` rather than publishing an
> unenforced uniqueness guarantee. This is the named-invariant strong protocol that
> [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) §7 requires before any
> cross-shard prepare/commit machinery is built; the acid roadmap tracks delivery as Phase 5
> contract 3 / Phase 6. Sections below are written as the contract the implementation must satisfy;
> except where this note says otherwise, they describe target behavior, not current code.
>
> **Revision 2026-06-22 #1 (post-review, APPROVE WITH CHANGES):** closed three uniqueness-violating
> paths — (1) intra-mutation duplicate claims, now distinguished by `ClaimId` and rejected
> non-retryably; (2) Router cannot pre-resolve row-dependent `SET` values, so the first cut is scoped
> to Router-resolvable `INSERT` and general `SET` is deferred to a named two-round protocol; (3) TTL
> reclaim is now a receipt-based **proof**, not a bare timer. Also added: ordered old→new
> update/delete transitions, a durable unique-effect receipt as recovery evidence,
> value-equivalence/key-encoding rules, and a constraint lifecycle treating the constraint as a
> logical definition distinct from indexes.
>
> **Revision 2026-06-22 #2 (second review, APPROVE WITH CHANGES):** closed four more blocking gaps —
> (1) "no receipt" was not proof of non-commit because the graph mutation journal evicts at 9 days
> (ADR 0027); the receipt now lives in a dedicated **pinned unique-effect outbox** (un-acked entries
> never evicted), so absence is authoritative. (2) `claim_ordinal` could not identify runtime
> multi-row output; the first cut is restricted to a **statically single-element INSERT** (no
> row-multiplying input) — value *and* claim count must be statically resolvable. (3) Per-claim CAS
> was not atomic and `Err` does not roll back on the IC; Try now **preflights all claims and applies
> in one no-`await` region**, returning `Err` before any write. (4) `validate-then-activate` cannot be
> Router-atomic (the scan needs `await`s); the first cut is **declare-on-empty (at creation) only**,
> with the `Validating → Active/Inactive` state machine deferred. Also: reservations carry a
> self-contained `proof_scope` and referenced mutation records are non-evictable, so GC ordering
> cannot strand a reservation.
>
> **Revision 2026-06-22 #3 (third review, APPROVE WITH CHANGES):** closed three IC-concurrency gaps —
> (1) the reclaim proof spans `await`s during which a same-`ClaimId` retry could dispatch and commit
> while recovery cancels; the proof now **fences dispatch** via a `Reclaiming { generation }` state
> and conditions the post-`await` Confirm/Cancel on the unchanged generation. (2) outbox receipts and
> ack keys now carry the **full `ClaimId`**, so a stale acked-but-unpruned `Acquire` is never misread
> as a new claim's commit evidence. (3) the absence proof is taken via a **replicated read** (read-only
> update endpoint / certified query), never a single-replica query. Also clarified: declare-on-empty
> means **creating a brand-new type with the constraint** (structural), not an async label-stats
> `count == 0`.
>
> **Revision 2026-06-22 #4 (fourth review, APPROVE WITH CHANGES):** closed two more gaps — (1)
> `ClaimId` previously embedded `element_id: Option`, which is `None` at Try and `Some` after insert,
> so the Try claim could never match the `Acquire` receipt; `ClaimId` is now **immutable
> `{ mutation_id, claim_ordinal }`** and `owner_element_id` is a separate reservation/receipt field,
> with `Release` matched by `owner_element_id`. (2) The fencing generation was nested in
> `Reclaiming { generation }` and lost when reverting to `Reserved`, allowing ABA; it is now a
> **persistent `reclaim_generation` field** that is checked-incremented per proof and retained across
> `Reclaiming → Reserved`.
>
> **Revision 2026-06-22 #5 (fifth review, APPROVE WITH CHANGES):** the outbox was identified/acked at
> `ClaimId` granularity, but one canonical segment emits multiple effects (update → `Acquire(new)` +
> `Release(old)`; `DELETE` → one `Release` per constraint). Added an immutable
> **`EffectId = { mutation_id, effect_ordinal }`**; the outbox is now keyed and acked **per effect**,
> `Acquire` is matched by `claim_id` and `Release` by `owner_element_id`, each effect is pinned/acked/
> pruned independently (acking an `Acquire` never unpins its sibling `Release`), and `effect_ordinal`
> is deterministic across replays so recovery can re-run each effect independently.
>
> **Revision 2026-06-22 #6 (sixth review, APPROVE WITH CHANGES):** two contract clarifications —
> (1) added the **Release-before-Acquire** rule: a `Release` seen while its value's reservation is
> still `Reserved`/`Reclaiming` or `owner_element_id` undetermined is **held (not acked)** until the
> `Acquire` is reconciled, preventing a pending `Acquire` from re-creating an already-deleted
> element's reservation (a permanent leak); a stale `Release` (owner is a different element) may
> no-op-ack. (2) made the **first-cut admission matrix explicit**: statically single-element `INSERT`
> admitted (acquire); `DELETE`/`DETACH DELETE`/`REMOVE`/label-removal admitted at any cardinality
> (release-only, no Try) so constrained elements can be deleted; every other acquiring shape (any
> `SET`/replacement, row-multiplying INSERT) rejected at admission until the two-round protocol.

## Context

An Internet Computer canister commits one message-handler execution atomically; an inter-canister
`await` is a commit and interleaving point ([ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md)
§8). Gleaph composes cross-shard mutations from multiple shard-local atomic segments coordinated by
the Router:

- the default cross-shard path is the idempotent **roll-forward saga** (ADR 0029 §4 and §6 contract
  2): each shard's write is atomic shard-locally, and the Router converges all shards by idempotent
  retry plus the recovery timer (Phase 4);
- the Router is already the single source of truth for index/constraint definitions, routing, and
  client-key idempotency, and already owns stable storage (the mutation journal, routing leases),
  a self-rescheduling recovery timer, and an amortized retention sweep
  ([ADR 0025](0025-client-mutation-journal-retention-sweep.md)).

ADR 0029 §7 reserves stronger protocols (TCC, staged commit, MVCC) for **named invariants** and
forbids making them the default for all GQL mutations. This ADR names the first such invariant.

## Problem

A **uniqueness constraint** — at most one element of a given label / edge type with property key
`K = v` across the **whole graph** — is a *negative* guarantee ("do not create a second one"). The
roll-forward saga provides only a *positive* guarantee (all writes eventually land) and therefore
**cannot enforce uniqueness**:

- two concurrent inserts of the same value `v` can be placed on different shards and both commit
  their shard-local segments; neither shard can see the other's value;
- roll-forward has no global rollback, and a compensating "delete the loser" decision is itself a
  cross-shard race that can delete both or neither.

Shard-local enforcement is insufficient because a graph-wide unique value can land on any shard, and
no shard has graph-wide visibility. The constraint needs a graph-wide serialization point that can
*reject the second claim before it commits*.

## Existing architecture assessment

- **Graph shard (canonical owner, ADR 0029 §1)** can enforce uniqueness only within its own shard;
  it has no cross-shard view, so it cannot own a graph-wide unique constraint.
- **Roll-forward saga (ADR 0029 §4)** gives positive convergence, not the negative guarantee.
- **Optimistic write-then-detect** cannot cleanly compensate a committed unique violation under
  concurrency (see Problem), so it does not satisfy the invariant.
- **Router** is the only component with a graph-wide, per-graph serialization point on the write
  path, and it already owns the index/constraint catalog, idempotency journal, recovery timer, and
  retention sweep. The uniqueness *decision* ("is `v` already claimed graph-wide?") is naturally a
  Router question.

Per the architecture-preservation bias, this ADR **extends the Router** (no new canister, no new
indexing subsystem) and **reuses the existing saga / mutation-lifecycle / recovery / GC machinery**,
rather than introducing a distributed coordinator or per-shard prepared state.

## Alternatives

1. **Shard-local uniqueness only (minimum change).** Reject. Does not provide a graph-wide
   guarantee; a duplicate value on another shard is undetected.
2. **Key-authority shard:** `hash(value) → owning shard` holds the reservation; Router issues a
   cross-canister prepare to that shard. Reject for the first cut. It reintroduces cross-canister
   *prepared* state, blocking while prepared, and the harder "coordinator trapped between prepare and
   commit" recovery that ICP makes acute — exactly the raw-2PC hazards ADR 0029 §7/§8 warn against —
   for no benefit over a Router-local table on the current single-Router-per-graph topology.
3. **Router-coordinated TCC with a Router-local reservation table (chosen).** The Try (reserve) is a
   Router-local stable compare-and-set with **no inter-canister `await`**; the distributed write
   stays a saga; Confirm/Cancel are Router-local. This is "a coordinator-local reservation phase
   added to the existing saga."
4. **Full blocking 2PC across all shards for every unique write.** Reject. Heaviest option, makes a
   strong protocol the default, violates ADR 0029 §7.

## Decision

Enforce a graph-wide uniqueness constraint with **Try / Confirm / Cancel (TCC)** where the Router
owns the reservation locally.

### Canonical owner and staged state

- A uniqueness **constraint is its own logical definition** in the Router catalog, *distinct from any
  index*. It owns the unique value space; indexes are merely access paths that may share the same
  property. This resolves the "which named index owns uniqueness" ambiguity: the answer is *neither —
  the constraint does*. A constraint has a `constraint_id` and binds a label / edge type plus one
  property. (It may reuse catalog storage infrastructure, but is a separate named entity with its own
  lifecycle; see Constraint lifecycle.)
- A new Router stable map, `UNIQUE_RESERVATIONS` (new `MemoryId`; see Migration), is keyed by
  `(graph_id, constraint_id, encoded_value)` and holds
  `{ claim: ClaimId, state: Reserved | Reclaiming | Committed, reclaim_generation: u64,
  owner_element_id: Option<ElementId>, reserved_at_ns, proof_scope: TargetShardSet }`, where
  `ClaimId = { mutation_id, claim_ordinal }` is **immutable** for the life of the claim.
  - `claim_ordinal` deterministically identifies the claim within the mutation. In the first cut a
    qualifying INSERT produces **exactly one** constrained element (see Scope), so `claim_ordinal` is
    its plan position; the general form `(plan_position, input_row_ordinal)` belongs to the deferred
    multi-row protocol. `ClaimId` carries **no `element_id`**, so the identity used at Try is byte-for-
    byte the identity stamped on the `Acquire` receipt — they always match (this was the bug fixed in
    revision #4).
  - `owner_element_id` is a **separate** field (not part of identity), `None` at Try and set to
    `Some(id)` at Confirm once the canonical target is known. `Release` reconciliation matches on
    `owner_element_id`, not on the producing mutation's `ClaimId` (see Update transition).
  - `reclaim_generation` is a **persistent** monotone fencing token (not nested inside the state),
    checked-incremented at the start of every reclaim proof and **retained even when the state reverts
    `Reclaiming → Reserved`**. Because the token is never reused, an in-flight proof's post-`await`
    decision (conditioned on the generation it began with) cannot be falsely matched by a later proof
    — this closes the ABA hole that a state-nested generation would leave.
  - `proof_scope` is the **complete set of target shards** the claim may have committed on, copied
    into the reservation at Try time. The reclaim/recovery proof reads it **from the reservation
    itself**, so a GC'd `RouterMutationRecord` can never strand the proof (see Timeout and
    Upgrade/retention).
  - The owner is therefore a **claim**, not a mutation: two distinct claims in the same mutation are
    distinguishable, which is what makes an intra-mutation duplicate detectable (see Try).
  - Keying by the **exact canonical-encoded value bytes** (not a hash) avoids hash-collision false
    rejects; see Value equivalence and key encoding.
- The **canonical effect** is owned by the graph shard, but its durable evidence must **not** live in
  the 9-day-evicting graph mutation journal
  ([ADR 0027](0027-graph-mutation-journal-retention.md) §2). If it did, "no receipt" would be
  ambiguous — *never committed* versus *committed but the receipt was retention-evicted* — and
  cancelling the latter would permit a duplicate commit. Instead, each unique-affecting canonical
  segment appends one **`UniqueEffectReceipt` per individual effect** to a dedicated, durable
  **unique-effect outbox** on the graph shard. A single segment can emit several effects — an update
  emits `Acquire(new)` *and* `Release(old)`; a `DELETE` emits one `Release` per owned constraint — so
  the outbox is identified and acked at **effect** granularity, not claim granularity:

  ```
  EffectId = { mutation_id, effect_ordinal }   // immutable; effect_ordinal deterministic across replays

  UniqueEffectReceipt {
      effect_id:        EffectId,
      claim_id:         Option<ClaimId>,   // Some for Acquire (the reserving claim); audit-only / None for Release
      owner_element_id: ElementId,
      constraint_id:    ConstraintId,
      encoded_value:    Bytes,
      op:               Acquire | Release,
  }
  ```

  - **outbox key and ack key = `EffectId`**; each effect is pinned, acked, and pruned **independently**.
  - **`Acquire` is matched by `claim_id` (`ClaimId`)** — so an old acked-but-unpruned `Acquire` for a
    prior claim on the same value is never mistaken for a newer claim's commit evidence.
  - **`Release` is matched by `owner_element_id`** (the producing mutation differs from the original
    `Acquire`; `claim_id` on a `Release` is audit-only).
  - An effect is **pinned until the Router acks its `EffectId`**, and the Router acks an effect **only
    after it has durably applied that effect** (advanced/removed the matching reservation). Acking the
    `Acquire` of a mutation therefore does **not** unpin that mutation's `Release`; the `Release`
    stays pinned until separately processed, and multiple `Release`s from one `DELETE` are acked one by
    one. Recovery can re-run each effect independently.
  - Consequently, for any reservation that is not yet terminal, a committed claim's `Acquire` effect is
    *guaranteed still present*; effect **absence is then authoritative proof of non-commit**. Outbox
    retention is decoupled from ADR 0027.
- The outbox **absence proof must be replicated**, not a single-replica answer: the Router reads each
  shard's outbox via a **replicated update call** (the simplest form is a read-only update endpoint),
  or an equivalently certified query. A non-replicated query result is insufficient evidence to
  cancel a reservation, because a negative (uniqueness) invariant cannot rest on one replica's view.
- The reservation table is the **single enforcement point** for the cross-shard negative invariant.
  It does not duplicate canonical data: graph shards still own the elements and the outbox receipts.
  The table is *derived* from the canonical unique-effect receipts and must stay consistent with them
  (`Acquire` ⇒ reservation `Committed`; `Release` ⇒ reservation removed).

### Prepare / commit / cancel / recovery transitions

The reservation lifecycle is bound to the existing `RouterMutationRecord` lifecycle (ADR 0029 Phase
4), so one mechanism drives both.

- **Try (Reserve)** — the reservation phase must be **all-or-nothing within a single message with no
  `await`**, because on the IC a Rust `Result::Err` return does **not** roll back state — only a trap
  does. If the Router inserted some reservations and then returned `Err` on a later conflict, the
  early reservations would persist (one could even commit), so Try must never mutate the table before
  it knows *every* claim is insertable. Try therefore runs as three phases with no `await` between
  preflight and apply:
  1. **Claim set** — compute the mutation's full claim set (every `(constraint_id, encoded_value,
     claim_ordinal)` the program will write; first-cut resolvability is in Scope) and reject
     **intra-mutation duplicates deterministically**: two claims targeting the same
     `(constraint_id, encoded_value)` is a non-retryable `UniquenessViolation` (a program error,
     never resolved by retry).
  2. **Preflight (read-only)** — classify every claim against `UNIQUE_RESERVATIONS` *without
     mutating*:
     - absent → *insertable*.
     - present, same `ClaimId` (idempotent replay of this claim) → *already reserved*; no insert
       needed.
     - present, `Committed` → **non-retryable** `UniquenessViolation`.
     - present, `Reserved` by a different claim of the same `mutation_id` → **non-retryable**
       `UniquenessViolation` (intra-mutation duplicate under partial-replay interleaving).
     - present, `Reserved` by a different, still-alive mutation → **retryable**
       `RouterError::UniquenessReservationInFlight` (retry after the in-flight saga resolves: if the
       holder cancels, the retry wins; if it commits, the retry then gets `UniquenessViolation`).
     - present, `Reclaiming { .. }` (a proof is in flight for this value, by this or another claim) →
       **retryable** `UniquenessReservationInFlight`; **the claim is fenced and must not dispatch**
       until the proof resolves. This is the Try side of the reclaim fence (see Timeout).
     - present, `Reserved` by a different mutation **proven abandoned** (see Timeout — proof, not a
       bare timer) → *reclaimable*.
  3. **Decide & apply** — if *any* claim classified as a hard or retryable conflict, **return `Err`
     now, before writing anything** (no partial state). Only if every claim is insertable / already
     reserved / reclaimable does the Router, in the same no-`await` region, insert (or reclaim) all
     reservations `{ claim, Reserved, reclaim_generation = 0, owner_element_id = None,
     reserved_at_ns = now, proof_scope }` together and then proceed to dispatch (a reclaim of an
     abandoned entry reuses its existing `reclaim_generation`, never resetting it). Because there is no
     `await` between preflight and apply, no concurrent message interleaves and the preflight decision
     stays valid through the writes; a trap mid-apply rolls the whole region back.
- **Confirm (Commit)** — after the canonical write is durable on its placement shard, evidenced by a
  `ClaimId`-matched `Acquire` effect in that shard's pinned outbox, the Router transitions the
  reservation `Reserved → Committed` and records `owner_element_id = Some(id)` from the receipt, then
  acks **that effect by its `EffectId`** (unpinning only the `Acquire`; any sibling `Release` of the
  same mutation stays pinned until separately processed). Idempotent and safe to repeat under recovery;
  if a concurrent recovery proof has meanwhile moved the entry to `Reclaiming` (possible only when a
  slow dispatch outlives `UNIQUE_RESERVATION_TTL_NS`), that proof reconciles to `Committed` from the
  *same* `Acquire` effect, so the two paths converge to the same terminal state and Confirm is a safe
  no-op.
- **Cancel (Compensate)** — if the canonical write is *proven* not to have committed (no `Acquire`
  effect for this `ClaimId`; see Timeout) and the mutation is terminally failed, the Router removes
  the entry **only if it is still `state = Reclaiming` with the unchanged `reclaim_generation = g`**
  that the proof began with, releasing the value. The recovery timer performs the same proof-gated,
  generation-fenced cancel.
- **Update transition (old → new value)** — this is the required ordering contract for *any* write
  that *changes* a constrained value. A single mutation that both acquires-new and releases-old is
  **deferred to the two-round protocol** (see Scope: its target selection is row-dependent); the first
  cut realizes the two halves only *separately* — the acquire half via a standalone statically
  single-element `INSERT`, and the release half via a standalone `DELETE`/`REMOVE`. The contract still
  governs ordering whenever the combined form ships: a changing write must follow a fixed order so
  neither the old nor the new value is double-claimed and no value is orphaned:
  1. **Reserve new** value (Try, as above);
  2. **canonical write** the element on its shard, which records two effects with distinct
     `effect_ordinal`s in the same atomic segment — `Acquire(new)` and `Release(old)` — both carrying
     the element's `owner_element_id`;
  3. **Confirm new** (`Reserved → Committed`) on the `Acquire(new)` effect, then ack **that effect's
     `EffectId`**;
  4. **Release old** reservation on the `Release(old)` effect, then ack **that effect's `EffectId`**
     (independently of step 3).

  **`Release` matching is by `owner_element_id`, not by the producing mutation's `ClaimId`.** A
  `Release` effect carries the changing/deleting mutation's `claim_id` only for audit; the Router
  removes the old-value `Committed` reservation **only when that reservation's `owner_element_id`
  equals the effect's** — i.e. the reservation still belongs to the same element. This makes the
  contract well-defined even though the `Release` is produced by a different mutation than the
  original `Acquire`, and prevents a `Release` from removing a reservation that a *different* element
  has since taken over the value.

  This single ordering covers `SET prop = …`, `SET n = {…}` / full property replacement,
  `REMOVE prop` (Release old, no new), label add/remove that changes which constraints apply
  (Release for constraints no longer applicable, Reserve+Confirm for newly applicable ones), and
  `DELETE` / `DETACH DELETE` (one `Release` **effect per owned constraint**, each pinned and acked
  **independently** by `EffectId`, matched by `owner_element_id`). The Router learns the *old*
  value(s) to release from the effects, never by guessing.
- **Recovery** — the recovery timer reconciles strictly from the pinned outbox using the same
  generation-fenced proof (Timeout): under `state = Reclaiming` with the proof's `reclaim_generation =
  g`, a `ClaimId`-matched `Acquire` effect advances the reservation to `Committed` (recording
  `owner_element_id`); a proven-absent `Acquire` whose mutation is terminal is cancelled (only if still
  `Reclaiming` at `g`); a `Committed` reservation whose element has a later `Release` effect matched by
  `owner_element_id` is removed. Each reconciled effect is acked **by its `EffectId`**, independently
  of sibling effects. The reservation table never diverges from the canonical outbox.
- **Release-before-Acquire ordering (mandatory)** — effects are not guaranteed to be reconciled in
  emission order (a `Release` for a value can be read while that value's `Acquire` is still
  un-`Confirm`ed, e.g. an `INSERT` commits, the `Acquire` is not yet `Confirm`ed, another mutation
  `DELETE`s the same element, and recovery sees the `Release` first). Acking such a `Release` would
  later let the pending `Acquire` re-create a `Committed` reservation for an already-deleted element,
  leaking it permanently. Therefore, for a `Release` effect on value `v`:
  - if the reservation at `(graph_id, constraint_id, v)` is `Reserved`/`Reclaiming`, or its
    `owner_element_id` is not yet determined → **hold the `Release`; do not ack it.** Reconcile the
    `Acquire` first (which sets `owner_element_id`), then re-evaluate the `Release`.
  - once `owner_element_id` is determined and **equals** the `Release`'s → apply (remove the
    reservation) and ack the `Release`.
  - once the owner is determined to be a **different** element (the value was taken over) → the
    `Release` is stale → **no-op ack** is allowed.

### Read visibility while prepared

A `Reserved`-but-not-`Committed` value corresponds to an element that may not yet exist canonically.
Reservations are **invisible to GQL reads**: queries read canonical shard state under the existing
read-consistency contract (ADR 0029 §5), and the reservation table is purely a write-side gate. This
keeps staged state encapsulated inside the Router and out of the query API. Read-your-writes for a
confirmed insert is unchanged from ADR 0029 §5.

### Timeout without unsafe lease expiry

A bare timer is **not** sufficient evidence to reclaim, because a canonical write can have committed
on a shard while its reply never reached the Router (the lost-reply case the current code already
handles by consulting the graph shard, `gql.rs` `recover_mutation_outcome`). Elapsed time alone
cannot distinguish "never committed" from "committed but reply lost"; force-expiring the latter would
let a second element with the same value commit. Crucially, the proof must read the **pinned
unique-effect outbox** (not the 9-day-evicting journal), so receipt *absence* genuinely means
non-commit rather than retention loss.

`UNIQUE_RESERVATION_TTL_NS` therefore only makes a `Reserved` entry *eligible* for a reclaim
**proof**; it is never itself the authority to cancel. The proof spans `await`s, and on the IC other
messages — including a client retry of the *same* `ClaimId` — execute during those `await`s. Without
a fence the following interleaving violates uniqueness: recovery observes the outbox absent → a
concurrent retry dispatches the old claim and commits → recovery cancels the reservation → another
mutation reserves and commits the same value → two committed elements. The proof must therefore
**fence dispatch** for the whole window:

1. **Fence (local, no `await`):** atomically **checked-increment `reclaim_generation` to `g`** and set
   `state = Reclaiming` (`g` is captured for the rest of the proof). The token lives outside the state
   and is never reused, so it is robust to the ABA that a state-nested generation would allow. While
   `Reclaiming`, Try fences the same value (it returns retryable and does **not** dispatch — see Try),
   so no claim for this value can commit during the proof. If the entry is not `Reserved` (already
   terminal or already `Reclaiming`), abort this proof.
2. read the claim's `proof_scope` (complete target-shard set) **from the reservation record itself**;
   it is self-contained, so a GC'd `RouterMutationRecord` cannot strand it. (If a legacy reservation
   somehow lacks `proof_scope`, leave `Reclaiming` and **hold**.)
3. via a **replicated** read (read-only update endpoint or certified query — never a bare query),
   ask **every** shard in `proof_scope` for an `Acquire` **effect** whose `claim_id` matches the
   **immutable `ClaimId`**. Un-acked effects are pinned, so a committed claim's `Acquire` effect is
   always present.
4. **re-check the fence:** apply the outcome **only if the entry is still `state = Reclaiming` with
   `reclaim_generation == g`**; if the generation advanced, a concurrent action intervened — discard
   this proof's result and re-evaluate from step 1.
5. if **any** shard reports an `Acquire` effect for this `ClaimId` → committed: **Confirm** (never
   cancel), regardless of elapsed time; then ack that effect by its `EffectId`.
6. only if **all** shards in `proof_scope` are reachable and **all** report the entry absent → never
   committed: **Cancel / steal**;
7. if **any** shard is unreachable or its answer is unknown → revert `state = Reserved` **keeping
   `reclaim_generation = g`** (never decremented or reset) and **hold** (no cancel, no steal),
   retrying the proof later under a fresh, higher generation.

"Expired/abandoned" in the Try and Recovery rules means exactly the generation-fenced outcome of
step 6. A reservation whose mutation reached canonical commit is always advanced to `Committed`,
never force-expired.

### Upgrade / reopen and bounded retention

- `UNIQUE_RESERVATIONS` is in Router stable memory; it survives upgrade. `post_upgrade` re-arms the
  recovery timer, which reconciles reservations against the pinned outbox on reopen.
- **GC ordering safety.** The reclaim proof depends only on the reservation's own `proof_scope` and
  the pinned outbox, never on the `RouterMutationRecord`, so a `RouterMutationRecord` GC'd first
  cannot strand a reservation. As defense in depth, a `RouterMutationRecord` referenced by any
  non-terminal reservation is **non-evictable** until that reservation is terminal (consistent with
  ADR 0029 Phase 4's exclusion of non-terminal sagas from TTL eviction).
- **Outbox retention.** Unique-effect outbox entries are pinned until acked and so are *not* governed
  by the ADR 0027 9-day journal eviction; they are pruned only after the Router has durably advanced
  and acked the matching reservation. This keeps the proof total while remaining bounded (one un-acked
  entry per in-flight unique effect).
- `Committed` reservations persist while the owning element exists (released on delete). `Reserved`
  entries are bounded by the amortized retention sweep (ADR 0025 mechanism, extended to scan
  reservations), which GCs only proof-confirmed abandoned reservations. Growth is bounded by the
  number of currently claimed unique values.

### Value equivalence and key encoding

The reservation key's `encoded_value` must be a **total, injective function of GQL value equality**
for the constrained property's declared type — two values are the same key iff GQL considers them
equal — otherwise the gate either false-rejects distinct values or false-admits equal ones:

- **Numbers:** encode in the property's declared scalar type's canonical form so GQL-equal numbers map
  to identical bytes (e.g. integer/float that compare equal under the type system share one
  encoding). Mixed-type numeric equality follows the GQL type rules for that property.
- **NaN:** `NaN ≠ NaN` in GQL, so it has no stable key identity; a `NaN` value is **rejected** as a
  unique key (it cannot participate in a uniqueness constraint).
- **NULL / missing:** SQL-style — a missing or `NULL` constrained property makes **no claim**
  (multiple `NULL`s are allowed). It is not reservable.
- **Strings:** byte-exact comparison on canonical UTF-8; no implicit Unicode normalization or
  collation in the first cut (a declared collation is out of scope and must be added explicitly if
  ever needed).
- **Max key length:** `encoded_value` is bounded by the stable map's key bound; a value whose
  encoding exceeds the bound is **rejected** for a unique constraint in the first cut (rather than
  hashing, which would reintroduce collision risk).

### Constraint lifecycle (CREATE / activate / DROP)

Because a constraint is a logical definition separate from indexes, its lifecycle is owned by the
Router catalog and must be atomic with respect to the write path (the Router is the per-graph write
serialization point):

- **CREATE (first cut = declare-on-empty, no scan):** validating a *populated* graph cannot be made
  atomic by the Router alone, because a validation scan requires inter-canister `await`s and other
  write messages interleave during it. The first cut therefore avoids validation entirely: a
  constraint may be declared **only by creating a brand-new label / edge type together with the
  constraint**, so that label was structurally never writable before the constraint existed.
  Emptiness is a *structural* property of "this type did not previously exist," **not** an
  asynchronous `count == 0` judgment from the lagging label-stats projection (which could be stale).
  No cross-shard scan and no half-active window exist. Declaring on an existing/live type is
  **deferred** and requires the state machine below.
- **CREATE on a populated graph (deferred state machine):** `Validating` (reject or hold writes to
  the constrained domain) → scan all shards via the outbox/canonical state → `Active` on success /
  `Inactive` on failure. `post_upgrade`/recovery must either resume a `Validating` scan or safely
  abort it to `Inactive` (never silently land in `Active` without a completed scan). This is its own
  ADR amendment.
- **DROP:** dropping a constraint atomically invalidates its reservations; a write observes the
  constraint as fully present or fully absent, never half-dropped. In-flight reservations for a
  dropping constraint are drained (their sagas complete or cancel) under the same proof rules.
- **Ownership:** uniqueness is owned by the constraint, never by an index. Multiple named indexes on
  the same property are access paths and do not each impose uniqueness; only a declared constraint
  does.

### Conflict and retry semantics

- `Committed` conflict → non-retryable `UniquenessViolation`.
- Different-claim same-mutation conflict (intra-mutation duplicate) → non-retryable
  `UniquenessViolation` (deterministic; retry never helps).
- `Reserved`-alive different-mutation conflict → retryable `UniquenessReservationInFlight`.
- Same-`ClaimId` replay → no double reservation; idempotent; converges.
- Placement is unchanged: the unique-constrained element still lands via the normal placement policy
  (latest shard for brand-new inserts, anchor for anchored writes; ADR 0029 §6 /
  [federation-target](../sharding/federation-target.md)). The reservation is graph-wide, so it
  serializes claims regardless of which shard the element lands on.

### Scope (first cut)

A mutation **acquires** a constrained value only through the Router-local Try (reserve-before-commit),
which must run with no inter-canister round trip; it **releases** a constrained value purely by
reconciling `Release` effects from the outbox (no Try involved). The first-cut admission matrix
follows from that asymmetry — what must be pre-resolvable is the *acquire* claim set, not releases:

- **Admitted — acquiring: statically single-element `INSERT`.** A single-property unique constraint on
  a vertex label (and, if cheap, an edge type), where the admission predicate requires (i) no
  row-multiplying input feeding the INSERT (no driving `MATCH` / `UNWIND`), so the program provably
  produces **exactly one** constrained element, *and* (ii) the constrained value is Router-resolvable
  (a literal or bound parameter, not a row-dependent expression). Router-resolvable *value* is not
  enough on its own — a single INSERT operator can emit one element **per input row**, so the *claim
  count* must also be statically one.
- **Admitted — release-only mutations (no acquire).** `DELETE` / `DETACH DELETE`, `REMOVE` of a
  constrained property, and label removal that drops a constraint's applicability acquire **no**
  constrained value, so they need no Try and are admitted **at any cardinality**: the Router simply
  reconciles their `Release` effects from the outbox by `owner_element_id` (per the Release rules
  above). Including these in the first cut is necessary — otherwise a unique-constrained element could
  never be safely deleted, and its reservation could never be released.
- **Rejected at admission (deferred to the two-round protocol):** any mutation that **acquires** a
  constrained value in any shape other than the statically single-element INSERT above — a
  row-multiplying INSERT (`INSERT` driven by `MATCH`/`UNWIND`), and any `SET` / full replacement that
  introduces a constrained value (`SET n.email = expr`, `SET n = {…}`, multi-row `SET`, and even a
  `SET` to a *literal*, because the **target element selection is row-dependent** so the acquire claim
  set is not Router-pre-resolvable). These — including update transitions that both acquire-new and
  release-old — need the two-round protocol: the graph shard enumerates the candidate unique values
  (with deterministic `(plan_position, input_row_ordinal)` ordinals) and returns the claim set to the
  Router *before* canonical commit; the Router reserves; only then does the shard commit. Until that
  amendment ships, such mutations are rejected at admission with a non-retryable error.
- **Also out (deferred):** composite multi-property unique keys; relationship-endpoint uniqueness;
  declaring a constraint on an already-populated, actively-written graph (backfill); cross-graph
  uniqueness.

## Consequences

- Graph-wide uniqueness with a real negative guarantee, enforced at the only graph-wide
  serialization point (Router) — strong SSOT for the invariant.
- No cross-canister prepare round trip: Try/Confirm/Cancel are Router-local stable operations; the
  distributed write remains the existing saga.
- Reuses `mutation_id`, the mutation lifecycle, the recovery timer, and the retention sweep — no new
  canister and no new index subsystem; the additions are one Router stable map, a logical constraint
  definition in the catalog, and a pinned unique-effect outbox on the graph shard.
- Staged state stays encapsulated: reservations never appear in read results or public read APIs.
- Recovery and reclaim are **proof-based** over the pinned outbox (so "absent" truly means
  non-commit) and self-contained via `proof_scope` (so GC ordering cannot strand a reservation); they
  are never time/heuristic-based, so the reservation table cannot silently diverge from canonical
  state.

## Trade-offs

- The Router must be on the write path for unique-constrained mutations (it already is, for routing
  and idempotency) and must run a Confirm step after canonical commit (already true for saga-tracked
  idempotent DML).
- A new **consistency surface** appears between the Router reservation table and shard canonical
  state. It is made provable rather than best-effort by the pinned outbox: the table is reconciled
  only from durable `Acquire`/`Release` outbox entries, so the two never independently drift.
- The graph shard must carry a **pinned unique-effect outbox** of `UniqueEffectReceipt`
  (`effect_id`, `claim_id`, `owner_element_id`, `constraint_id`, `encoded_value`, `op`) with per-effect
  pin-until-acked retention, separate from the ADR 0027 journal — a new, small durable structure and a
  per-effect ack round on the canonical write path.
- General `SET` / row-dependent values are **not** covered by the first cut; they require a
  two-round (graph-evaluates-then-Router-reserves-then-commits) protocol, a real extension of the
  Router-local-Try shape. First cut deliberately ships only Router-resolvable `INSERT`.
- The loser of a concurrent claim receives a retryable error (more client retries under contention).
- A unique-constrained write is no longer a fire-and-forget shard-local mutation; it participates in
  the Router-coordinated lifecycle.

## Migration

- Add a `MemoryId` for `UNIQUE_RESERVATIONS` (update
  [stable-memory inventory](../storage/stable-memory-inventory.md)).
- Add a logical **constraint definition** to the Router catalog (distinct from index definitions) and
  the `CREATE CONSTRAINT` / `DROP CONSTRAINT` DDL; existing graphs are unaffected until a constraint
  is declared.
- Add a **pinned unique-effect outbox** on the graph shard (new durable structure keyed by `EffectId`
  with per-effect pin/ack/prune; `Acquire` matched by `ClaimId`, `Release` by `owner_element_id`;
  retention decoupled from the ADR 0027 9-day journal) and a **replicated read-only update endpoint**
  the Router uses to obtain a replicated presence/absence answer. Gate it behind the constraint being
  declared so non-constrained writes are unchanged.
- First cut declares a constraint **only before the constrained label can have elements** (graph/
  schema creation), so no validation scan is needed. Declaring on a populated, actively-written graph
  (the `Validating → Active/Inactive` state machine and backfill) is deferred to its own amendment.
- No wire-breaking change to existing entrypoints; unique enforcement is additive on the idempotent
  DML path.

## Design documentation impact

- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) §7 (link this ADR as the
  first named-invariant instance) and §6 (contract 3 pointer).
- [acid roadmap](../architecture/acid-roadmap.md) Phase 5 contract 3 and Phase 6.
- [stable-memory inventory](../storage/stable-memory-inventory.md) (new Router `UNIQUE_RESERVATIONS`
  region and the graph-shard unique-effect outbox region).
- [ADR 0027](0027-graph-mutation-journal-retention.md) (note that the unique-effect outbox has its
  own pin-until-acked retention, distinct from the 9-day journal eviction).
- [derived-state query semantics](../index/derived-state-query-semantics.md) (uniqueness enforcement;
  reservation invisibility to reads).
- [federation-target](../sharding/federation-target.md) (placement unchanged; uniqueness gate added).
- index / DDL design doc (constraint as a logical definition, distinct from indexes) and the
  graph-shard outbox schema doc (unique-effect receipt).

## Required test and benchmark gate (Phase 6)

The decision is already `accepted` and the catalog/DDL layer has landed (see the status note), but
the constraint feature stays **unpublished** — the public dispatch returns `NotImplemented` — until
enforcement is fully implemented. Before that gate opens and `CREATE`/`DROP CONSTRAINT` is accepted
on the write path, the implementation must add:

- failure-injection tests for **every** message boundary: Try-then-Router-trap, canonical-write
  failure after Try, Confirm-then-trap, concurrent same-value claims (one wins, loser retryable),
  delete-releases-reservation, and upgrade-reopen reconciliation;
- **intra-mutation duplicate**: one mutation inserting the same unique value twice is rejected
  non-retryably with no partial state;
- **multi-claim Try atomicity**: a conflict on a later claim leaves **no** earlier reservation
  written (the `Err` path mutates nothing);
- **outbox-vs-eviction proof**: a committed-but-reply-lost write is `Confirm`ed via the pinned outbox
  even past the ADR 0027 journal eviction window, and is **never** cancelled; `Cancel` happens only
  when all `proof_scope` shards are reachable and the entry is provably absent; an unreachable shard
  holds the reservation;
- **GC-ordering**: a `RouterMutationRecord` evicted before its reservation is terminal still resolves
  via the reservation's own `proof_scope` (and the record is non-evictable while referenced);
- **reclaim fence**: a client retry of the same `ClaimId` arriving *during* a reclaim proof is fenced
  by `state = Reclaiming` (it cannot dispatch), and a proof whose `reclaim_generation` changed across
  the `await` is discarded — the recovery-cancels-while-retry-commits interleaving cannot occur;
- **generation ABA**: a reservation that goes `Reserved → Reclaiming → Reserved` (held on an
  unreachable shard) **keeps** its incremented `reclaim_generation`, so a later proof never reuses a
  number an in-flight callback still holds;
- **ClaimId stability**: the `ClaimId` computed at Try (no `element_id`) byte-matches the `Acquire`
  receipt's `ClaimId` after canonical insert; `owner_element_id` is carried as a separate field;
- **Release matching**: a `Release` removes a `Committed` reservation only when `owner_element_id`
  matches, so a `Release` cannot remove a reservation a different element has taken over;
- **per-effect ack**: acking a mutation's `Acquire` effect leaves that mutation's `Release` effect
  still pinned until separately processed; a multi-constraint `DELETE`'s several `Release` effects are
  acked one by one, and recovery can re-run any single un-acked effect independently;
- **stale receipt isolation**: an old acked-but-unpruned `Acquire` for a prior `ClaimId` is not
  accepted as commit evidence for a new claim on the same value;
- **replicated proof**: the outbox absence proof is taken via the replicated read endpoint; a
  single-replica query answer is never used to cancel;
- **acquire deferral**: a statically-multi-element / row-multiplying INSERT, and **any** acquiring
  `SET` / full replacement (even to a literal, because target selection is row-dependent), are
  rejected at admission in the first cut (until the two-round protocol ships);
- **release-only admitted**: `DELETE` / `DETACH DELETE` / `REMOVE prop` / label-removal of a
  constrained element is admitted at any cardinality and releases every owned claim via `Release`
  effects matched by `owner_element_id`;
- **Release-before-Acquire recovery**: a `Release` observed before its value's `Acquire` is
  `Confirm`ed is **held** (not acked) until the `Acquire` sets `owner_element_id`; it is then applied,
  or no-op-acked only once the owner is proven to be a different element — an already-deleted element's
  reservation is never re-created or leaked;
- **declare-on-empty only**: declaring a constraint after the constrained label has elements is
  rejected in the first cut (only declare-at-creation is admitted);
- **value equivalence**: GQL-equal values share a key; `NaN` and over-length values are rejected;
  `NULL`/missing makes no claim;
- canbench evidence for the Try/Confirm/Cancel overhead on the write path, the outbox ack round, and
  reservation-table storage growth.
