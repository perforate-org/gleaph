# ADR 0097: compaction movement publication

## Result and boundary

**`15ceaabf1` fixes unreported ordinal changes at compaction completion.** The starting revision
is `d4c59ff8b`. Current-head owner and deferred-observer witnesses fail there, then pass after
physical publication is restricted to retaining logical ordinals. This is a correctness repair,
not a performance optimization or landing approval. Descriptor P2 remains accepted only at its
own tested boundary. Main remains untouched and unmerged.

Bounded implementation review approved `15ceaabf1` + `4912fa786` for movement publication
only, after inspecting the complete diff, contracts and preserved evidence. The reviewer did
not run Rust validation. This does not approve overall ADR 0097, performance or landing.
The subsequent [bounded-read baseline attempt](adr0097-compaction-bounded-read.md) is incomplete
and retains no executable change or new performance acceptance.

The invariant is: **replaying returned movement events must reproduce canonical edge locations,
including at `Finished`.** Canonical target/order agreement alone cannot establish this: external
sidecars and postings are keyed by owner, label and slot. The deferred driver forwards
`EdgeMoved` and bounded `OverflowRewrite` events; `Finished` only completes the work item.
`GraphSidecarMoveObserver` uses the forward events to relocate canonical sidecars/postings.

## Historical classification corrected

The prior [16K report](adr0097-descriptor-rejection-crossover.md) incorrectly excused the first
historical `(0,1)` versus `(1,1)` failure and reset the oracle at completion. That was a valid
unreported-movement signal, not an oracle error to normalize away.

Preserved history:

1. [Initial event-oracle source](sources/adr0097-crossover-16k-initial-event-probe.rs) and
   [initial historical failure](sources/adr0097-crossover-16k-initial-event-red.txt).
2. [Superseded reset-oracle source](sources/adr0097-crossover-16k-finished-reset-probe.rs),
   byte-identical to the previously published probe, SHA256
   `17379d2b5993849c365f6969754e10f2d3c98d99755ee2c18460eb1ea15d904c`.
   Its passing historical exit remains a runtime fact, not movement-contract acceptance.
3. The original canonical probe path now contains the [corrected event-replay probe](sources/adr0097-crossover-16k-probe.rs).
   Neither `Finished` nor an error resets its expected ordinals. It asserts actual completion
   separately and compares cursor strategies only after proving byte-identical backing.

Replaying the preserved movement TSVs against the original odd-slot survivors, then comparing
with the recorded final ordinal TSV, yields:

| Pin | Reported moves | Canonical survivors | Unreported ordinal changes |
| --- | ---: | ---: | ---: |
| `3ea7dc739` | 0 | 8,192 | **8,192** |
| `5ad7f5a61` | 8,192 | 8,192 | 0 |

Thus the old fast completion did retain all targets, but violated notification semantics. No
historical reference tree was edited or rerun. The original instruction totals and phase counts
remain observations of those pins, not acceptance or a performance target obtainable by silent
bulk packing. The historical prefix/run state does **not** prove current-head reachability from
the present slab-only 16K seed; the following small fixture supplies separate current-head evidence.

## Current-head owner and observer witnesses

`retained_compaction_fixture` uses existing insertion, explicit initial compaction and the
non-folding physical publisher, with actual allocated backing. It does not fabricate extents or
alter relocation/fold policy. The initial dense materialization occurs before holes or external
identity keys exist.

- Source vertex 0, label 2: edge prefix **8**, used run **9**, original ordinals `0..=16`,
  holes **0/3/9**, fourteen survivors. Values have widths **8/33**, prefix **13** and a retained
  run with **one used row**. Payloads encode the entire `target + 1` in eight bytes.
- Label 3 has a four-slot slab prefix with a leading hole: its three survivors must still move
  one at a time and emit three notifications. Vertex 1 holds a distinct untouched neighbor.
- The source prefix is backed but bounded by the next label. Calling the real fold owner returns
  exact `Store(CollectAllocationOverflow)`; descriptor and exact source rows remain unchanged.
  The tested step/maintenance path keeps `SlackMayBeDropped` and can successfully finish.

On `d4c59ff8b`, both tests fail at completion ([runtime red](sources/adr0097-compaction-movement-red.txt)):

- `retained_compaction_finished_matches_every_reported_move`: fourteen source survivors are
  silently repacked; the first is `(0,1)` instead of reported `(1,1)`.
- `retained_compaction_observer_keeps_external_identities_through_retry_and_finish`: the actual
  deferred callback stream leaves the external identity missing at the new canonical key.

Both initial failures reach width 8; the [green](sources/adr0097-compaction-movement-green.txt)
runs both widths. The named test bodies are byte-identical red/green. The shared fixture's
publisher call drops the removed `compact=false` argument. `4912fa786` subsequently strengthens
the values preconditions to prefix 13/run used 1, with another passing focused run; the initial
red required unequal prefixes and a retained values-run id, not these exact value extents.
Hashes and the distinction are recorded in the run metadata.

The owner test replays every event and checks exact slots, targets and inline values in both
orders after every step, including `Finished`. It bounds run-fold events to run ordinals, requires
three single-edge slab moves, and verifies that source prefix/run/value-run identities remain
unchanged. The deferred witness uses distinct external identity values, rejects overwriting a
neighbor key, and checks every canonical row against that external map in both traversal orders.
It never rebuilds the external map from post-compaction output.

The existing pre-step fault hook separately exercises retry: a failed work item stays queued,
reports no completed rebalance and delivers no moves. With the hook cleared, the same work
finishes successfully; the next tick has no work or repeated events. This is not a late-write
OOM or general allocation-failure atomicity claim. The observer is a contract sink, not a full
Graph postings-store/PocketIC fixture.

## Owner-level correction

The call-site audit found two callers selecting observer-free logical packing:

- the `Finished` arm in `compact_vertex_edge_span_one_step_with_span_policy`;
- the peer pass in `rebalance_cascade_after_labeled_mutation` (`graph/bucket.rs`).

Both now use `pack_vertex_edge_span_layout`, which removes physical inter-bucket slack inside
existing backing. The existing materializer/commit pair, also used by streaming and buffered
leaf relocation, captures only initialized prefixes/root arrays. It cannot fold a run, drop
prefix tombstones or change logical ordinals. A declined non-forcing fold therefore leaves the
source prefix width and run intact, even at successful completion.

The obsolete compact/fold switches, optional decoded-edge representation, whole-prefix encoding
buffer, silent run retirement and unused `vertex_edge_span_slot_moves` collector are deleted.
No unbounded move batch was added to `OverflowRewrite`. The notification-bearing bounded run
fold and single-edge slab paths remain the logical movement owners. Their error handling,
required-fold branch, cursor forwarding, interleaved-delete fallback and independent value
mapping are unchanged. Logical-count overflow admission remains before movement; reservation
weights, thresholds, allocator/growth policy, layout and persisted queues are unchanged.

Physical publication is not constant-time work: it may copy physical regions. Its guarantee is
that it does not smuggle logical remapping into an outcome with no movement payload. No new
cache, queue, schema, dependency or compatibility path is introduced.

## Cursor attribution

`bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384` supplies slot cursor **zero**
on every step and ignores `moved.new_slot_index`. The whole-span wrapper and production deferred
driver already advance to `moved.new_slot_index + 1`. The prior 67M-read derivation describes
that benchmark, not all production execution. Benchmark source remains byte-identical.

The corrected native probe runs both cursor strategies sequentially on `15ceaabf1`. It asserts
all **sixteen memory images byte-identical before compaction**, as well as exact seeded targets,
even removals and odd survivors in both orders. Each source is the unchanged ten-byte, capacity
`1 << 20`, insertion-ordered 16K fixture: prefix 16,384, no run, vertex width 17,536.

Both strategies produce exactly `2*j+1 → j` for all 8,192 moves, then advance and finish: **8,194
calls**, no `OverflowRewrite`, stopping error, compaction-time growth or unreported ordinal
change. Final event replay equals `(j, 2*j+1)` in both orders. Final prefix/vertex width is 8,192;
leaf reservation total remains 17,536. Newly executed move/final TSVs byte-match the preserved
5ad movement/final TSVs; distinct cursor-step traces and hashes are retained in the raw directory.

| Explicit compaction on `15ceaabf1` | Zero cursor | Forwarded cursor |
| --- | ---: | ---: |
| Native reads, all roles | 67,182,614 | 33,640,470 |
| Edge-slab reads (role 5) | 67,141,633 | 33,599,489 |
| Read bytes, all roles | 673,047,193 | 337,625,753 |
| Live / dead decodes | 33,574,912 / 33,566,720 | 24,576 / 33,574,912 |
| Writes / bytes, all roles | 16,389 / 245,878 | 16,389 / 245,878 |

For forwarded move `j` (zero-based), scanning from `j` to old slot `2*j+1` still costs `j+2`
reads: **33,566,720** across the moves. Add 8,192 terminal suffix reads, the required 16,384-slot
fallback scan, 8,192 source reads applying moves and one physical copy read: **33,599,489**.
Forwarding avoids repeatedly reading the packed live prefix, but **still repeatedly scans the
growing tombstone gap**. This fixture remains quadratic; no linear-time claim is justified.

[Raw native output](sources/adr0097-compaction-cursor-native.txt),
[all phase/role counters](adr0097-compaction-cursor-phases.csv), and
[provenance/trace hashes](sources/adr0097-compaction-movement-run.json).
These are native `Memory` observations, not IC syscalls, per-read instruction prices, or production
maintenance-queue latency. No cursor behavior was added to production and no canbench was run.
The two-read metadata difference and removed final re-decoding relative to 5ad are not an IC
performance-recovery claim.

## Validation and preservation

All Rust/toolchain commands use one serialized monotonic ledger, with preflighted remaining
allowance and a protected **120-second final reserve**. [Ledger](sources/adr0097-compaction-movement-ledger.json),
[runner](sources/adr0097-compaction-movement-run.py), [versions](sources/adr0097-compaction-versions.txt).
Total: **89.116 / 600 seconds**, **510.884 unused**. This separate slice does not erase the earlier
approximately 726-second breach.

Commands use the normal target from `/Users/yota/dev/gleaph-spill`:

| Command | Result |
| --- | --- |
| `cargo test -p ic-stable-lara --lib retained_compaction -- --nocapture` | Red **0/2**; green and final **2/0** |
| `cargo test -p ic-stable-lara --lib compact -- --nocapture` | **81/0**, includes exact overflow and interleaved-delete contracts |
| `cargo test -p ic-stable-lara --lib relocat -- --nocapture` | **11/0**, includes both retained-run relocation arms |
| `cargo test -p ic-stable-lara --lib rewrite -- --nocapture` | **10/0**, including existing value/order witnesses |
| `cargo test -p ic-stable-lara --lib diagnostic_crossover_16k_event_parity -- --nocapture` | **1/0**, both cursor cases; **31.005 s** including compilation |
| `cargo clippy -p ic-stable-lara --lib -- -D warnings` | Passed, including final source |
| Scoped `rustfmt --edition 2024 --config skip_children=true --check` | Passed; exact paths in ledger |

Filters overlap; counts are not an additive unique-test total. No unfiltered library/workspace
sweep was run. Temporary probe registration/source was removed and committed production bytes
verified unchanged. Final runtime log: [retained contracts](sources/adr0097-compaction-final-contracts.txt);
[clippy](sources/adr0097-compaction-final-clippy.txt); [format](sources/adr0097-compaction-final-format.txt).

[Lizard 1.24.0](sources/adr0097-compaction-lizard.md) compares the six changed Rust files with
d4c59ff8b: maximum touched CCN **42 → 31**, aggregate **245 → 212**, including new tests.
This supporting self-review is not independent acceptance or evidence of runtime speed.

The as-used lock is `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`;
the restored workspace lock is `15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`.
The existing zero-context lock patch reconstructs the same as-used bytes. Canonical CSV/YAML,
benchmark/config source, both foreign files and main match starting hashes. No persistence,
clean, stash, reset, reference edit, cold target, GUI fallback or merge occurred.

Raw logs, snapshots, patches, full traces, Lizard CSVs and SHA256 manifest are retained under
`/tmp/spill-moves-d4c-aivo_i73/`. Durable log copies trim terminal blank lines only; ledger hashes
refer to raw logs. One evidence-copy reporting lookup used a nonexistent ledger key after copies
and lock restoration; summing the recorded runs corrected reporting without rerunning validation.

All nine historical performance increases remain open. The first cause of the 16K seed-shape
divergence, broader write/publication atomicity, wide edges, arena/other callers, independent
review, space/PocketIC, final-source full persistence/all-name review and landing remain separate
gates. Existing duplicated unpinned accounting in the rewrite wrapper/commit was not changed or
validated by this notification repair. Previously stale external sidecars are not repaired by
this patch; no migration or recovery claim is made.
