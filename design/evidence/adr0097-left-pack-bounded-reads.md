# ADR 0097: bounded reads in left-pack selection

**Status: Bounded slab-read batching/evidence through `56d381b3f` approved; broader acceptance open.**

## Result

The unchanged ten-byte, zero-cursor 16K delete/compact benchmark improves from
**15,498,138,427 instructions at `761b2a808` to 7,185,627,417 at `ea12e7c12`**:
**−8,312,511,010 (−53.6355%)**. The baseline, initial candidate and final admission-refined
candidate canbench commands all completed without persistence.
The eight observed maintenance scopes have identical names, calls and instruction counts.
The changed finder loop is outside those scopes. Missing scopes are not zero-cost observations.

This is a **constant-factor improvement, not linear-time compaction**. The same rows are
decoded, the same bounded moves are reported, and both cursor strategies still repeatedly scan
an expanding tombstone gap. Historical `3ea7dc739`'s 43M completion violated movement publication;
it is not the acceptance baseline. The raw canbench delta columns still compare to that old
canonical YAML: use the [current-source A/B table](adr0097-left-pack-ic-comparison.csv), not those
columns, to interpret this change. Full canonical artifacts remain unchanged and historical.

No overall ADR 0097, broad performance, space, PocketIC, atomicity or landing approval is claimed.
Main remains untouched and unmerged.

## P2 evidence correction is separate

`a4f62e096` corrects the false draft-validator pass in the
[previous incomplete-attempt report](adr0097-compaction-bounded-read.md). Its saved draft log
says exactly `ERROR: Completion Criteria item 6 must be unchecked in draft phase`; final
validation also failed on pending work. Both original logs are preserved byte-for-byte and
now linked durably. The completed comment criterion was not cleared to manufacture a pass.
No Rust command was needed for that correction.

The previous raw README was corrected, with only its digest changed in the existing 54-entry
manifest. The old README/manifest and pre-revision plan are preserved in the new raw root.
The previous timeouts, partial trace and unvalidated full-graph draft remain historical,
incomplete evidence; none was relabeled as a fresh result.

## Diagnosis before execution

Inspection used existing source, configuration and crate-specific build/fingerprint metadata,
plus bounded disk/memory observations. No process enumeration/control, shared artifact deletion,
clean, cold target, dependency change or build-profile override was used.

- Native fingerprint: `default`, no rustflags; wasm fingerprint: `canbench`, SIMD enabled.
  Test/dev debug information remains line-tables-only; release remains fat LTO, one codegen unit.
- Rust/Cargo/canbench versions and the as-used lock match the preceding slice.
- The saved resource observation had 6 GiB available. This observation had **23 GiB available**;
  `memory_pressure -Q` reported **28%** system-wide free memory. Swap usage was also recorded.
- A **2.055-second** vm_stat sample separately records deltas/rates: pageins 1,429, pageouts 135,
  swapins 217, swapouts 0, compressions 33,005 and decompressions 37,257. Host pages are 16 KiB;
  they are not IC stable-memory 64-KiB pages. Cumulative counters were not called pressure rates.

These observations **do not establish the prior timeout's cause**. There is no contemporaneous
compiler-phase/pressure/I/O trace for the interrupted builds. The previous native run spent
61s compiling, then made real progress; it supports neither a deadlock nor a slower IC-cost
inference. Release compilation took 111s before, 44.59s for the initial candidate and 44.87s
for the final candidate; these different build times are not attributed to the optimization
or fixture change.

The evidence-backed scope adjustment was to execute the small owner witnesses first, without
registering the 16K diagnostic or instantiating two new complete graph types. The first attempt
kept the original 100s cap and passed in **37.799s**, with 36s compilation and 0.01s runtime.
Only then was the missing current IC baseline attempted, still with a 150s cap. This is test-layer
cost control and an observed viable execution, **not proof that the old draft caused a timeout**:
the earlier canbench timeout preceded that draft.

[Build/configuration/resource evidence](sources/adr0097-left-pack-build-observations.json).

## Test boundary and implementation

`761b2a808` adds three tests in the existing `compact.rs` test module:

- `left_pack_bounded_four_byte_selector`
- `left_pack_bounded_ten_byte_selector`
- `left_pack_bounded_caller_falls_back_and_preserves_neighbor`

The first two each construct one real `EdgeStore` over six separate backed memories and a
`LabelBucket`, not a complete graph. A 35-slot prefix starts at slot 3, with distinct bytes
immediately outside it. Ten cases per width cover holes, partial final reads, a nonzero cursor,
no move, exhausted/out-of-range cursors, zero live degree and an empty prefix. They assert the
exact first movement, complete decode sequence and stop, initialized read bounds, row alignment,
no reads for empty/exhausted scans, and byte-identical descriptor/all six memories after selection.
A separate invalid-first-address case asserts exact `Store(CollectAllocationOverflow)`, no reads
and unchanged backing. The caller test uses the existing four-byte graph type and confirms that
a no-move scan at cursor 6 falls back to `1 → 0`, preserving the neighboring descriptor/rows and
exact source ordinals/counts.

Constructor inventory: the old unvalidated draft had two complete graph fixtures with new
edge/memory specializations. The replacement has **two EdgeStore fixtures plus one existing-type
graph fixture**, no new complete-graph specializations, and zero PocketIC constructors/installs.
The test filter still compiles the library test target. New edge/memory wrappers exist only to
observe decoder calls and reject out-of-owner reads; backing uses the existing VectorMemory.

All three tests pass **before and after**, with byte-identical test bodies. They extend coverage;
there is no claimed baseline correctness red. Two deliberate wrong implementations discriminate:

- Removing the initialized-prefix clamp: **1 passed / 2 failed**, both widths reject an overread.
- Decoding each entire prefetched buffer eagerly: **1 passed / 2 failed**, exact decode sequences
  reject the suffix/duplicate decodes even though move selection remains correct.

Both mutations were checked on the initial candidate and removed. Final compaction-filter
execution passes **84/0**, including the
accepted retained-run event/observer/retry, independent values, exact errors and interleaved-delete
contracts. The three new tests are included in that filter: totals are not additive.

`aab46e51f` changes only `first_edge_slot_move_in_bucket` and the crate visibility of the existing
`INLINE_EDGE_BYTES = 64` constant. It uses the existing EdgeStore contiguous-read API with a
fixed stack buffer: up to 16 four-byte or six ten-byte rows per read. Each read stops at the
initialized prefix and checked address limit, including the partial final window. Per-slot
checked addressing remains in the original order, so a later overflowing address cannot reject
an earlier selected move. Decoding stops at the original selected edge. Widths not fitting the
scratch, including the zero-width generic case, retain the existing scalar read path.

Final inspection found an admission distinction: `transcribe_and_publish_tree` stores logical
extent in the tree descriptor, while allocating only its edge/property root arrays in the slab.
`ea12e7c12` therefore restricts the new prefetch to slab descriptors; tiny/tree descriptors keep
the old scalar path. The predicate is computed per call, not persisted. This fixes the new
read path's admission without changing existing mode dispatch, move selection or errors. It is
not a repair or validation of legacy tree-mode compaction. `first_compaction_move_in_bucket`
skips tiny rows but not trees; both its left-pack and swap selectors scan `stored_slots` when
reached. At that measured source, the shared caller-admission/selection owner still needed
separate reachability review and valid-backed Tree-CSR coverage, not a fix to just one selector.
No artificial tree fixture was treated as a valid-backed Tree-CSR witness. The subsequent
[tree-mode correctness slice](adr0097-tree-compaction-admission.md) establishes slab-triggered
sibling reachability and repairs shared admission through finalization; it does not rerun or
extend this instruction comparison.

That necessary admission refinement changed production after the initial measurements, so the
same IC target, candidate native probe and final scoped checks were repeated on the changed
source. No unchanged baseline was rerun. Initial `aab46e51f` measured 7,230,320,227 instructions;
that observation remains preserved but is superseded by the final 7,185,627,417. No causal
instruction attribution to the admission predicate or code generation is claimed.

No heap scratch allocation or new allocation error, public parameter, persisted state, index,
cache, schema, event batch, compaction policy, slack/threshold or benchmark-helper change was
introduced. Scratch is rebuilt per invocation; it does not change interleaved-delete invalidation
or caller cursor/fallback semantics. Physical publication and the bounded run-fold owner are
unchanged. The measured/tested read bound is the slab descriptor's initialized prefix; this is
not an all-mode/all-caller admission audit.

## Work equivalence and instruction measurement

Current IC measurement uses exactly:

```sh
cd crates/ic-stable-lara
canbench bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384 --csv
```

Before: exit 0, **131.846s**, source `761b2a808`.
Initial candidate: exit 0, **50.906s**, source identical to `aab46e51f`.
Final candidate: exit 0, **51.377s**, source identical to `ea12e7c12`.
All report zero heap/stable-memory growth for the benchmark. The protected foreign CSV was
restored after each command; no `--persist`, full survey or optional small control was run.
Source/config/benchmark hashes, as-used locks and wasm hashes are recorded in
[before](sources/adr0097-left-pack-canbench-before-provenance.json),
[initial](sources/adr0097-left-pack-canbench-after-provenance.json) and
[final](sources/adr0097-left-pack-canbench-final-provenance.json) provenance.
The after command ran on the recorded uncommitted diff; run metadata verifies its hashes against
the eventual implementation commit. Tests are cfg(test)-only; the baseline's production code is
equivalent to `af063ea42`, apart from the already-reviewed comments.

Small native work is directly paired on both sources: [40 observed rows](adr0097-left-pack-owner-work.csv).
For example, the ten-byte case selecting `2 → 0` changes **3 reads/30 bytes → 1 read/60 bytes**,
with three decodes unchanged. Scanning all 35 rows changes **35 reads/350 bytes → 6 reads/350 bytes**,
with 35 decodes unchanged. Fewer reads do not necessarily mean fewer bytes or imply an IC price.

The unchanged corrected 16K probe ran on the initial candidate (19.258s, exit 0), then on the
admission-refined final candidate (**19.852s**, exit 0, including 11.33s compilation).
Each log verifies the compiled canonical manifest; recorded probe/production hashes match the
respective sources. Temporary registration/source were removed before final checks. Neither
execution was a rerun of the unchanged historical baseline. Both current cursor strategies assert byte-identical sixteen
pre-compaction memory images, 8,192 exact `2*j+1 → j` notifications, advance/Finished in 8,194
calls, no error/run rewrite/growth, zero unreported moves, and event-replayed final rows in both
orders. All six current step/move/final TSVs byte-match the preserved `15ceaabf1` traces.

| Native compaction | Historical `15ceaabf1` zero | Current zero | Historical `15ceaabf1` forwarded | Current forwarded |
| --- | ---: | ---: | ---: | ---: |
| All-role reads | 67,182,614 | **11,240,812** | 33,640,470 | **5,651,138** |
| Read bytes | 673,047,193 | **673,211,013** | 337,625,753 | **337,830,513** |
| Live decodes | 33,574,912 | 33,574,912 | 24,576 | 24,576 |
| Dead decodes | 33,566,720 | 33,566,720 | 33,574,912 | 33,574,912 |
| Writes / bytes | 16,389 / 245,878 | same | 16,389 / 245,878 | same |
| Grows | 0 | 0 | 0 | 0 |

Historical columns are preserved mechanism evidence, **not fresh measurements or the new IC
baseline**. In the 204 phase/role records, only compaction slab read calls/bytes and their total
rows differ; all other traffic and decode counts agree. Read bytes increase by 163,820 / 204,760
because prefetch can extend past the selected edge while remaining inside the initialized prefix.
Unchanged quadratic decode work is decisive: batching has not changed the algorithmic behavior.
The two candidate variants have identical observed work and traces.
[Final phases](adr0097-left-pack-native-phases.csv), [final native log](sources/adr0097-left-pack-native-final.txt).

## Validation, budget and remaining gates

Exact commands, environments, exits, elapsed times, source/lock/probe hashes, trace comparisons
and artifact mappings are in [run metadata](sources/adr0097-left-pack-run.json) and the
[single ledger](sources/adr0097-left-pack-ledger.json). Scoped final commands:

```sh
cargo test -p ic-stable-lara --lib compact -- --nocapture
rustfmt --edition 2024 --config skip_children=true --check \
  crates/ic-stable-lara/src/labeled/graph/compact.rs crates/ic-stable-lara/src/lara/edge.rs
cargo clippy -p ic-stable-lara --lib -- -D warnings
```

All exit 0 after temporary probe/mutation removal: **84/0**, format pass, strict library clippy
pass. Clippy is library-only, not a claim that all pre-existing test warnings were lint-clean.
No full-library runtime, full workspace, persisted survey, space audit or PocketIC run occurred.
Final source is byte-identical to the measured candidate. Range whitespace and preservation
checks pass; original benchmark source/YAML/config, foreign brief/CSV, restored Cargo.lock and
main are unchanged. The revised plan's [final structural check](sources/adr0097-left-pack-plan-final.txt)
passes after actual completion; it does not alter the two historical failures or grant
independent acceptance.

Lizard 1.24.0 (`--csv -i -1`) records final finder CCN **9 → 14**, NLOC **31 → 52**, tokens
**181 → 365**, and unchanged three parameters. The added branches handle slab-only admission,
bounded buffer refill and preservation of the existing scalar width/mode path. No new production module/type/helper was introduced. Metrics support
scope review; they are not speed or independent-acceptance evidence.

The ledger records **382.63679633138236s measured monotonic command time**, plus a separately
identified **30s conservative setup charge**, for **412.63679633138236 / 600s** charged;
**187.36320366861764s unused**. The 120s final reserve was protected before non-final commands.
The setup charge covers a non-Rust pre-ledger script-name error: `inspect.py` shadowed Python's
stdlib module during runner import. It was renamed; no Rust ran in that failure. The charge is
the full tool cap, not a fabricated measured duration. Resource observations from that script
remain labeled observations. No timeout occurred in this turn's Rust/canbench commands.

Raw root: `$TMPDIR/spill-owner-7e946-or50uro5/`, with SHA256 manifest. Durable logs trim terminal
blank lines only; raw logs and ledger hashes remain unchanged. The pinned lock is
`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`; compilation used
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`.

The next action is bounded independent review of the implementation, tests and paired evidence.
The remaining cost is still quadratic. A different progress algorithm needs a concrete owner,
state and invalidation proposal before widening this slice. Other benchmark increases, broader
caller/mode admission, wide edges/arena exhaustion, unpinned accounting, canonical-write atomicity,
space, PocketIC/lifecycle and final-source complete persisted benchmark gates remain open.
