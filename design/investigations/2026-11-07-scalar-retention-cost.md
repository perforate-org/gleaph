
## Correction (same day): the trap shapes above are impossible, and B1 does not fix anything

The section above derived two failure shapes from the write ordering in `gql_run.rs` (a trap before the
`Incomplete` marker leaves committed state with no evidence; a trap after it leaves an `Incomplete`
entry that ages out). **Both are impossible**, and the derivation is left in place only so the
correction has something to point at.

ADR 0029 states the rule the whole design rests on: *"An Internet Computer canister commits one
message-handler execution atomically. A trap rolls back"* (`design/adr/0029-...md:10`, repeated at
ADR 0045:148 and in the ledger). A trap inside Graph's own `execute_plan` message therefore discards
every write that message made - canonical effects **and** journal entries together. Graph cannot
commit state without its journal entry, so "absent evidence" can never mean "a commit may have
happened in this message".

That leaves exactly the shape the PocketIC reproduction already recorded:

1. Graph completes the update, replies `Completed` with the exact count;
2. the **Router's** callback fails (the probe injects fault 11 there) - `Router` stays
   `CanonicalPending`, Graph stays `Completed`;
3. nothing adopts that evidence for the nine days until Graph's GC ages it out;
4. a client retry then finds no evidence and re-executes.

Step 3 is the only place this can go wrong, and it is **operational, not traffic-driven**: the
autonomous recovery timer scans non-terminal records that carry a dispatch envelope
(`idempotency.rs::scan_recoverable_mutations` - a scalar record qualifies, it has shards) and
`capture_scalar_mutation_outcome` adopts the `Completed` entry, so a healthy driver closes the record
in seconds. The probe had to **pause** Router recovery to model "unavailable outcome capture"; that
pause, not load, is what opens the window. The other ways to hold it open for nine days are the same
kind: the driver cannot reach the shard, or its bounded cursor never reaches the record.

**Consequence for the repair choice: B1 is withdrawn.** Moving the `Incomplete` marker to admission
would not have addressed the defect (the evidence in the failing shape is `Completed`, written by a
message that then succeeded), and it would have introduced a fail-closed stuck state for a hazard that
does not exist. The fix remains what the ledger's next decision says: **A1** (retire the evidence only
when replay permission ends) or **A3** (absence while replay is still permitted is a refusal, never a
first attempt).

Frequency, restated: `lost Router outcome capture` x `no adoption for nine days` x `client retry after
the evidence is evicted`. The middle factor is an operational condition, so with a healthy recovery
driver the expected rate approaches zero even under heavy mutation traffic; it is not a per-request
probability.

## Consequence found while landing the repair: a permanently stuck record keeps its evidence

Writing scalar journal entries `Active` moves the journal's only reclamation trigger from *age* to *retirement*, and a
record can only be retired once it reaches a terminal state. That is the intended coupling for the defect this repair
targets - a captured outcome that nobody adopted - but it has a second, narrower consequence:

- **A saga that never reaches terminal keeps its Graph journal entry forever.** Before this change, the nine-day age
  sweep erased it; now it stays `Active` until some owner retires it, and no owner will, because retirement requires a
  terminal record.
- Non-terminal is the normal state of a saga that is *waiting* (a paused target, an unreachable shard, a held
  reservation). Those are exactly the cases the recovery driver reports through `mutation_status`, and they are supposed
  to converge; the growth is therefore bounded by records that are **stuck**, not by records that are merely slow.
- The bound is small but real: about 47 B + key per stuck record, held indefinitely.

**This is not a regression against the old behaviour's safety, but it is a new retention obligation**, and it is why the
repair also needs an answer for abandoned sagas: either the Router reaches a terminal verdict for every saga it admits
(the recovery driver's own convergence, plus the reclaim reconciler for reservations), or a scalar entry that its owner
has abandoned must be retired explicitly rather than aged out. Recording it here rather than deciding it in code: the
ledger entry now carries it as the next question, because the answer changes what "an abandoned mutation" means.

**A concrete test consequence**, found by reading rather than running: `adr0030_uniqueness_failure_injection`'s
`outbox_pinned_reservation_survives_journal_eviction_and_is_confirmed` asserts the journal drains to zero while an
`Acquire` is still pinned. Its mutation is a scalar one whose record stays non-terminal (the ack is trapped), so under
this contract the entry is `Active` and cannot drain. The assertion's premise - "journal retention is independent of the
Router obligation" - is exactly what this repair changed, so that test needs a new premise rather than a relaxed
assertion; it was not edited blind, and the PocketIC suite was not run in this slice.

## Decision: an unresolved mutation is an in-doubt transaction, resolved by an operator

The remaining case - a saga that never reaches terminal - was settled as follows, in the vocabulary databases use for the
same problem.

| Database behaviour | Here |
| --- | --- |
| prepared / in-doubt transaction is never GC'd | the record and the Graph evidence are retained until a decision exists |
| the DBA resolves it, and the resolution is durable | `resolve_in_doubt_mutation` records a terminal failure with its reason |
| evidence is released only after resolution | the recorded decision releases the key; the Graph retirement stays best-effort |

**Automatic resolution was rejected, and the reason is structural.** "Graph has no entry" is *not* proof that nothing was
written: a dispatch that is still in flight has not journaled yet, so an automatic abort could report a failure and a
moment later the Graph would commit it. Resolving automatically therefore needs a Graph-side fence that makes a late
commit for a retired mutation id impossible. Until that fence exists, only an operator can decide, and the Router's job is
to make the state visible and the decision durable - not to guess.

**One owner.** `InDoubtView` (crates/router/src/facade/store/in_doubt.rs) holds the rule; the client status, the operator
listing, the resolution endpoint and the retirement driver all ask it, so the answers cannot drift (ADR 0100). The
threshold (`UNRESOLVED_AFTER_NS`, one day) is a *reporting* line, not an expiry: nothing is evicted or aborted when it
passes.

**What remains**: the E2E proof (PocketIC 11-fault rerun) and, if automatic resolution is ever wanted, the Graph fence.
