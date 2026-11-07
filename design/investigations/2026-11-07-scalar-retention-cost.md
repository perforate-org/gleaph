
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
