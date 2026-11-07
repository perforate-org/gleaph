# Scalar journal retention: lost-outcome reproduction

Status: Experimental — retention failure still open; Graph continuation and Router capture/send/preparation-write prerequisites implemented
Date: 2026-09-22
Anchor timestamp: 2026-09-22 10:54:06 UTC +0000
Inspected HEAD: `9a64049f47fc07542446d651d4b09421636b73c5` (dirty working tree)
Tracking: [GAP-2026-09-22-001](../implementation-gaps.md#gap-2026-09-22-001--recoverable-plan-execution-can-outlive-graph-journal-retention)

## Result

The [source-derived gap](2026-09-22-bulk-edge-set-outcomes.md#current-scalar-retention-gap) is reproduced
through real Router → Graph calls in PocketIC. Both scalar reservations remained `CanonicalPending`
after the callback trap, not merely pending at the bulk chunk level. After nine days plus one hour,
the actual Graph sweep erased their completed outcomes while Router retained the same mutation IDs.
Identical Append then executed canonical SET again.

| Original Graph outcome | Mutation ID | State immediately before retry | Actual identical retry | Required replay |
| --- | --- | --- | --- | --- |
| Applied count 1 | 6 | `nick = "later-write"`, set by another mutation | Returned `Updated(1,1)` and restored `"original-write"` | Same successful result, preserve `"later-write"` |
| Applied count 0 | 9 | Original target now satisfies its unchanged property policy; `nick` absent | Returned `Updated(1,1)` and wrote `"forbidden-write"` | Original `NotFound("bulk-load update target is no longer eligible")`, `nick` remains absent |

`Updated(1,1)` abbreviates `next_offset=1, updated_row_count=1`, at chunk index 0. IDs above are
observations from this fixture, not values hard-coded into the assertions. This is replay failure,
not an authorization bypass: the second zero-row execution was eligible under the unchanged grant,
but should never have executed after its original durable zero.

The intended contract assertion failed with exit 101: **0 passed, 1 failed, 13 filtered out**.
That is a successful reproduction, not a green regression or a repaired feature. There is no claim
of a production incident, an ordinary-GQL lost-callback runtime reproduction, or an explanation for
0364's native failures.

## Fixture and controls

The probe (measurement dump, not retained) temporarily attaches as a child of
[`adr0057_durable_bulk_load.rs`](../../crates/pocket-ic-tests/tests/adr0057_durable_bulk_load.rs), reusing
its existing setup, command, value-encoding, and canonical-query helpers.

1. Install one single-shard federation: one PocketIC instance and three canisters (Router, Index,
   Graph). Seed positive, blocked, and unrelated-control vertices. Register a real conditional
   MATCH grant plus READ/UPDATE for a non-admin zero-row caller.
2. Complete an ordinary scalar control mutation. Its Graph journal must exist before expiry.
3. Call the existing exact bulk receipt-GC seam on the unexpired completed seed job; assert zero
   removals. This also pauses autonomous Router recovery, modeling unavailable outcome capture.
   No scalar record is forged, and no new fault or timer endpoint is introduced. This is not a test
   of autonomous recovery liveness or a claim that ordinary timers leave these particular rows idle.
4. For each distinct bulk job, arm existing fault 11 and submit one row. Require the exact injected
   callback-trap diagnostic. Clear the fault; require Router `CanonicalPending`, Graph `Completed`,
   and exact Graph count 1 or 0. Assert the positive value exists and zero caused no property write.
5. Advance nine days plus one hour, run the real Router key sweep, and require removals. Run the real
   Graph sweep: journal length **4 → 0**, including unrelated completed scalar control **ID 3**.
   Point reads confirm both row outcomes absent; Router still reports the original IDs pending.
6. Apply independent canonical mutations to the positive value and zero-row eligibility. Retry the
   original chunks under their original callers. Check result, unchanged mutation ID, and exact
   canonical property. Collect both final violations before asserting, so positive failure cannot
   mask zero failure. A correct row count alone cannot satisfy the positive oracle.

No assertion was relaxed. The first run stopped before the fault boundary because its grant literal
`"retention-eligible"` exceeded the existing 15-byte conditional-policy bound in
`gql_grants.rs::literal_to_predicate_literal`. Only that fixture literal and its references changed
to `"eligible"`. The production bound, policy and replay assertions stayed unchanged.

The temporary module attachment was removed after the run; the owning test file is byte-identical
to its initial snapshot. The red probe is retained as an executable investigation artifact, not an
ignored/`should_panic` test or a passing test that enshrines the defect. A repair must promote its
contract into normal owning tests. Existing ADR 0057 tests, including the eight-day Router-retention
contract, remain unchanged.

## Enforcing repair boundary

**Not implemented or accepted as a final protocol.** Existing ownership remains appropriate:
Graph region 39 owns dedup evidence; Router's existing mutation record owns canonical resend
permission, captured result, projection and retirement progress. No second journal, occurrence ID,
new MemoryId, or portable GQL/planner change is justified.

The following table records the source inspected for the original reproduction, before the
[Graph](#graph-replay-and-continuation-prerequisite) and
[Router](#router-count-capture-and-actual-send-prerequisite) prerequisite follow-ups. A coherent
explicit-retirement repair must cover every listed boundary:

| Boundary | Existing source behavior | Required proof/change before retiring scalar evidence |
| --- | --- | --- |
| Graph representation | `plan_exec.rs::validate_graph_mutation_journal_fields` requires `PlanExecution/NotApplicable`; scalar constructors use that state | Explicit scalar lifetime representation and exhaustive codec/reader validation; ordered insert receipts are not scalar receipts |
| Graph replay publication | `gql_run.rs::run_wire_plans_inner` returns a saved outcome, but `run_wire_plans_last_read_row_count` still calls the completed-journal writer | Replay must preserve the terminal result and retirement state/timestamp; changing a constructor default to Active alone would resurrect retired entries |
| Graph continuation | Journal lookup precedes `read_phase_seed_rows(...).await`; scalar/bundle execution uses the general read executor with an index handle | Establish which admitted paths can actually suspend, then enforce an owner-bound fence at every resumed canonical boundary; Rust `.await` syntax alone does not prove an IC suspension or its absence |
| Router actual send | `prepare_mutation_for_batch` checks completion before dispatch construction; later preparation awaits and `execute_prepared_mutation` dispatches groups/chunks | Recheck durable per-shard resend permission at the actual outbound call after every possible suspension, including the batch endpoint |
| Router outcome capture | `record_router_mutation_shard_completed` writes count/completion; projection completion can immediately compact all shard envelopes | Durable exact outcome closes resend monotonically; retain target/result identity until acknowledged retirement. Stale callbacks must not rewind projection or change a captured count |
| Projection/recovery | Inline success, inline journal recovery, `reconcile_router_mutation_projection`, and `recover_mutation_record` advance scalar progress | All must converge on the same capture/retire transitions; the autonomous driver must remain canonical-write-free |
| Bulk settlement | `classify_update_row` reads Router and Graph evidence; Abort distinguishes zero, positive and unresolved | Graph retention must cover the row's actual resend lifetime, including AbortPending and pending child ownership; preserve vertex zero semantics |
| GC/pressure | Scalar GC is age-only; ordered Active evidence is pinned | No age/LRU/capacity eviction while replay remains possible; define admission bounds and measure retained evidence and retirement cost |

The no-external-call **mutation tail** does not itself establish quiescence of an entire earlier
request. Likewise, request ordering is not callback ordering. Before adopting the ordered-family
retirement argument, prove no earlier accepted scalar request can resume canonical work after
retirement/GC. This continuation question was source-inspected, not exercised by this probe; the
probe's exact bound-vertex path does not establish safety for every ordinary plan or batch.

Minimum coherent direction: extend the existing journal/reservation lifetime protocol, with durable
capture → permanently closed canonical resend → required projection → retirement pending → exact
acknowledgement → compaction/retirement-age GC. Exact scalar receipt/fingerprint fields, admission
bounds, and continuation fencing remain implementation prerequisites. No schema tags are assigned
by this investigation. Incompatible state must use a coherent fresh development federation;
reinstall alone is not proof that old requests are quiescent.

Alternatives considered:

- **Increase TTL:** smallest diff, but cannot protect indefinite pending replay; rejected.
- **Pin every scalar forever:** stops this deletion path, but reinstates unbounded steady-state
  retention without retirement/admission; not installed as a partial repair.
- **Graph-enforced execution deadline/fence:** potentially bounds retention, but must also fence
  suspended callbacks and permanently refuse expired IDs. It changes indefinite resume semantics;
  it is not equivalent to returning an ordinary retryable error when the journal is absent.
- **Existing-owner explicit retirement:** preferred direction for preserving resume semantics,
  conditional on the proofs above. Costs additional retained state/RPCs and needs exact measurements.
- **New generic transaction subsystem:** unnecessary ownership expansion; rejected.

Abort settlement, retirement/lost acknowledgement, delayed requests, upgrade/reopen, ordinary scalar
and transport-batch behavior, codec conflicts and admission/benchmark limits remain unvalidated.
The recommendation is to implement one coherent lifetime slice, not publish a retention-only fix
and assume later code will make it safe.

## Reproduction and validation

From the repository root, temporarily add this child module beside the imports in the existing
ADR 0057 integration-test file:

```rust
#[path = "../../../design/investigations/artifacts/2026-09-22-scalar-journal-retention-probe.rs"]
mod scalar_journal_retention_probe;
```

Run the ordinary command, then remove only that exact attachment (do not restore the entire dirty
file from Git):

```sh
cargo test --offline --locked -p gleaph-pocket-ic-tests \
  --test adr0057_durable_bulk_load \
  scalar_journal_retention_probe::lost_scalar_outcomes_survive_graph_gc -- --exact --nocapture
```

Before attachment the target has 13 tests and 13 install-helper calls; with it, 14/14. The selected
command runs one test, one constructor, one federation install and three canister installs. Across
the failed setup run and corrected run, two fixtures/six installs executed; neither did an upgrade.
Normal federation Wasm building remained enabled and used the shared targets; no skip-build,
external-terminal, serial, or cold-target substitute was used.

| Command / scope | Terminal evidence |
| --- | --- |
| `cargo test --offline --locked -p gleaph-graph --lib scalar_completed_journal_replay_returns_count_without_re_executing` | Passed: 1 test, 1160 filtered; 2.34 s total / 0.17 s test runtime. Retained-journal control only |
| PocketIC command above, first run | Failed before reproduction: conditional-policy fixture literal; exit 101; 84.08 s total / 16.19 s runtime |
| Same PocketIC command, corrected literal | Failed at intended final replay assertions for **both** cases; exit 101; 95.64 s total / 19.09 s runtime; 13 filtered |
| `rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-22-scalar-journal-retention-probe.rs` | Passed |
| Scoped clippy | Not run: the unchanged PocketIC dependency lint blocker recorded by 0364 remains outside this diagnostic; no lint pass claimed |
| Canbench / full Graph / full PocketIC / Abort / retirement / upgrade | Not run; no production behavior changed and the repair is not implemented |

Total long-command observation: **182.07 s**. Compiler: `rustc 1.100.0-nightly (574ff7d98 2026-09-14)`.
Manifest/future-compatibility notices and the existing CLI unused-field warning remain in transcripts.
Elapsed totals include runner/process teardown, not merely Cargo's build and test timings.

Evidence pointer: `$TMPDIR/gleaph-scalar-retention-current`, containing snapshots, full transcripts,
command JSON, measured-source hashes, the removed attachment diff and scope/document checks.
No dependency, production Rust, stable region, codec, API, benchmark artifact, HEAD/index, or unrelated
diff was changed. The investigation and reproduction artifact are new; relevant design caveats were
updated. This is a same-assistant diagnostic review, not independent or security approval.

## Reproduction review

Encapsulation and separation are preserved: the probe uses existing public/test seams; Graph and
Router stores are not edited or forged. The expected invariant is checked against actual durable
outcomes, retained IDs and canonical values. No domain rule or data owner is duplicated. Three local
transport/status helpers (at most three parameters each) support one shared lifecycle fixture; no
framework, production abstraction or new dependency was added. The only obsolete code is the
removed temporary module attachment.

**Verdict:** approve the diagnostic evidence; production repair remains blocked on the explicit
lifetime/continuation contract. This checkpoint does not close GAP-2026-09-22-001.

## Graph replay and continuation prerequisite

Follow-up anchor: **2026-09-22 12:18:58 UTC +0000**. The bounded
[0367 implementation plan](../../plans/0367-scalar-mutation-retirement.md) implements only the Graph
prerequisite described in [ADR 0027](../adr/0027-graph-mutation-journal-retention.md#graph-replay-and-continuation-prerequisite-implemented-2026-09-22).
The full lifetime repair is **not implemented**. Router code, Graph retention predicates, codecs,
regions, public APIs, fault seams and the original PocketIC reproduction artifact remain unchanged.
The old >nine-day lost-capture probe was not rerun or promoted as a repaired regression.

### Implemented and tested boundary

- Scalar terminal replay returns before journal/outbox republication. A new byte-for-byte oracle
  on the existing scalar test first failed because replay replaced native fixture timestamp 123
  with 0; the fixed path preserves the complete entry and canonical state.
- The existing journal owner fences actual future re-entry. Native tests cover a newly published
  count 0/1, conflicting incomplete/ordered-family evidence, unchanged own progress across three
  suspensions, exact/one-over nine-day time bounds, backward time and near-`u64::MAX` time. Another
  test actually runs Graph GC between polls and proves a stale continuation cannot write after
  the journal disappears. The `None` identity case checks the whole-transport-request time bound.
- The real wire-plan executor is manually polled against the existing native Property Index mock.
  Its equality lookup really returns Pending. With no intervening outcome, the control reaches
  SET; with saved 0/1, it preserves the later property value and exact journal. These cases run both
  without an earlier canonical segment and with an INSERT prefix. They do not claim a whole-bundle
  zero-write/rollback guarantee: a committed prefix remains committed.
- The prefix case exposed a necessary cleanup issue: postings were still heap-only while the
  later read waited. Its intended oracle failed before the fix. Each canonical segment now writes
  its collected intents to the existing outbox before later suspension; the test checks an empty
  heap queue, durable mutation-owned intents, and no duplicate publication on replay.
- Removing only the scalar guard's mutation identity (`replay_id` → `None`) made the wire test fail
  on the saved journal oracle. The one-line mutant was reversed exactly; all six source hashes
  match the measured implementation, and the restored focused test passes.
- The batch handler retains exact `[3,0]` replay counts, rejects different cardinality or missing
  current progress without writes, and no longer manufactures legacy synthetic-zero results.
  Whole-batch suspended execution is source-wired to the shared guard; it is not a new PocketIC
  delayed-callback/GC race demonstration.

The fixture-extension compile attempt initially used `u64::from(VertexCount)` and failed because
that owner type wraps `u32`. It was corrected to the owner's `u32` conversion before the intended
prefix-intent runtime failure was observed. This compile failure is not counted as a behavioral red.

### Terminal validation

All Cargo commands use `--offline --locked` and the ordinary shared targets.

| Scope | Result |
| --- | --- |
| Initial replay-byte regression | Expected failure, 0 passed / 1 failed, exit 101; 15.17 s |
| Graph journal owner (`--lib facade::store::label_stats_delta::tests`) | **11 passed**, 1156 filtered; 31.80 s |
| Graph GQL owner (`--lib gql_run::tests`), final | **48 passed**, 1119 filtered; 14.30 s |
| Graph handlers (`--lib canister::handlers::tests`), final | **22 passed**, 1145 filtered; 2.38 s |
| Shared mock consumers (`--lib plan::query::executor::scan::tests`) | **97 passed / 7 failed**, 1063 filtered, exit 101; 2.14 s |
| Graph clippy (`-p gleaph-graph --lib --tests -- -D warnings`) | **Passed**; 9.65 s |
| PocketIC ADR0057 `bulk_load_update -- --nocapture` | **9 passed**, 4 filtered; 272.42 s total / 94.86 s test runtime |
| Graph `canbench graph_exact_vertex_set` | Both cases completed; 125.05 s; no baseline persistence |
| Restored wire-continuation test after mutant | **1 passed**, 1166 filtered; 12.35 s |

The seven failed scan tests already appear as failures in the earlier 0364 verification log at
`verification-20260922T051438Z/native.log`: six edge-IN-list/range cases plus the parsed edge-range
path case (the full exact list is retained in `scan-failure-comparison.json`). They were not repaired,
skipped or weakened. Consequently 0367 remains **open at validation**, not fully validated. The
whole Graph suite and the separate PocketIC dependency lint blocker were not rerun here.

PocketIC reused the nine existing update fixtures: nine constructors, eight single-shard
federations plus one two-graph federation, **29 canister installs**. No new E2E fixture or endpoint,
serial substitute, skipped federation build or external terminal was used. These passes cover the
existing update/Abort/retry behavior, not the missing scalar retirement/upgrade protocol.

### Measured cost and limits

| Exact vertex SET fixture | Previous 0364 measurement | This measurement | Approximate change |
| --- | ---: | ---: | ---: |
| 1 decoy | 146.14 K instructions | 153.69 K | +5.2% |
| 1024 decoys | 225.16 K instructions | 232.71 K | +3.4% |

Both still report zero heap-page growth and eight stable pages. The approximate increase is the
same **7.55 K instructions** in both cases. The displayed mutation-tail and journal-commit scopes
are unchanged; the patch adds the request guard and per-segment intent flush outside those scopes.
The increase is approximately constant across these two fixtures; this does not establish its
cause or scaling beyond them. This is not an exact attribution or matched-source/toolchain A/B
experiment. No nested scope subtraction, throughput claim or user cost acceptance is inferred. The required
safety checks were retained; no speculative optimization or benchmark-baseline update was made.

These synchronous benchmarks do not measure suspended-callback cost or peak heap. On an actual
suspension the guard retains a full journal snapshot to compare at re-entry. Zero heap-page growth
is not zero allocation, and no total in-flight state/admission bound is claimed. No persisted
journal bytes or memory-region policy changed. The retained benchmark Wasm and its hash are in the
evidence directory; saved `canbench_results.yml` is unchanged.

Evidence pointer: `$TMPDIR/gleaph-scalar-retirement-current`. All 0367 long commands terminated normally;
aggregate observed time is **525.47 s**, including intentional red/mutant and fixture-compile runs.
The source diff, initial snapshots, full logs/command JSON, hash restoration proof, historical
failure comparison and measured Wasm are retained. Review is by the same assistant, not independent
or security approval. At this Graph-only checkpoint, the next repair remained Router
send/capture/projection/acknowledged retirement coupled to scalar Active evidence. The Router
follow-up below implements another prerequisite, not the original positive/zero GC repair.

## Router count capture and actual-send prerequisite

Follow-up anchor timestamp: **2026-09-22 14:36:09 UTC +0000**.
Status: **Implemented prerequisite; scoped validation complete; measured read cost is material; end-to-end lifetime gap still open**.
Plan: `plans/0368-router-scalar-dispatch-fence.md`.

### Implemented boundary

The [owning contract](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md#scalar-count-capture-and-actual-sends)
uses the existing Router mutation record, without stable schema, public RPC or Graph GC changes:

- `record_scalar_shard_progress` replaces the separate completion/projection writers. It checks the
  reserved mutation ID and family, preserves equal repeated counts and projection bytes, rejects
  conflicting retained counts, requires capture before projection, and refuses aggregate overflow
  without publishing the final marker. Compacted same-ID callbacks cannot replace the aggregate;
  their discarded per-shard counts are no longer verifiable.
- `dispatch_plan_group` performs the fresh store-owned gate in the synchronous segment before each
  individual or dynamic-chunk send. Captured targets reconcile instead; pending peers can still
  dispatch. Later chunks do not reuse the permission observed before an earlier response await.
  Typed bulk-child completion closes sends even if the scalar row remains pending. The existing
  bulk admission/SettleOnly rule is unchanged; AbortPending is not cancellation.
- `capture_scalar_mutation_outcome` validates Graph journal ID/family, records its count, then
  advances projection. Inline success/error recovery, explicit retry reconciliation and autonomous
  recovery share it. Projection errors do not rewind capture; same-segment IC traps can still roll
  back that segment's writes. Recovery never canonically redispatches. Existing maintenance
  ordering is retained, and an inline success count must agree with the completed journal.

These are count/progress protections, **not exact receipt retention or acknowledged retirement**.
An outcome lost before durable Router capture can still disappear under Graph's nine-day GC and
be re-executed by a fresh request. The original 0366 probe is unchanged, was not rerun, and has not
been promoted as repaired. No Active/Retired scalar format, retirement RPC, retained-target lifetime,
production cap, new MemoryId, migration, staging or deployment was introduced.

At the close of 0368, an adjacent boundary remained open: envelope, zero-shard completion and
routing-release writers accepted a client key without its expected mutation ID. The per-shard
capture/send fence did not protect a new pristine same-key reservation from these earlier writes.
That checkpoint did not reproduce the trace. The [preparation callback follow-up](#router-preparation-callback-identity)
below reproduces and fixes the three write boundaries separately.

### Contract evidence

Eight native contracts cover this prerequisite: seven from the initial implementation and one from
the completion checkpoint below.

| Contract | Discriminating evidence |
| --- | --- |
| Monotonic zero/positive capture | The original writer failed full-record byte equality after a duplicate notification reset projection; fixed writer preserves bytes and rejects a changed count |
| Identity, family and target | Missing/stale/failed/wrong-family owners and wrong targets refuse; captured and pending peers differ; compacted aggregate stays immutable |
| Aggregate overflow | `u64::MAX + 1` refuses final projection publication, preserving the entire prior record |
| Bulk child closure | An opaque row key uses typed ownership; pending/AbortPending permits existing unresolved work, completed child refuses it |
| Preparation pause | An actually polled Pending future observes new saved 0/1 or compaction before the Graph client boundary; exact call trace contains only the still-pending peer, or no calls |
| Paused target conflict | The existing store owner publishes the winning envelope after a real Pending poll. Same mutation/shard IDs with a wrong Graph canister return the exact conflict, make no client calls and preserve full record bytes; matching controls reach individual and two-operation batch clients |
| Later chunks | Five 600 KB operations force multiple transport chunks. The first client call is recorded before a Pending response; capture during that wait suppresses later sends. The unchanged-owner control sends all five |
| Projection failure | Wrong Graph ID/family leaves bytes unchanged; a real native projection-client error leaves count captured. Retry uses the same journal watermark after the existing projection owner advances, not a substituted zero-delta receipt |

The Graph-client probe is native-test-only. Chunk tests use repeated operations on one registered
shard and opaque payloads to exercise the actual transport owner; they do not execute those payloads
on Graph or simulate IC scheduling. Typed bulk-child closure still has store-owner coverage only.
The completion checkpoint adds the previously missing wrong-target paused-transport case; it does
not add a child-closure-during-batch-response scenario. Real delayed callback/GC, upgrade,
lost-acknowledgement and full retirement races remain unvalidated.

### Initial validation and cost limit

Commands use the normal shared targets. Cargo tests/clippy use `--offline --locked`; canbench's
existing build command uses `--locked`, with `CARGO_NET_OFFLINE=true` for this attempt.

| Gate | Terminal evidence |
| --- | --- |
| Router `facade::store::tests` | **161 passed**, 972 filtered; 14.50 s including build |
| Router `gql::tests` | **52 passed**, 1081 filtered; 1.46 s |
| Router `graph_client::tests` | **4 passed**, 1129 filtered; 1.14 s |
| Router clippy, `--lib --tests -- -D warnings` | **Passed**, 31.21 s |
| Existing ADR0057 `bulk_load_update` PocketIC filter | **9 passed**, 4 filtered; 281.52 s observed, 61.60 s reported test runtime |
| `CARGO_NET_OFFLINE=true canbench scalar_dispatch_gate` | **Incomplete**, stopped during Router Wasm compilation at the aggregate observation limit; exit −15, 133.21 s; no benchmark sample |

The 217 focused native passes are not a whole-Router or Graph-suite pass. Existing runtime fixtures
are unchanged: nine federation constructors and 29 installs, no new bootstrap or upgrade. They do
not replace the original >nine-day lost-capture regression or validate the native pause schedule.

The two new benchmarks target one durable permission read with 1 KiB and 1 MiB retained request
fingerprints. Setup, byte-preservation assertions and destruction are outside timing. The build did
not complete, so **no instruction, allocation, latency or bounded-cost claim is available**. The
check decodes the existing full variable-size record; it is not a compact-header read. No new bound
or optimization is justified by unmeasured costs. Saved `canbench_results.yml` remains unchanged.

Ten long-command records total **580.39 s**, including stopping/cleanup after the configured 580 s
limit. Four exit-101 records are preserved: initial missing graph-registration fixture setup, the
intended duplicate-progress byte failure, an unavailable `PreflightContext::default` constructor
(replaced by the existing fixture helper), and an incorrect assumption that reopening the test
projection owner reset its stable cursor. The latter test now uses the owner's next sequence and
retains the immutable journal throughout retry. The final affected suites above pass. No long
validation was retried after exhausting the budget; no skip-build, cold target, serial substitute,
external terminal or benchmark persistence was used.

Evidence pointer: `$TMPDIR/gleaph-router-scalar-fence-current` (a pointer file). Initial dirty-worktree
snapshots, full command logs/JSON, final source hashes, scope diff and closeout review are retained
there. Benchmark hashes describe attempted source, not a measured Wasm. The only Rust change after
runtime validation corrects one dispatch-helper doc-comment line; validated-source and final-source
hashes are retained separately, without claiming byte identity across that prose edit.

Initial closeout anchor: **2026-09-22 15:08:37 UTC +0000**. The same-assistant review was not independent
or security approval. Its verdict was **not approved complete**: the planned wrong-target paused-send
case and required cost gate were unfinished. The final plan validator failed (exit 1) for those open
TODOs/criteria, despite passing preservation/reference checks. This historical failure is preserved;
the completion checkpoint below does not relabel the interrupted benchmark as a pass.

### Completion validation and measured read cost

Completion anchor: **2026-09-22 16:18:32 UTC +0000**.

`scalar_actual_send_rejects_wrong_target_after_preparation_pause` closes the missing native case.
Its four subcases cover matching/mismatching targets for individual and two-operation batch sends.
The future is actually polled Pending before the existing owner publishes the durable envelope.
The wrong target shares the saved mutation/shard IDs, so an unrelated identity error cannot satisfy
`Conflict("scalar dispatch target differs from the durable envelope")`. Rejection must emit zero
calls and preserve all record bytes. The matching controls assert the exact target/shard call trace
and the respective native unavailable-client errors; an unconditional rejection also fails.
No production hook/helper, new fixture abstraction or PocketIC bootstrap was added.

Temporarily bypassing the canister-equality check in `scalar_dispatch_gate` made the new test fail:
the wrong single-call target reached the native client and returned `Reply(Err(...))` instead of the
required conflict. The exact owner bytes were then restored, and the full GQL suite passed. That
mutant stopped at the first failing single-call subcase; it is not separate mutation-test evidence
for the batch branch. The final source change is only this one native test.

| Completion command | Terminal result |
| --- | --- |
| Focused new target test | 1 passed, 1133 filtered; 14.14 s including build |
| Same test with owner-guard mutant | Expected failed contract: 0 passed, 1 failed, 1133 filtered; exit 101, 9.02 s |
| Restored Router `gql::tests` | 53 passed, 1081 filtered; 13.57 s |
| Router clippy, `--lib --tests -- -D warnings` | Passed; 6.38 s |
| `CARGO_NET_OFFLINE=true canbench scalar_dispatch_gate` | Both cases completed; 130.89 s including normal Wasm build |

These five commands total **174.00 s** in the separately authorized completion checkpoint. The
initial 580.39 s remains separate. No timeout, skip-build, cold target, external terminal or partial
benchmark persistence was used here. Earlier store 161, client 4 and PocketIC 9 passes are carried
forward after verifying unchanged production code, not reported as new executions. GQL inventory
is now 53 (Router total 1134); this checkpoint added zero PocketIC constructors/installs/upgrades.

| Retained request fingerprint | Instructions for one fresh gate | Heap-page increase | Stable-page increase |
| --- | ---: | ---: | ---: |
| 1 KiB | approximately **4.89 M** | 0 | 0 |
| 1 MiB | approximately **125.74 M** | 0 | 0 |

The instruction values are canbench's rounded output, not exact raw counters. Each case uses one
pending scalar shard and differs only in retained request-fingerprint length. Setup, result/byte
oracles and fixture destruction are outside timing; the measured operation includes acquiring its
return value. Both cases returned `Dispatch` and preserved the encoded record. Saved benchmark
baselines remain unchanged.

The inspected read path is `scalar_dispatch_gate` → `scalar_mutation_record` →
`router_mutation_record` → stable B-tree `get` → `RouterMutationRecord::from_bytes`, which decodes
the **entire Candid record**. This is not a compact control-row read. The larger fixture costs about
25.7 times as many instructions; the approximately 120.85 M difference belongs to the complete read
path, not an isolated attribution to Candid, allocation, copying or B-tree I/O. There is no prior
same-operation baseline or throughput measurement. Zero page growth does not establish zero
allocation, peak memory or retained-state bounds.

**Disposition:** keep the fresh durable check because a preparation-time/cached permission across
await would reopen the proven safety defect. Its measured cost is material, not approved as cheap
or bounded for production. Before expanding retained results, evaluate request/control data
placement and decoding at the existing record owner, with codec/ownership invariants and separate
measurements; do not add a shadow authority, omit the check or invent an admission cap here. No
storage-format optimization was attempted or inferred from this two-point measurement.

The bounded 0368 test/measurement work is complete; production cost/capacity acceptance and the
original Graph-result lifetime repair are not. Preparation-callback ID binding was still open at
that closeout and is addressed below; exact receipt retention, scalar Active/Retired state and
acknowledged retirement remain open. Neither 0367's scan failures nor 0364's separate gates were
rerun or repaired by that checkpoint.

Evidence pointer: `$TMPDIR/gleaph-router-scalar-fence-completion-current`. It retains the fresh
2,438-file baseline, added-test diff, full command logs/JSON, exact mutant-restoration proof, measured
source snapshots and Router Wasm/hash, scoped Lizard output, review and final checks. Source/test
review is by the same assistant, not independent validation or security approval.

## Router preparation callback identity

Implementation anchor: **2026-09-22 17:29:35 UTC +0000**. The owning contract is
[ADR 0029](../adr/0029-shard-local-atomicity-and-cross-canister-consistency.md#scalar-preparation-callback-identity).
All three preparation writers now use `scalar_mutation_record` with the original reserved ID.
Missing, stale, failed or inconsistent-family records refuse before publication; envelope/progress
and terminal-result idempotency are preserved. No second record read, map, stable format, public RPC,
Graph lifetime change or read-cost optimization was added.

### Callback regression and validation

`scalar_preparation_callbacks_reject_reused_key_after_index_pause` polls the **actual preparation
future** Pending inside the existing fake index's equality lookup. A same-ID completion, production
Router expiry sweep, and new same-key/same-fingerprint reservation occur before the response resumes.
The original code failed all three branches: positive hits published an envelope into the new
reservation, empty hits completed it with zero, and an index error cleared its routing lease.
The final code requires the exact `scalar mutation identity changed` conflict and full new-record
byte equality in every branch, with no Graph plan calls. Same-ID controls prove publication of the
expected shard/vertex-17 seed, terminal zero and releasable/reclaimable routing under the original ID.

Catalogs are shared across the six native schedules; mutation records are cleared **between finished
schedules only**. The decisive old-to-new transition uses real completion/sweep/reservation APIs,
not direct deletion or a forged new ID. This tests Router preparation, not Graph execution, real IC
callback timing, Graph GC, upgrades or the complete lifetime protocol.

Two owner tests cover missing/foreign/inconsistent/failed identities and no-shard completion/release
idempotency (counts 0/7, retained timestamp 123, conflicting count/catalogs). The existing partial
shard-envelope test additionally checks complete bytes and refusal of a conflicting target. Normal
scoped executions completed:

| Gate | Result | Observed elapsed |
| --- | --- | ---: |
| Focused preparation schedule | 1 passed, 1136 filtered | 14.26 s |
| Store owner suite (final) | 163 passed, 974 filtered | 15.23 s |
| GQL suite | 54 passed, 1083 filtered | 0.93 s |
| `bulk_load::tests` filter (execution/stable/store modules) | 20 passed, 1117 filtered | 0.65 s |
| Router clippy (`--lib --tests`, `-D warnings`; final) | Passed | 7.15 s |
| Existing ADR0057 bulk-update PocketIC filter | 9 passed, 4 filtered; 50.47 s test runtime | 232.98 s |
| Two envelope-publication benchmarks | Both completed, normal Wasm build | 120.32 s |

Thirteen long commands total **453.55 s**. Four exit-101 records remain in the evidence: two test-fixture
failures (consumed call probe; catalogs not reset by repeated initialization), the genuine three-branch
behavioral red, and a native-test import/type compile failure. They are not passing gates. No timeout,
skip-build, cold target, serial/external-terminal substitute or benchmark persistence was used.
PocketIC fixture budget is unchanged: nine constructors/29 installs, zero upgrades or new fixtures.
The original 0366 probe and the unrelated 0367/0364 failing gates were not rerun or weakened.

Review found that the new conflicting-target assertion also changed shard cardinality. It was
corrected to keep both shards and seeds identical and change only the first Graph canister. The
final owner suite and clippy above were then rerun; their earlier 2.25 s/5.68 s passes remain in the
logs. This sole post-measurement Rust change is in the `cfg(test)` store-test file. Measured and final
source snapshots are separate; production sources, GQL schedule and benchmark fixtures are unchanged.
The PocketIC/benchmark results are retained executions, not newly repeated runs after that test edit.

### Envelope-publication cost

| Retained request fingerprint | Instructions | Heap-page increase | Stable-page increase |
| --- | ---: | ---: | ---: |
| 1 KiB | approximately 13.18 M | 0 | 0 |
| 1 MiB | approximately 258.33 M | 0 | 0 |

Each sample publishes one shard envelope into the existing warmed one-record owner, including the
full Candid record acquisition and stable publication. An untimed successful control, pristine
record reseeding, input construction and full-record byte oracles are outside measurement; the
operation and its returned result are inside. These are **new whole-operation samples**, not a
before/after regression, an isolated ID-check cost, a measurement of the other two writers, or a
quantity to subtract from the earlier read-only gate samples. Zero page growth is not zero allocation
or a peak/retained-state bound. Cost is material; production acceptance and owner-preserving read/data
placement investigation remain open.

The three-write prerequisite is complete, not full Router response identity or scalar retention.
Client-key-only count/phase reads and same-ID lease reclaim remain audit obligations in the gap
ledger. No new attempt generation or permanent tombstone was added. The lost-capture Graph-GC
re-execution defect remains open.

Evidence pointer: `$TMPDIR/gleaph-router-preparation-identity-current` (2,438-file baseline, exact source
patch/hashes, all terminal logs, measured Wasm, scoped Lizard comparison and final plan/preservation
checks). Review is by the same assistant, not independent validation or security/performance approval.

## References

- [ADR 0027 — journal retention](../adr/0027-graph-mutation-journal-retention.md)
- [ADR 0057 — durable bulk load](../adr/0057-router-operation-api-and-durable-bulk-load.md)
- [Official IC multi-canister skill](https://skills.internetcomputer.org/.well-known/skills/multi-canister/SKILL.md), checked against repository `ic-cdk 0.20.1`
- [IC message execution properties](https://docs.internetcomputer.org/references/message-execution-properties)

## Outcome (2026-11-07): the repair this record demanded has landed

This record ended at "this checkpoint does not close GAP-2026-09-22-001" and left the probe red: both rows stayed
`CanonicalPending`, the real Graph sweep removed all four entries, and identical retries reapplied the positive SET or
turned zero into a write. That is now closed, by the mechanism ADR 0027's amendment chose - scalar entries are `Active`
until their owner retires them, which is ADR 0049's ordered-family predicate.

The probe's fixture was not retained (it lived under `$TMPDIR`), so the repair is proven by two committed PocketIC
contracts instead of by re-running it:

- `crates/pocket-ic-tests/tests/adr0027_scalar_journal_retention.rs` - the entry survives the real sweep past both
  retention windows, retirement is exact and starts retention, an unknown id is refused, and the retained entry is the
  completed scalar record.
- `crates/pocket-ic-tests/tests/adr0057_durable_bulk_load.rs::a_lost_prefix_record_still_settles_once_after_the_retention_window`
  - the probe's own shape: Graph commits, the Router callback traps (fault 11), both windows pass, and the identical retry
  settles the row **once**.

What this record's caution still buys: a saga that never reaches terminal stays in doubt indefinitely, and the Router now
makes that visible and decidable (`InDoubtView`, `list_in_doubt_mutations`, `resolve_in_doubt_mutation`) rather than
resolving it automatically, because absence of evidence is not proof of no write.
