# ADR 0097 — Allocator publication preflight

**Status:** Implemented allocator boundary; validation partial, not a final-approval candidate.

This evidence is limited to plan 0385 and `FreeSpanStore`. It does not enable width declaration,
complete the grouped segment/geometry gate, or change the approved W2 protocol. Contract v2 is a
document revision, not a storage format. The implementation checkpoint used HEAD
`4952323c21c073652a0c7c495fb2471cd2d43918`, with no stage, commit or push at that checkpoint.
The subsequent user-authorized commit includes this allocator change and its inseparable existing
A0 exact-span guards/tests, but excludes unrelated dirty paths and the deletion/demotion document
hunk. Commit/index evidence is under `width-implementation/commit/`; no push is authorized.

## Evidence binding

External evidence root:
`/Users/yota/.local/share/gleaph/evidence/adr0097/maintenance-fixtures-f2-f4-0383`.

- `width-implementation/baseline.json` and `before/` freeze the pre-implementation dirty source,
  completed 0384 closure, original lock, protected paths and continuing command-ledger prefix.
- `width-implementation/budget-extension/authorization.json`: user-authorized ceiling 710→1310s,
  without resetting the ledger. Every command uses the existing bounded runner.
- Approved `width-owner/contract-v2.md` SHA256:
  `b08330854aa683a75da43815b7953315431e33a173f748e6263e54535cb2616a`; packet, decision and
  completed review/0384 evidence are not edited.
- Final `free_span.rs` SHA256:
  `072c3da1f1e9ac424d72c78d6af6a354547c280567416fc21a6cb04ccbe18569`.
  Validated source SHA256: `7be92409f9d4aa106d0c3cdb02cb35c289b5a591475fc4dfc2e4da339b38a9ca`;
  `post-validation-doc-only.json` proves the only subsequent Rust-file change is a two-line
  capacity-only API comment correction, with executable code unchanged.
  Baseline SHA256: `35b7b00899db94ef58f6f6777ae2b29f9e6bc4a3c46a5dd4762e7067f87349fd`.
- `width-implementation/analysis.json`, `production.patch`, and `delta/` bind the actual source
  comparison, native totals, diagnostic comparison and benchmark measurements.
- `width-implementation/checkpoint-final.json` records the exact owned delta and protected-byte checks;
  the final structural-plan transcript deliberately does not certify completion while gates fail.

## Owning boundary and implementation

`FreeSpanStore` retains ownership of records, intrusive bins, allocator counters/summaries,
heap candidates and the paired by-start index. `replace_spans` is the surviving publisher.
The private `FreeSpanEdit` constructs prospective affected records/bin heads and final counters;
its captured original record-slot bound is distinct from the prospective allocation bound.
Source and target backing, checked arithmetic, cache maximum/scratch, fixed-width encodings and
complete ordered paged-map admission finish before its first canonical record write.

After that write there is no recoverable branch: publication writes already backed encoded
records/bin heads/header fields, consumes the existing map `PreparedEdits`, releases the map
borrow and installs admitted cache state. Returned errors preserve canonical bytes and cache
contents/ready flag; physical backing or scratch capacity may remain. There is no native rollback
or assumed IC message rollback. Invalid memory implementations/system traps are not converted
into returned-error guarantees.

Audited symmetric paths: insertion/release's four merge cases; whole removal; best-fit and
explicit-prefix split; prefix restore; exact pair replacement including outside neighbors.
The standalone split publisher, `replace_existing_span`, record allocation/free writers,
`unlink_from_bin`, per-field record writers and post-write summary/cache rebuilding were removed.
All use the one preparation/publication path. The shared coalescer preserves valid-state guard
precedence. Restore no longer accepts a range containing an intervening free span merely because
another span starts at its end: it returns exact `OverlapNext` without mutation.

`OutOfMemory` and `IndexFull` are non-persistent error additions only. No stable field, MemoryId,
table, queue variant, migration, compatibility branch, new dependency or generic transaction
engine was introduced. Record encoding/decoding stays in the existing record type and V1 layout.
`reserve_for_releases` is explicitly capacity-only, not a ready publication receipt.

## Native contracts and wrong implementations

Exactly six new `width_allocator_` tests; zero PocketIC constructors and zero canister/federation
installs, before and after. Existing 27 owning tests, including both A0 guard tests, remain.

- Record grow: a real next record-memory grow returns exact `OutOfMemory`, rather than corruption.
- By-start grow: real index admission failure preserves distinct existing spans.
- Restore grow: a genuinely allocated prefix has a remainder on a nonempty page whose predecessor
  is full; copied-map preparation proves the actual grow failure. Keeping that page nonempty avoids
  an incorrect fixture that would legitimately recycle its empty page without growing.
- Pair grow: complete remove/remove/insert admission fails while both inputs, unrelated spans and
  a reopened cold cache survive. It rejects a publisher that deletes inputs before preparation.
- Success: both split entry points, restore, whole removal, every release merge case and pair
  coalescing of two outside neighbors have exact spans, derived summaries, bijection and reopen
  assertions. A no-op setter or missing co-write cannot satisfy them.
- Rejections: exact empty, overflow, duplicate, previous/next overlap and missing-span errors,
  established precedence, intervening restore overlap, canonical/cache snapshots and reopen.
  Directory-full error translation is checked without copying the generic map's directory fixture.

The red transcript ran all six: one passed, five failed. Growth tests require a native error,
not a caught panic, and compare old paired bytes/cache, exact spans/statistics, validation and
reopen. Final green runs passed all six and all 33 owning tests. Contiguous-record decoding also
received the affected crate's complete 701-test runtime pass.

## Terminal validation

Commands use worktree `/Users/yota/dev/gleaph-spill`, except canbench from
`crates/ic-stable-lara`. Transcript names below are under the external evidence root.

| Command | Result / transcript |
| --- | --- |
| `cargo test -p ic-stable-lara --lib width_allocator_ -- --nocapture` | RED: 1/6; `width-allocator-red-final.log`. Final GREEN: 6/6; `width-allocator-green-final.log` |
| `cargo test -p ic-stable-lara --lib lara::edge::free_span::tests:: -- --nocapture` | PASS 33/33; `width-allocator-regression-final.log` |
| `cargo test -p ic-stable-lara --lib` | PASS 701/701; `width-allocator-client-runtime.log` |
| `cargo clippy -p ic-stable-lara --lib --tests -- -D warnings` | FAIL; `width-allocator-clippy-final.log`: exactly the same 41 baseline diagnostics, none in the owner. `analysis.json` compares diagnostic text and locations, not just counts |
| `cargo fmt --all -- --check` | FAIL; `width-allocator-format-check.log`: unrelated preserved paths only. Not repaired or suppressed |
| `rustfmt --edition 2024 --check crates/ic-stable-lara/src/lara/edge/free_span.rs crates/ic-stable-lara/src/lara/edge/free_span/bench.rs` | PASS; `width-allocator-owned-format-check.log`. This is not a substitute workspace-format pass |
| `canbench bench_r_fs_tkbfs_` | PASS, exactly three cases; `width-allocator-candidate-final-split.log`, 81.89s including build |
| `canbench bench_r_fs_lbin_` | PASS, exactly two cases; `width-allocator-candidate-final-release.log`, 4.63s |
| `git diff --check` and exact manifest/protected-byte verification | PASS; `width-allocator-diff-check-final.log` / `width-allocator-checkpoint-final.log`. The earlier diff check caught and corrected an owned Markdown trailing-space line |
| `python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py plans/0385-free-span-publication-preflight.md --phase final` | NOT COMPLETE / FAIL; `width-allocator-plan-final.log`: validation/design completion remains pending; no waiver of failed gates |

Neither failed Clippy nor failed workspace formatting is a passed gate. Their unrelated paths
require separately authorized ownership before repair; this slice is paused rather than widening
scope or weakening completion criteria. No full workspace tests, PocketIC or 0383 Graph fixture
rerun is substituted. No background process or partial benchmark artifact is left running.

## Cost and fixture comparison

Compare against the **fresh** pre-implementation logs, not canbench's printed percentages against
older checked-in YAML. The initial stale-source run executed only 25 tests; touching only the
owning source timestamp reacquired all 27 and the fresh benchmark baseline before implementation.

Population, sizes, cardinalities, survivor count and measured operation sequence remain unchanged.
New read-only selection/fixture sanity checks are outside measured closures and do not mutate
allocator topology or heap state. Native success tests assert actual mutation results before these
benchmark runs. The measured loops retain mutation, restore and largest-summary maintenance.
No `--persist` was used; existing benchmark artifacts remain unchanged.

Rounded total instruction measurements:

| Benchmark | Fresh baseline | Final | Approximate change |
| --- | ---: | ---: | ---: |
| split/restore 256 | 812.16K | 974.46K | +20.0% |
| split/restore 1024 | 819.56K | 973.47K | +18.8% |
| split/restore 4096 | 934.00K | 1.00M | +7.1% |
| largest-bin remove/release 256 | 991.68K | 786.99K | −20.6% |
| largest-bin remove/release 4096 | 4.45M | 1.57M | −64.7% |

Every case reports zero measured heap and stable-memory page increase. The first unified candidate
had approximately +25–34% split overhead. Inspection found repeated scalar record reads and source
bound reads in the prospective path; one contiguous record read and a captured backed source bound
reduced that cost without changing layout or weakening admission. The remaining split increase is
a necessary safety/semantic trade-off of preparing complete paired edits/encodings/cache capacity
and sharing overlap-safe restoration, instead of the old destructive/unchecked fast path. It is
not an improvement claim. This evidence does not measure grouped W2 publication or its callers.

## Architecture and remaining gates

- **Encapsulation:** the prospective edit and encoded writes are private to the existing owner.
- **Separation of concerns:** the generic map still owns page/directory admission; no Graph/GQL
  syntax, queue policy, sidecar behavior or width semantics moved into it.
- **Invariants:** the owner admits the whole record/bin/index mutation before canonical writes.
- **Consistency:** one publisher maintains the paired index, counters, bins and derived maximum;
  no second persistent authority was added.
- **Fitness:** one ephemeral owner-bound edit replaces the duplicated mutators. Production prefix
  growth is 1420→1493 lines (+73), not a new transaction framework. No signature has five or more
  meaningful parameters. Sparse prospective records and precomputed cache maximum are the
  irreducible preparation cost; no unrelated abstraction or module was added.

Checked the approved W2 packet, allocator/map owners, callers and affected active storage design.
Updated [LARA storage](../storage/lara.md) for the implemented allocator boundary, capacity-only
reservation and explicit planned outer gate. No new ADR is needed for this prerequisite of the
already approved protocol. The existing native outer reservation/abort, grouped segment/leaf
geometry, queue ownership and Graph first-write obligations remain later slices, not implemented
claims. Public width declaration stays gated; 0383 remains its unchanged RED 0/3 checkpoint.
