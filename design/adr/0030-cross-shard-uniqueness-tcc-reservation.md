# 0030. Cross-shard uniqueness via Router-coordinated TCC reservation

Date: 2026-06-22
Status: accepted (partially implemented)
Last revised: 2026-06-22

> **Status note:** The decision is **accepted**. Implementation is **complete through slice 7:
> catalog/DDL + value encoding + reservation table & no-`await` Try + pinned unique-effect outbox + the
> INSERT write-path TCC (Try/Acquire/Confirm) + the DELETE/REMOVE Release write-path + the slice-6
> recovery reconciler (reclaim Driver 1 + unified effect recovery Driver 2) + the slice-7
> failure-injection / canbench gate (the Phase-6 test/benchmark gate is satisfied — see Revision #14).
> Two items remain before `CREATE`/`DROP CONSTRAINT` is published, neither a Phase-6 gap: (1) the
> deliberately separate CREATE publication decision, held for final architectural review; and (2) a
> dedicated DROP-lifecycle slice — DROP currently removes only the constraint definition, so
> reservation invalidation, in-flight saga draining, and a `ConstraintNameId`-reuse guard (re-`CREATE`
> of a dropped name reuses its id against stale reservations, risking false rejects) are not yet
> implemented (see Revision #15).** Slice 1 has
> landed the logical constraint catalog
> (Router-owned `ConstraintNameId` + `ROUTER_UNIQUE_CONSTRAINTS`) and `CREATE`/`DROP CONSTRAINT`
> parsing and storage, under the declare-on-empty contract. Slice 2 has landed the canonical
> `encoded_value` (`gleaph_gql_ic::unique_key`): the equality-injective reservation key built on the
> property-index key encoding, with NULL→no-claim, non-finite(`NaN`/`±∞`)/unsupported/over-length
> rejection, and the
> shared `MAX_UNIQUE_ENCODED_VALUE_LEN` bound. Slice 3 has landed the reservation table
> (`ROUTER_UNIQUE_RESERVATIONS`, MemoryId 37, keyed by `(graph_id, constraint_id, encoded_value)`),
> the `Reserved`/`Reclaiming`/`Committed` state-machine schema (immutable `ClaimId`,
> `reclaim_generation`, `owner_element_id`, `proof_scope`), and the **no-`await` Try** transition
> (claim-set dedup → read-only preflight → all-or-nothing apply). Slice 3 deliberately classifies a
> value held by another live mutation as retryable *in-flight* rather than attempting a reclaim — the
> safe subset that never falsely admits a duplicate — because the generation-fenced reclaim proof
> depends on the unique-effect outbox (slices 4–6). Slice 4 has landed the graph-shard **pinned
> unique-effect outbox** (`UNIQUE_EFFECT_OUTBOX`, MemoryId 42, keyed by `EffectId`): per-effect
> append/pin, `Acquire`-by-`ClaimId` and `Release`-by-`owner_element_id` matching, idempotent replay,
> plus the Router-only **replicated `Acquire`-proof read** and **per-effect ack** endpoints
> (`read_unique_effect_proof` / `ack_unique_effects`). Slice 5a has landed the **INSERT write-path
> TCC**: Router admission (single-vertex pure `INSERT`, single-shard routing, Router-resolvable
> literal/parameter constrained value), the fenced no-`await` Try wired into the dispatch path before
> the canonical write, the dispatch envelope carrying the claim list (`ExecutePlanArgs.unique_claims`),
> the shard-side `Acquire` emit inside the canonical DML segment, and Confirm/ack (replicated
> proof read → `Reserved → Committed` (also re-acking an idempotent re-confirm) → per-effect ack).
> Slice 5b has landed the **DELETE/REMOVE Release write-path**: release-only mutations
> (`DELETE`/`DETACH DELETE`, `REMOVE prop`, and label-removal) are admitted at **any cardinality**
> with no Try; the dispatch
> envelope carries the graph's constrained `(label, property)` set
> (`ExecutePlanArgs.constrained_properties`, an ephemeral per-operation catalog slice like
> `indexed_properties`, ADR 0023); the shard pins one `Release` receipt per freed value inside the
> canonical DML segment (captured pre-mutation so the value and owner element id are still readable)
> — covering vertex delete, `REMOVE n.prop` on a constrained property, and `REMOVE n:Label` that drops
> a constraint's applicability — with a **mutation-wide `effect_ordinal` cursor carried across every
> canonical segment** so a multi-statement DELETE never re-mints an `EffectId`. The Router reconciles
> each `Release` after commit via a **paginated** per-`mutation_id` replicated read
> (`read_unique_release_effects(mutation_id, after_ordinal, limit)`, cursor by `effect_ordinal`, hard
> page cap so an arbitrary-cardinality DELETE cannot overflow the IC response), removing the
> `Committed` reservation **matched by `owner_element_id`** and acking the effect page by page, while a
> **held** release (Release-before-Acquire) is left pinned (the cursor still advances past it so
> reconciliation terminates and slice-6 recovery revisits it). Slice 6 has landed the **recovery
> reconciler** (see Revision #10): the reclaim Driver converges reservations from a generation-fenced
> proof, and the unified effect-recovery Driver drains a durable discovery index to converge every
> pinned `Acquire`/`Release`/orphan effect the inline path left behind. Slice 7 has **landed**
> (see Revision #13, corrected by #14): the enforced lifecycle (Try/Acquire/Confirm, duplicate
> rejection, in-flight retryability, SET deferral, delete-release, roll-forward retry, reclaim-Cancel,
> upgrade-reopen reconciliation, admin-gated declaration) is verified on real canisters, the seven
> ADR-required cross-canister failure-injection scenarios (Try-then-trap, Confirm-then-trap,
> Confirm→ack-failure, true concurrent same-value, outbox-vs-9-day-journal-eviction,
> reclaim-during-retry, Release-before-Acquire) pass on PocketIC, and both the Router-local
> Try/Confirm/Cancel/reclaim overhead and the graph-shard outbox append/ack + storage-growth have
> persisted canbench baselines. **The Phase-6 test/benchmark gate is satisfied.**
> DDL `CREATE`/`DROP CONSTRAINT` nonetheless stays `NotImplemented`. For `CREATE` the only remaining
> step is the deliberately separate publication flip, held for final architectural review; `DROP`
> additionally requires a dedicated lifecycle slice (reservation invalidation, in-flight saga draining,
> and a `ConstraintNameId`-reuse guard) before it can be published — see Revision #15. This is the named-invariant
> strong protocol that
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
>
> **Revision 2026-06-22 #7 (slice 5a implementation landed):** the INSERT write-path TCC is now wired
> end-to-end (Router admission → no-`await` Try → claim dispatch → shard `Acquire` emit →
> Confirm/ack). Two implementation contracts hardened during review: (1) the **fallible preflight and
> the single-shard gate both run before the shard-envelope record and before Try**, so a rejected or
> over-scope constrained insert records no saga envelope and reserves nothing (a post-envelope
> rejection would strand a non-terminal saga that the recovery scan revisits forever); (2) Confirm
> acks an effect **iff the value is committed by that claim** — a fresh `Reserved → Committed`
> transition **or** an idempotent re-confirm of an already-`Committed` claim (so a Confirm replayed
> after a failed ack re-acks and the pinned effect is eventually unpinned) — and only after a **full
> `ClaimId` (mutation + ordinal) match** of the proof. Acking on a missing/`Reclaiming`/
> committed-by-another-claim/mismatched reservation would destroy the sole durable commit evidence. `CREATE`/`DROP CONSTRAINT`
> remains `NotImplemented` (see status note): the lifecycle is not complete until delete-release
> (5b), recovery (6), and failure-injection (7) land.
>
> **Revision 2026-06-22 #8 (slice 5b implementation landed):** the DELETE/REMOVE Release write-path is
> now wired end-to-end. Release-only mutations need **no Try** and are admitted at **any cardinality
> and any shard count** (the asymmetry in Scope: only the *acquire* claim set must be
> Router-pre-resolvable). The constrained-property set rides the dispatch as
> `ExecutePlanArgs.constrained_properties` — an **ephemeral per-operation slice** of the Router
> constraint catalog, mirroring `indexed_properties` (ADR 0023): the shard persists **no** derived
> constraint catalog, so it cannot go stale across the upgrade boundary. Because the Router is the
> sole interner of label/property names (it ships `Resolved*Table`, the shard persists those ids), the
> dispatched `(VertexLabelId, PropertyId)` match a deleted vertex's stored ids with no translation.
> The shard captures each freed value **before** the canonical delete (the value and `owner_element_id`
> are unreadable afterward), then pins one `Release` receipt per freed value inside the same atomic
> DML segment, with `effect_ordinal`s offset past the mutation's `Acquire` ordinals so every
> `EffectId` stays distinct and replay-deterministic. After commit the Router reads the mutation's
> `Release` effects (`read_unique_release_effects`, a replicated per-`mutation_id` read since a
> `Release` is matched by `owner_element_id`, not `ClaimId`) and reconciles each: a `Committed`
> reservation whose `owner_element_id` matches is removed and the effect acked; a missing reservation
> or a stale `Release` (the value was taken over by a **different** element) is a no-op ack; and a
> **held** release — the value is still `Reserved`/`Reclaiming` or its owner is undetermined
> (Release-before-Acquire) — is **not acked**, staying pinned for the slice-6 reconciler so a pending
> `Acquire` can never re-create an already-deleted element's reservation. Reconciliation is
> best-effort and idempotent like Confirm (the canonical delete cannot be rolled back). `CREATE`/`DROP
> CONSTRAINT` remains `NotImplemented`: recovery (6) and failure-injection (7) must still land.
>
> **Revision 2026-06-22 #9 (slice 5b correctness/scale hardening):** three gaps in #8 are closed so the
> release path matches what admission/this ADR already claim. (1) **`REMOVE` capture parity** — the
> shard now captures a `Release` not only for vertex delete but for `REMOVE n.prop` on a constrained
> property and for `REMOVE n:Label` that drops a constraint's applicability (both captured before the
> property/label is gone, keyed by the same canonical `encoded_value` and matched by
> `owner_element_id`). Previously these deleted directly and **stranded the reservation**, so the value
> could never be reused. (2) **Mutation-wide `effect_ordinal` cursor** — a single mutation can run
> several canonical segments (one per DML statement); the `Release` ordinal cursor is now carried
> **across** segments (seeded past the mutation's `Acquire` ordinals) instead of restarting per
> segment, so a multi-statement `DELETE` can no longer re-mint an `EffectId` and trap the outbox on a
> mismatched receipt. (3) **Paginated release read** — `read_unique_release_effects` now takes an
> `(after_ordinal, limit)` cursor and returns one bounded page (the shard clamps `limit` to a hard cap
> so an arbitrary-cardinality `DELETE` cannot exceed the IC response/heap limits); the Router pages
> through, reconciling and acking each page and advancing the cursor past **every** observed effect
> (including held ones, which slice-6 recovery revisits). End-of-stream is signaled by an **empty
> page**, not a short one: the shard clamps `limit` to its own hard cap, so a page shorter than the
> Router's requested size does not imply the last page (a rolling upgrade or a smaller shard cap would
> otherwise strand releases past the first short page). The cursor increases by at least one each
> iteration, guaranteeing termination. (4) **`SET` admission gate now enforced** — the deferred-write
> rule above (reject `SET` that touches a constrained value until the two-round protocol ships) was
> specified but not wired: `plan_unique_claims` only scans `InsertVertex` and `plan_can_release` only
> covers `DELETE`/`REMOVE`, so `SET n.email = …`, `SET n = {…}`, and `SET n IS User` (adding a
> constrained label) reached the canonical write **unguarded**. The dispatch now calls
> `reject_unsupported_constrained_writes` before reserving/dispatching: when the graph declares any
> constraint, a `SET` item that targets a constrained property, replaces all properties, or adds a
> constrained label is refused with `NotImplemented`. DDL is still non-public so no external guarantee
> was breached, but the gate must exist before `CREATE CONSTRAINT` is published.
>
> **Revision 2026-06-22 #10 (slice 6 implementation landed — recovery reconciler):** the autonomous
> recovery layer is now wired into the existing self-rescheduling recovery timer
> ([ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) Phase 4) as **two
> bounded, cursor-paged drivers** that ride the same tick with independent round-robin cursors and a
> non-wasm no-op; both converge only the **safe subset** (the authority to free a value is always a
> generation-fenced proof, never a timeout).
>
> **Driver 1 — reclaim reconciler** (`reclaim.rs`). Scans the reservation table for overdue
> `Reserved` / `Reclaiming` / `Committed`-with-pending-ack entries and applies exactly one fenced
> outcome each (per Timeout): *Acquire present on any reachable scope shard* → `Reclaiming@g →
> Committed` + ack; *every scope shard reachable and explicitly absent* **and** the mutation is
> irreversibly terminally failed → cancel; anything else → hold. Three correctness contracts were
> hardened during review and are now binding:
> - **Cancel needs an irreversible terminal-failure, not merely `Failed`.** A `RouterMutationRecord`
>   that is `shards.is_empty() && completed_row_count.is_none()` could be re-driven `routing_in_progress`
>   by a same-client-key retry, so it is not safe Cancel grounds. The record now carries a distinct,
>   **irreversible `terminal_failure`** state; `is_terminal()`/lifecycle-phase prioritize it; a
>   same-client-key retry of a terminal-failed mutation is refused (only a **new** client key may
>   retry). Cancel is the conjunction of *all-scope-explicitly-absent* **and** *this irreversible
>   terminal-failure*, applied with count-decrement in one **all-or-nothing** region:
>   `reclaim_cancel_uncommitted` preflights every condition (record eligibility + `mutation_id`,
>   reservation claim/generation fence, reverse-row/count) and only then applies the infallible
>   terminal-fail + cancel + decrement — never a `false`/`Err` that leaves partial state (`Err` does
>   not roll back on the IC; only a trap does).
> - **Strict negative-proof classifier.** `AllAbsent` requires an **explicit `{ claim_id, acquire:
>   None }` from every reachable scope shard**; an empty scope, a missing claim row, a malformed
>   response, or any unreachable shard is `Inconclusive` → hold. An incomplete negative can never
>   authorize a Cancel or an ack-reply-lost clear.
> - **ClaimId + generation fence (ABA).** `cancel`/`hold` reclaim are keyed on the reclaim ticket's
>   **`ClaimId` and `generation`** together, so a delayed callback for a deleted reservation A cannot
>   Cancel/Hold a same-value reservation B that reused the generation.
> - **Crash-after-commit re-ack.** `→ Committed` (both inline Confirm and reclaim commit) atomically
>   stamps `pending_acquire_ack`; it is cleared **only after** the ack succeeds, so a crash between
>   commit and ack re-discovers the `Committed`-pending entry and re-acks (Confirm replays return
>   `ConfirmOutcome::{FreshlyCommitted, AlreadyCommitted, NotApplicable}`; the non-terminal count is
>   decremented only on `FreshlyCommitted`). A `Committed`-pending entry whose proof is all-absent is
>   the **ack-reply-lost** case and clears the marker; unreachable → hold.
>
> Terminal-record resolution (which `RouterMutationRecord` owns a reservation's claim, and is its GC
> pin) is an **idempotency-owned reverse index** `MutationId → { client_key, nonterminal }` (region
> 38), not a per-reservation client-key copy: `++` per fresh Try insert (idempotent replay does not
> re-increment), `--` on `FreshlyCommitted` Confirm and on reclaim Cancel; `count > 0` GC-pins the
> record; the row is removed with the record at zero. The pin count is **fail-closed**
> (`checked_add().expect`/trap, client-key-consistency assert, trap on release underflow/missing) so
> an undercount can never let a non-terminal sibling's record be GC'd.
>
> **Driver 2 — unified effect recovery** (`effect_recovery.rs`). Driver 1 is reservation-driven, but a
> `Release` (whose releasing mutation differs from the original `Acquire`) and an **orphan `Acquire`**
> (reservation gone) own no reservation it can find. Driver 2 drains the **`UNIQUE_EFFECT_PENDING`
> discovery index** (region 39; this is the generalization of the Release-only "Release work
> discovery" row below — it now covers **both** `Acquire` and `Release`). Each row
> `(graph_id, mutation_id, shard_id) → PendingEffectRecord { schema_version, canister, client_key,
> state: Active | Quarantined, next_retry_ns, attempts, diagnostic? }` is **registered before the
> first dispatch `await`** for any dispatch that may emit a unique effect, so it co-commits with the
> reservation/envelope. The value is a **versioned record** (so orphan diagnostics / quarantine fields
> can extend it without a stable-layout break) and registration is **fail-closed on both identities**
> — re-registering a key to a different `canister` or `client_key` traps. The row **GC-pins its owning
> `RouterMutationRecord`** (its terminal-completion proof); the `client_key` is stored verbatim so the
> record is resolvable for **any** effect kind, even after the shard is unregistered. Per row:
> - **Termination gate:** drain only when the owning record is the **same `mutation_id`** *and*
>   terminal (effect generation finished). A missing record (the GC pin should prevent it), a
>   same-client-key retry that recycled the record onto a different mutation, or a still-non-terminal
>   record → **hold**. This is what makes the orphan classification sound: a reservation-less `Acquire`
>   is an orphan **only** after the mutation can emit no more effects.
> - **Drain** (replicated paginated all-effects read `read_unique_mutation_effects`, cursor by
>   `effect_ordinal`, **short page ≠ EOF; only an empty page is EOF**): a `Release` reconciles the
>   reservation **durably first** and acks only on a proven free (else held — Release-before-Acquire);
>   an `Acquire` **with** a reservation is **delegated to Driver 1** (never acked here); a
>   reservation-less `Acquire` is an **orphan** — never acked, the row is **quarantined** with a
>   persistent diagnostic and a long re-check backoff (`next_retry_ns`), keeping the row and evidence.
> - **Removal** only after the termination gate passed **and** a fresh `cursor = None` re-scan is empty
>   (every effect acked), which un-pins the owning record for GC.
> - **No timer hot-loop / no going dark:** a `Quarantined` row inside its backoff is skipped without
>   counting as lap work, but the sweep surfaces the **earliest `next_retry_ns`** so the timer re-arms a
>   one-shot for that deadline instead of stopping when every row is quarantined.
>
> Retention order is unchanged from the contract: an effect stays pinned until acked (decoupled from
> the ADR 0027 journal); a discovery row stays until its terminal-proof re-scan is empty; and the
> owning record is non-evictable while either a non-terminal reservation (count > 0) or a discovery row
> references it. `UNIQUE_RESERVATION_TTL_NS` (30 min, statically asserted `≥ ROUTING_LEASE_TTL_NS`)
> only makes a `Reserved` entry *eligible* for a reclaim proof; cancel safety rests on terminal-failure
> + proof, never on the TTL. `CREATE`/`DROP CONSTRAINT` remains `NotImplemented`: only failure-injection
> / canbench coverage (slice 7) is left.
>
> **Revision 2026-06-22 #11 (slice 7 first cut — failure-injection e2e + canbench baseline landed):**
> a first PocketIC e2e suite and a write-path canbench baseline landed; the enforced lifecycle is
> verified end-to-end on real canisters and the Router-side overhead has a persisted baseline.
> `CREATE`/`DROP CONSTRAINT` **still returns `NotImplemented`**.
>
> **Revision 2026-06-22 #12 (slice 7 gate scope corrected — gate NOT yet satisfied):** Revision #11
> overstated completion. The Phase-6 gate is **not** satisfied: several ADR-required cross-canister
> failure-injection scenarios (Try-then-Router-trap, Confirm-then-trap, true concurrent same-value,
> outbox-vs-eviction, reclaim-during-retry, Release-before-Acquire) and the inter-canister outbox
> ack-round / end-to-end storage-growth canbench remain pending; the canbench baseline measures only
> the Router-local facade Try/Confirm/Cancel + reclaim scan. The slice-1–6 unit tests plus the
> existing e2e suite cover the remaining invariants. The admin guard on the
> `test_declare_unique_constraint` seam is now explicit (`auth::require_admin`), with a non-admin
> rejection test (`declare_unique_constraint_rejects_non_admin`). The across-`await` ambient
> element-id-key thread-local was removed in favour of an `element_id_key` carried explicitly on the
> execution/evaluator/materialize path, with an interleaving regression test
> (`element_id_encoding_uses_per_evaluator_key_not_ambient_state`). `CREATE`/`DROP CONSTRAINT`
> remains `NotImplemented` until the gate closes and the separate publication flip is approved.
>
> - **Test seam (no public DDL).** Because public `CREATE CONSTRAINT` stays gated, the e2e suite declares
>   constraints through a `#[cfg(feature = "pocket-ic-e2e")]` router endpoint
>   `test_declare_unique_constraint` that reaches the same admin-authorized, declare-on-empty store path
>   (`RouterStore::create_unique_constraint`) the published DDL eventually will. The public dispatch and
>   its `NotImplemented`/`Forbidden`/path-mismatch contract are unchanged (still covered by
>   `adr0030_constraint_dispatch`).
> - **PocketIC e2e (`adr0030_uniqueness_lifecycle`, `adr0030_uniqueness_recovery`).** Through the real
>   ingress (`gql_execute_idempotent`): a single-vertex constrained `INSERT` reserves, commits, pins and
>   acks its `Acquire`, and is queryable; a same-value `INSERT` is **non-retryable** (`UniquenessViolation`);
>   a value held `Reserved` by an in-flight saga (its dispatch stalled on a stopped shard) refuses a
>   competitor **retryably** (`UniquenessReservationInFlight`); `SET` on a constrained property is deferred
>   (`NotImplemented`); a `DELETE` releases the reservation so the value is reusable. Failure injection
>   stops the target shard so the no-`await` Try persists the reservation while the canonical dispatch
>   fails, then drives convergence via the two sanctioned routes: **roll-forward** (an idempotent same-key
>   retry re-dispatches, commits, and re-Confirms — exercising idempotent Confirm) and **reclaim** (the
>   abandoned saga is terminally failed and Cancelled by Driver 1 past the eligibility TTL, freeing the
>   value; the abandoned key stays terminally failed and is never re-dispatched). The same reclaim
>   convergence is re-verified **across a full canister upgrade** (`post_upgrade` re-arms the recovery
>   timer; the reservation/envelope survive in stable memory).
> - **Encoding-key fix surfaced by e2e (slice 5b correctness).** The first real-canister constrained
>   `DELETE` trapped: a DML plan's read prefix runs as a nested query phase that **set then cleared** the
>   execution element-id key to `None` on exit, wiping the key `run_wire_plans` installed; the canonical
>   tail's `Release` capture (`capture_constrained_release` → `path_vertex_element_id`) then found no key
>   and trapped. Unit tests masked it via the `cfg(test)` host-fixture fallback. The ambient
>   thread-local key was **removed entirely** (Revision #12): a per-graph `element_id_key` is resolved
>   once per execution and carried explicitly down the execution / `QueryExprEvaluator` / materialize
>   path and into the canonical capture (`path_vertex_element_id(&key, …)`), so it can never be wiped by
>   a nested phase **and** can never be corrupted by an interleaved message that installed a different
>   graph's key across an `await` (a graph canister hosts shards of different logical graphs). This
>   covers both the threaded constrained insert (`MATCH … INSERT (:Constrained {…})`) and the
>   read-prefixed `DELETE`; it is regression-tested by
>   `element_id_encoding_uses_per_evaluator_key_not_ambient_state`.
> - **canbench baseline (`crates/router/canbench_results.yml`).** Router-local facade benches (no
>   inter-canister calls): the no-`await` Try through `try_reserve_unique` (incl. the reverse-index
>   slot bump) at **1 / 16 / 256** claims, Confirm + non-terminal count decrement, the reclaim-fence
>   Cancel + count decrement, the `Acquire` ack-clear, and the bounded reclaim scan over a populated
>   table. This establishes a **Router-side** Try/Confirm/Cancel + reservation-growth baseline only;
>   the inter-canister outbox append/ack round and the end-to-end storage-growth measurement are still
>   pending (see the Phase-6 status note — the gate is not yet satisfied).
>
> **Revision 2026-06-22 #13 (slice 7 gate closed — failure-injection + outbox canbench landed):** the
> two items Revision #12 left open are now satisfied, so the **Phase-6 test/benchmark gate is met**.
> `CREATE`/`DROP CONSTRAINT` still returns `NotImplemented`: the remaining gate is the deliberately
> separate **publication flip**, which stays held for final architectural review (not a test gap).
>
> - **PocketIC failure-injection (`crates/pocket-ic-tests/tests/adr0030_uniqueness_failure_injection.rs`,
>   6/6 passing).** A `#[cfg(feature = "pocket-ic-e2e")]` router fault seam (`test_arm_fault` →
>   `InjectedFault::{TrapAfterTry, TrapBeforeConfirm}`) and a `test_force_reclaiming` seam drive every
>   ADR-required boundary on real canisters: **Try-then-Router-trap** (trap after the no-`await` Try,
>   before dispatch, rolls the reservation + envelope back with the message — the value is reusable,
>   then genuinely reserved on reuse); **Confirm-then-trap** (trap after the shard's canonical commit
>   but before Router Confirm leaves a `Reserved` entry that recovery re-Confirms, never loses);
>   **true concurrent same-value** (two in-flight ingress messages race via `submit_call`/`await_call`
>   — exactly one wins, the loser is retryable); **outbox-vs-eviction** (a pending `Acquire` GC-pins
>   the journal record so a commit-but-reply-lost write is `Confirm`ed via the pinned outbox past the
>   ADR 0027 eviction window and is never cancelled); **reclaim-during-retry** (a same-`ClaimId` retry
>   arriving while `state = Reclaiming` is fenced as `UniquenessReservationInFlight`); and
>   **Release-before-Acquire** (a `Release` seen before its `Acquire` is held, not acked, until the
>   owner is reconciled).
> - **Graph-shard outbox canbench (`crates/graph/canbench_results.yml`).** Shard-side benches the
>   Router-only suite cannot reach, through `GraphStore`: `Acquire` outbox **append** at **1 / 16 / 256**
>   effects (the per-effect pin work in the canonical segment, doubling as the **outbox storage-growth
>   baseline**), the Confirm **`Acquire` proof read** over a 256-effect outbox, the post-Confirm
>   **ack round** (unpin all 256), and Driver 2's paginated **effects-page** enumeration.
> - **Artifact separation.** canbench builds into a dedicated `CARGO_TARGET_DIR=target/canbench`
>   (`--features canbench`) so it never clobbers the `--features pocket-ic-e2e` wasm the
>   `pocket-ic-tests` build script writes under `target/`; the final gate needs no manual cross-feature
>   rebuild of a shared artifact.
> - **Lint fix surfaced by the wasm gate.** `clippy --workspace --all-targets --all-features`
>   exercises the `pocket-ic-tests` build script's nested wasm build under clippy-driver, which caught a
>   `clone_on_copy` on the wasm-only recovery timer's `Option<UniqueEffectPendingKey>` effect cursor
>   (`recovery.rs`); the redundant `.clone()` (the key is `Copy`) was removed — behavior-preserving.
>
> **Revision 2026-06-22 #14 (post-review: two failure-injection boundaries + bench baseline closed):**
> review found Revision #13's "gate satisfied" claim covered only **6** of the required boundaries.
> Two were missing and a baseline was lost; all three are now closed (the gate is genuinely met, DDL
> still `NotImplemented` pending the publication flip):
>
> - **Confirm→ack failure boundary now injected (the 7th scenario).** Revision #13's *Confirm-then-trap*
>   trapped *before* the Router's Confirm, so the `Reserved → Committed` + `pending_acquire_ack`
>   persistence and the **ack** call/response failure edge were never exercised. A new graph-shard
>   `#[cfg(feature = "pocket-ic-e2e")]` fault seam (`e2e_arm_unique_ack_fault` →
>   `InjectedFault::TrapOnUniqueAck`, guarded by the shard's control-plane/router guard) traps inside
>   `ack_unique_effects` *after* Confirm has durably committed and stamped the pending ack. The mutation
>   still succeeds (Confirm is best-effort), the `Acquire` stays pinned, and slice-6 recovery re-reads
>   the proof, re-acks the effect, and clears `pending_acquire_ack` — verified by
>   `confirm_ack_failure_is_reacked_by_recovery` (the graph **outbox length** is the observable: `1`
>   while pinned, `0` after recovery re-acks). Because an unpinned outbox alone does not prove the
>   *Router-side* marker cleared, the test closes with a `DELETE` → same-value `INSERT` round-trip: a
>   lingering pending ack would hold the `Release` (Release-before-Acquire) and refuse the reuse, so a
>   successful reuse is positive proof the marker was cleared.
> - **outbox-vs-eviction now actually surpasses the 9-day graph journal.** Revision #13's test advanced
>   only **7 days** (the router client-key TTL, ADR 0025) and swept only the **router** side, never the
>   graph mutation journal (ADR 0027, **9 days**). The rewritten
>   `outbox_pinned_reservation_survives_journal_eviction_and_is_confirmed` keeps the ack fault armed so
>   the `Acquire` stays pinned across the window, advances **past 9 days**, then actually evicts the
>   graph mutation journal (`e2e_evict_mutation_journal`, driving the real `evict_expired` /
>   `GRAPH_MUTATION_JOURNAL_RETENTION_NS`). It asserts the journal drains to **0** while the pinned
>   outbox `Acquire` and the GC-pinned router record both **survive** (decoupled pin-until-acked
>   retention), then — fault cleared — recovery re-acks via the surviving outbox and the value is
>   durably committed. This is the direct decoupling proof Revision #13 only claimed.
> - **Router canbench baseline restored.** A pattern-filtered `canbench --persist` had overwritten
>   `crates/router/canbench_results.yml` with only the slice-7 unique benches, dropping
>   `bench_layout_router_stable_reopen_touch`. All **8** router benches were re-run unfiltered and
>   persisted, restoring the layout baseline alongside the slice-7 set.
>
> **Revision 2026-06-22 #15 (scope correction: DROP needs its own slice, not just publication):**
> review (`APPROVE WITH CHANGES`) accepted the Slice 7 / Phase-6 Try/Acquire/Confirm/Release/recovery
> gate but corrected an overstated publication scope: `CREATE` and `DROP CONSTRAINT` are **not**
> symmetric. `CREATE` may be published on its own once the deliberate publication decision clears final
> review. `DROP` is **not** publishable yet — `RouterStore::drop_unique_constraint`
> (`facade/store/catalogs.rs`) removes only the `ROUTER_UNIQUE_CONSTRAINTS` **definition**; it does
> **not** touch `ROUTER_UNIQUE_RESERVATIONS`, and the name→id interning in
> `ROUTER_CONSTRAINT_NAME_CATALOG` survives the drop. Two unimplemented behaviors block publication:
>
> - **Reservation lifecycle on DROP.** Dropping a constraint must invalidate its reservations and
>   drain any in-flight saga that could still emit an `Acquire`/`Release` for it; today `Committed`
>   reservations (and any pinned outbox effects) persist indefinitely after the definition is gone.
> - **`ConstraintNameId` reuse hazard.** Because the dropped name's id is retained, a later
>   `CREATE CONSTRAINT` reusing that name (e.g. on a *different* label) re-interns the **same**
>   `ConstraintNameId`. Reservations are keyed `(graph_id, ConstraintNameId, encoded_value)`, so stale
>   reservations from the dropped constraint would falsely reject values under the new one. A safe DROP
>   must either purge those reservations or fence id reuse until they are gone.
>
> These are deferred to a dedicated **DROP-lifecycle slice** (draining/purge state machine + regression
> tests: drop-with-live-reservations, drop-then-recreate-same-name-different-label, drop-during-saga).
> Until it lands, `DROP CONSTRAINT` stays `NotImplemented`; `drop_unique_constraint` remains
> store-test-only and is documented as definition-only. The Phase-6 gate itself is unaffected.

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
    into the reservation at Try time. Each entry pins the **canister identity** (`{ shard_id,
    graph_canister }`), not a bare graph-local shard id: a shard id can be unregistered and reused by
    a *different* canister, so a bare id would let the proof query the wrong (new) canister, see no
    `Acquire`, and unsafely Cancel a claim that committed on the old one. The reclaim/recovery proof
    reads it **from the reservation itself**, so a GC'd `RouterMutationRecord` can never strand the
    proof (see Timeout and Upgrade/retention).
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
6. **Cancel / steal** only when **both** hold: (a) the owning mutation is **terminally `Failed`** in
   its `RouterMutationRecord` (so no in-flight or future dispatch can still land an `Acquire` after
   this proof's absence read), **and** (b) **all** shards in `proof_scope` are reachable and **all**
   report the entry absent. Absence alone is insufficient — an already-sent canonical dispatch could
   arrive and commit after the proof. If the record is **missing or non-terminal**, do **not** cancel:
   revert to `Reserved` (keep `reclaim_generation = g`) and **hold**. (A non-terminal reservation's
   `RouterMutationRecord` is therefore retained as its terminal proof and is non-evictable until the
   reservation leaves `Reserved`/`Reclaiming`.)
7. if **any** shard is unreachable or its answer is unknown → revert `state = Reserved` **keeping
   `reclaim_generation = g`** (never decremented or reset) and **hold** (no cancel, no steal),
   retrying the proof later under a fresh, higher generation.

An **orphan `Acquire`** (a pinned `Acquire` effect whose reservation is absent) must **never** be
acked and must **not** fabricate a reservation: the evidence is preserved and surfaced as a
persistent diagnostic, because under the generation-fenced proof this state should not occur and
silently discarding commit evidence would be unsafe. The unified effect-recovery driver (Driver 2)
classifies a reservation-less `Acquire` as an orphan **only after** confirming the owning mutation is
terminal (its effect generation has finished — a still-non-terminal mutation is held, not orphaned),
then **quarantines** the discovery row (`state = Quarantined`, a long `next_retry_ns` backoff, the
diagnostic recorded) so the orphan is retained and re-checked without hot-looping the recovery timer.

"Expired/abandoned" in the Try and Recovery rules means exactly the generation-fenced outcome of
step 6. A reservation whose mutation reached canonical commit is always advanced to `Committed`,
never force-expired.

### Upgrade / reopen and bounded retention

- `UNIQUE_RESERVATIONS` is in Router stable memory; it survives upgrade. `post_upgrade` re-arms the
  recovery timer, which reconciles reservations against the pinned outbox on reopen.
- **GC ordering safety.** The reclaim proof's *read* depends only on the reservation's own
  `proof_scope` and the pinned outbox, never on the `RouterMutationRecord`. The **Cancel** decision
  additionally consults the record for the terminal-`Failed` predicate, but **fails safe**: a missing
  or non-terminal record makes recovery **hold**, never cancel. As both a safety and liveness measure,
  a `RouterMutationRecord` referenced by any reservation still in `Reserved`/`Reclaiming` is
  **non-evictable** until that reservation leaves those states (consistent with ADR 0029 Phase 4's
  exclusion of non-terminal sagas from TTL eviction), so the terminal proof is available when needed.
- **Outbox retention.** Unique-effect outbox entries are pinned until acked and so are *not* governed
  by the ADR 0027 9-day journal eviction; they are pruned only after the Router has durably advanced
  and acked the matching reservation. This keeps the proof total while remaining bounded (one un-acked
  entry per in-flight unique effect).
- **Effect work discovery (unified).** Effects the inline post-commit path left pinned — a held
  `Release` (Release-before-Acquire), an `Acquire` whose inline Confirm/ack was lost, or an orphan
  `Acquire` — are rediscovered through a Router-owned, ADR-0030-independent **unified discovery index**
  keyed in **row form** `(graph_id, mutation_id, shard_id) → PendingEffectRecord` (a row per target,
  so scans are bounded and values stay small). The row covers **both** `Acquire` and `Release`
  dispatches (not Release-only), and its value is a **versioned record**
  `{ schema_version, canister, client_key, state: Active | Quarantined, next_retry_ns, attempts,
  diagnostic? }`: the pinned `canister` is the row's immutable identity (so recovery reaches the exact
  canister even after the shard is unregistered/reused), and `client_key` resolves the owning
  `RouterMutationRecord` for any effect kind (a `Release`/orphan owns no reservation, so the reverse
  index cannot resolve them). Registration runs **before the first dispatch `await`** so it co-commits
  with the reservation/envelope, and is **fail-closed** — re-registering a key to a different
  `canister` or `client_key` traps. A row is removed **only after** the target's all-effects page —
  re-read from `cursor = None`, never inferred from a tail empty page after a failed ack — comes back
  empty (all effects acked). While any row exists for a mutation, its owning record is GC-pinned and
  its un-acked effects always remain rediscoverable.
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
- **Non-finite floats (`NaN` / `±∞`):** `NaN ≠ NaN` in GQL, so a `NaN` has no stable key identity;
  `±∞` likewise has no canonical finite key. Every non-finite float is **rejected** as a unique key
  (it cannot participate in a uniqueness constraint).
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
- Add the slice-6 recovery indexes (update the
  [stable-memory inventory](../storage/stable-memory-inventory.md)): the reservation reverse index
  `MutationId → { client_key, nonterminal }` (GC-pins the owning record while non-terminal
  reservations remain) and the unified pending-effect discovery index
  `(graph_id, mutation_id, shard_id) → PendingEffectRecord` (versioned value; GC-pins the owning
  record while rows remain). Both are Router-local and additive; existing graphs are unaffected.
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

> **Status (Revision #13, corrected by #14): the test/benchmark gate IS satisfied.** Every
> ADR-required scenario below is covered by a passing unit, PocketIC e2e, or canbench artifact. The DDL
> nonetheless stays `NotImplemented`. For `CREATE` the remaining step is the deliberately separate
> publication flip, held for final architectural review — not a test or benchmark gap. `DROP` is
> **not** publishable on the gate alone: it needs a dedicated lifecycle slice (reservation
> invalidation + saga draining + `ConstraintNameId`-reuse guard, with regression tests) — see
> Revision #15.
>
> **Covered by PocketIC e2e** (`crates/pocket-ic-tests/tests/adr0030_uniqueness_lifecycle.rs`,
> `adr0030_uniqueness_recovery.rs`):
> - constrained INSERT commit + committed-value duplicate rejection;
> - canonical-write-failure-after-Try leaves a live `Reserved` value that refuses a competitor
>   retryably;
> - acquiring `SET` / full replacement deferred (rejected at admission);
> - `DELETE` releases the reservation (value reusable);
> - held-dispatch roll-forward via idempotent retry;
> - abandoned reservation reclaimed after TTL (terminal-failure + Cancel);
> - upgrade-reopen reconciliation across a mid-saga upgrade;
> - the `test_declare_unique_constraint` seam is admin-gated (non-admin → `NotAuthorized`).
>
> **Covered by PocketIC failure-injection e2e**
> (`crates/pocket-ic-tests/tests/adr0030_uniqueness_failure_injection.rs`, 7/7 passing):
> - Try-then-Router-trap (a trap after the no-`await` Try, before dispatch, rolls back the reservation
>   with the message — the value is reusable, then genuinely reserved on reuse);
> - Confirm-then-trap (a trap after the shard's canonical commit but before Router Confirm leaves the
>   reservation re-confirmable by recovery, never lost);
> - Confirm→ack-failure (a graph-shard trap **inside** `ack_unique_effects`, *after* Confirm has
>   durably moved the reservation `Reserved → Committed` and stamped `pending_acquire_ack`, leaves the
>   `Acquire` pinned; slice-6 recovery re-acks it and clears the pending marker — the
>   `confirm_ack_failure_is_reacked_by_recovery` test, observed by the graph outbox length (`1` while
>   pinned, `0` after re-ack) **and** a closing `DELETE` → same-value `INSERT` round-trip: the outbox
>   length alone cannot prove the *Router-side* `pending_acquire_ack` cleared, since a lingering marker
>   would hold the `DELETE`'s `Release` (Release-before-Acquire) and refuse the reuse, so a successful
>   reuse is positive proof the marker was cleared);
> - true concurrent same-value conflict (two in-flight ingress messages race via
>   `submit_call`/`await_call`: one wins, the loser is retryable) — distinct from the sequential
>   duplicate test;
> - outbox-vs-eviction (the ack fault keeps the `Acquire` pinned while time advances **past the 9-day**
>   graph mutation-journal retention, ADR 0027; the graph journal is then actually evicted via
>   `e2e_evict_mutation_journal` and drains to 0, yet the pinned outbox `Acquire` and the GC-pinned
>   router record both **survive** — decoupled retention — and the value is `Confirm`ed via the
>   surviving outbox, **never** cancelled);
> - reclaim-during-retry (a same-`ClaimId` retry arriving *during* a reclaim proof is fenced by
>   `state = Reclaiming`);
> - Release-before-Acquire (a `Release` observed before its `Acquire` is held, not acked, until the
>   owner is reconciled).
>
> **Covered by slice 1–6 unit tests** (router/graph `--lib`): multi-claim Try atomicity (`Err`
> mutates nothing), ClaimId stability across Try→Acquire, Release-by-`owner_element_id` matching,
> per-effect ack independence, stale-receipt isolation, reclaim generation-fence + ABA retention,
> GC-pin reverse-index count, value equivalence / non-finite & oversized rejection, intra-mutation
> duplicate rejection, and the no-ambient-key (`element_id_key`) interleaving regression
> (`gleaph-graph` `element_id_encoding_uses_per_evaluator_key_not_ambient_state`).
>
> **Covered by canbench** (`crates/router/canbench_results.yml`, `crates/graph/canbench_results.yml`):
> the **Router-local** reservation TCC + slice-6 recovery indexes through the production facade
> (Try/Confirm/Cancel incl. reverse-index and non-terminal count; reclaim scan — see
> `crates/router/src/bench.rs`), **and** the **graph-shard** unique-effect outbox: `Acquire` append at
> 1 / 16 / 256 effects (the storage-growth baseline), the Confirm `Acquire` proof read, the
> post-Confirm ack round, and Driver 2's paginated effect enumeration (see `crates/graph/src/bench`).

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
- **value equivalence**: GQL-equal values share a key; non-finite floats (`NaN`/`±∞`) and
  over-length values are rejected;
  `NULL`/missing makes no claim;
- canbench evidence for the Try/Confirm/Cancel overhead on the write path, the outbox ack round, and
  reservation-table storage growth. *(Satisfied: the Router-local Try/Confirm/Cancel facade overhead
  and reclaim scan are benched in `crates/router/src/bench.rs`; the graph-shard outbox append/ack
  round and the outbox storage-growth baseline are benched in `crates/graph/src/bench` — see the
  Phase-6 status note above.)*
