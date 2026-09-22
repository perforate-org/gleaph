# ADR 0097: bounded-read baseline attempt

**Status: Experimental — incomplete measurements; no executable change retained in this attempt.**

This report describes the stopped `af063ea42` → `7e9465c45` attempt and its later evidence
correction. The [owner-layer follow-up](adr0097-left-pack-bounded-reads.md) has separate completed
measurements and implementation; it does not change the failures or exclusions recorded here.

## Outcome

The requested one-factor optimization was not implemented: baseline measurements did not
complete within their command caps. No IC improvement, negative IC result, new runtime pass,
or before/after work equivalence is established by this attempt. The performance gate remains
open. Main remains untouched and unmerged.

The baseline is **`af063ea427415c88c622d6e0124192ad028a9ecf`**, not historical `3ea7dc739`.
The latter's 43,376,650-instruction completion silently changed survivor ordinals and is not a
movement-contract acceptance target. The earlier complete
[zero/forwarded cursor comparison](adr0097-compaction-movement-publication.md) remains separate:
forwarding still repeatedly scans an expanding tombstone gap and is not linear-time recovery.

Only **`d292014eb`** is retained in Rust source. It corrects local physical-publication and tiny-row
comments in `graph/compact.rs`: captured slab prefixes can contain holes, retained runs are not
folded, and tiny rows need not have `stored == degree`. Every non-comment line is byte-identical
to the baseline. No finder, read helper, movement selection, cursor, fallback, error, policy,
threshold, initialized extent, schema or allocator behavior changed.

## Inspection and intended experiment

The existing graph owner is `first_edge_slot_move_in_bucket`. It checks the run/degree gates,
then visits initialized slab slots in order until the first live edge after a hole. Its caller,
`compact_vertex_edge_span_one_step_with_span_policy`, performs the zero-cursor fallback when a
nonzero-cursor scan finds no move. `EdgeStore::read_slots_contiguous` and
`read_slots_contiguous_bytes` already expose contiguous slab reads; the edge iterator layer
already uses bounded 32-slot reads. A new cache, persisted cursor or movement representation
was not needed for the proposed read-granularity experiment.

The planned candidate would bound each prefetch to the initialized prefix and checked address
range, retain the same first move, and stop decoding at that same selected edge. Fewer read
calls might reduce IC cost, but could also prefetch additional bytes; decode count, bytes and
calls must remain separate observations. Repeated logical scanning would remain quadratic.
**No candidate code was written or measured.**

A draft pair of 4/10-byte first-move tests was prepared before implementing the candidate. Each
uses one small, actually backed graph with a 35-slot slab prefix and a neighboring label. The
intended assertions cover a partial chunk, holes, nonzero and exhausted cursors, exact decode
stop, read bounds, read-only selection, an exact address-overflow error, and the real caller's
no-move/fallback path. Their compilation timed out before any selected test ran. They are **not
validated fixtures**, runtime reds, or passing contract coverage. They were removed from the
working source and preserved only as a
[draft evidence patch](sources/adr0097-bounded-read-draft-fixture.patch), not a patch to stack
onto HEAD. Existing P1 event/observer and interleaved-delete/value tests were not weakened.

## Commands and terminal evidence

All Rust/canbench/rustfmt commands used one sequential
[monotonic ledger](sources/adr0097-bounded-read-ledger.json) and
[runner](sources/adr0097-bounded-read-run.py). Before each command it checked the remaining
600-second cumulative command budget. Non-final commands protected a 120-second final reserve.
Normal configured targets were used; no clean, cold target, wrapper retry or persistence.

Cargo commands ran from `/Users/yota/dev/gleaph-spill`. Canbench ran from
`/Users/yota/dev/gleaph-spill/crates/ic-stable-lara`.

| Command | Cap | Exit / elapsed | Result |
| --- | ---: | --- | --- |
| `canbench bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384 --csv` | 150s | 124 / 150.272s | **Incomplete:** release compilation; no benchmark started |
| `cargo test -p ic-stable-lara --lib diagnostic_crossover_16k_event_parity -- --nocapture` | 80s | 124 / 80.037s | **Incomplete:** compiled in 61s, interrupted during zero-cursor compaction |
| `rustc --version --verbose; cargo --version; canbench --version` | 10s | 0 / 0.647s | Tool versions recorded |
| `rustfmt --edition 2024 --config skip_children=true crates/ic-stable-lara/src/labeled/graph/compact.rs` | 10s | 0 / 0.770s | Draft fixture formatted, not runtime validation |
| `cargo test -p ic-stable-lara --lib left_pack_bounded -- --nocapture` | 100s | 124 / 100.146s | **Incomplete:** compilation; neither selected test ran |
| `rustfmt --edition 2024 --config skip_children=true --check crates/ic-stable-lara/src/labeled/graph/compact.rs` | 10s | 0 / 0.740s | Passed on final comment-only source |
| `cargo clippy -p ic-stable-lara --lib -- -D warnings` | 90s | 0 / 14.986s | Passed on final comment-only source; not a runtime pass |

Total ledger: **347.5981497091707 / 600 seconds**, with **252.40185029082932 seconds unused**.
The failed attempts are included. Work stopped rather than using the remaining allowance for
speculative repeated builds. Logs establish the terminal states, not the cause of the slow
builds. Tool versions match the earlier slice; local memory/disk observations are retained as
diagnostics, not causal attribution. No process remained attached to the three owned attempt
logs at the subsequent `lsof` inspection; no unrelated process was managed.

After canbench/native measurements and the optional small control were **not run**: the
baseline was incomplete. Final affected runtime filters were also **not run** after removing
the unvalidated fixtures; the retained source changes are comments only. There is no new full
library, workspace, survey, PocketIC or lifecycle pass. The last full library result remains
the older descriptor checkpoint, not this source validation.

Logs: [canbench](sources/adr0097-bounded-read-baseline-canbench.txt),
[native](sources/adr0097-bounded-read-baseline-native.txt),
[draft fixture check](sources/adr0097-bounded-read-draft-fixture-check.txt),
[clippy](sources/adr0097-bounded-read-final-clippy.txt),
[format](sources/adr0097-bounded-read-final-format.txt).
Exact commands, environment, source hashes, raw/durable artifact hashes and exclusions are in
[run metadata](sources/adr0097-bounded-read-run.json). Durable text logs trim terminal blank
lines only for whitespace checks; the raw logs and ledger hashes are unchanged.

## Incomplete native observations are not a comparison

The installed diagnostic was byte-identical to the existing
[corrected probe](sources/adr0097-crossover-16k-probe.rs), SHA256
`be28e6196fd3cc8a42f0720177821174d69f55f841c2b00d029de2bddb30e602`.
It checks the compiled canonical manifest path. The temporary cfg(test) registration and source
were removed afterward.

The zero-cursor run reached seeded prefix 16,384, no run, vertex span 17,536, and verified all
seeded identities and odd survivors after the original even-slot removals. The saved partial
trace contains **7,663 moves**, ending at step 7,662 (`15325 → 7662`). It has no `Finished`, no
final replay check, and no completed compaction phase counters. The forwarded-cursor run did
not start. Consequently:

- no current paired-cursor memory-image equivalence is established;
- no completed call/byte/decode totals are available for compaction;
- no after source exists to compare against;
- the earlier complete pair is neither overwritten nor substituted for these missing results.

Canbench produced **no fresh total or scope rows**. The copied CSV was the preserved foreign
workspace CSV, not output from this attempt; the existing wasm hash is not an executed baseline
artifact. Neither is used as measurement evidence. Missing scopes are absent, not zero-cost
observations. The exact ten-byte benchmark, cursor, setup, source, YAML and configuration remain
unchanged.

## Preservation, review and open gates

The final source diff contains only the reviewed local comment corrections. Supporting Lizard
1.24.0 comparison (`--csv -i -1`, working tree against `af063ea42`) finds no NLOC/token/parameter
or CCN change in the three touched functions: CCNs **17/25/10** on both sides. This is neither
independent acceptance nor performance evidence. [Metrics](sources/adr0097-bounded-read-lizard.json).

The restored Cargo.lock SHA256 is
`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`;
the as-used lock is
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.
The existing [zero-context lock patch](sources/adr0097-crossover-as-used-lock.patch) reconstructs
those as-used bytes. The foreign brief/CSV, canonical YAML, both benchmark source files,
canbench config and main pin were preserved. `git diff --check` passed.

Raw artifacts are under `/tmp/spill-read-af063-obmxdo77/`, with a SHA256 manifest. At this
checkpoint, plan `0334-compaction-bounded-read.md` was **partially complete, not approved**.
Both saved validation logs fail: [draft](sources/adr0097-bounded-read-plan-draft.txt) reports
`ERROR: Completion Criteria item 6 must be unchecked in draft phase`; [final](sources/adr0097-bounded-read-plan-final.txt)
rejects the pending TODOs and unchecked criteria. The earlier draft-pass statement was false
for this revision and is withdrawn. The evidence correction preserved the completed comment
criterion and the then-pending performance work. Neither log is a structural pass, and no
preserved earlier passing log is used to qualify these failures. Later plan revisions/results
are described only in the linked follow-up.

At the stop, the next evidence needed was a completed current-baseline measurement and runnable
small first-move/bounds fixtures before prototyping the single read-granularity change. An algorithmic
progress change would require an explicit owner/invalidation contract and a separate scope.
All remaining performance, space, PocketIC, arena/caller, unpinned-accounting, canonical-write
atomicity and landing gates remain open. The earlier bounded P1 approval does not waive them.
