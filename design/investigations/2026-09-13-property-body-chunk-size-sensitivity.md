# Property bodies: wider capped chunks

Status: Experimental — native and IC query benchmarks complete; no production storage selected.
Anchor: 2026-09-13 12:23:17 UTC +0000.
IC runtime verified: 2026-09-13 12:30:53 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## Conclusion

**Increasing the chunk size helps some operations, but does not remove multi-chunk mutation cost.**
With 16 KiB/64 KiB chunks, a full 1 MiB replacement still costs **3.44×/3.85×** the Whole control;
shrink costs **5.28×/5.12×**. The 16 KiB shrink also grows stable memory by one 64 KiB page in the
measured query. Neighbor isolation worsens with the larger caps. None is an unconditional winner.

This answers a structural question without assigning fictional traffic weights. The
[demo profile](2026-09-13-property-body-workload-profile.md) contains no Value above 143 B and no
old/new operation history. It does not establish how often these large-body cases occur. No chunk
size, directory threshold, production format, total-work budget or allocator is selected.

## Same owner, additional axes

The [shared probe](artifacts/2026-09-13-property-body-btree-probe.rs) gains only fixture calls: P=16384
and P=65536, both D=0. `Chunk::check_width` still enforces P in the existing codec. `Bound::Unbounded`
selects the library's default 1024 B node geometry, not uncapped chunk contents. The library remains
exact `ic-stable-structures 0.7.2`; no store, allocator, schema, helper, backend or algorithm is copied.

The original [comparison](2026-09-13-property-body-btree-comparison.md) defines key width, raw bytes,
complete range reads, previous-value acquisition and setup/measurement boundaries. Those are unchanged:

- Common 18-byte keys: 14-byte opaque body prefix plus u32 BE chunk ordinal; Whole uses ordinal zero.
  This is not Graph's 8/14-byte property directory or Value envelope.
- Full old body acquired/validated before inserts and obsolete suffix removals. Every replacement
  changes every byte. No partial patch, unchanged-chunk elision, batch-node API or alternate shrink path.
- Native counted VectorMemory and Wasm direct Ic0StableMemory use the same Store. Caller length is
  not persisted; same-type native reopen is tested, cross-configuration reopen is not supported.
- IC primary target is the higher prefix with a 65 B guard; neighbor controls select 65 B beside
  1 MiB. Native neighbor series selects the lower prefix and has prior growth history. Do not equate
  exact native and IC costs or claim a production read/work allowance.
- Setup/payload generation/small sanity cycle/prechecks/audits/logging/returned Vec destruction stay
  outside timing. Replacement includes full previous-value acquisition. Sanity does not prewarm a
  large replacement/shrink. Position-bearing payloads make chunk reordering observable.

Seven native configurations produce **260 rows** (7×36 plus 8 Whole point controls); the original
188 rows are exactly unchanged. The [native CSV](artifacts/2026-09-13-property-body-chunk-size-native.csv)
retains every case, including original declared-geometry penalties.

The [complete IC result artifact](artifacts/2026-09-13-property-body-chunk-size-results.yml) has
**48 cases**: the original 32 plus two caps × eight cases (64 KiB/1 MiB read/rewrite/shrink, neighbor
read/rewrite). All original 32 instruction/heap/stable counters remain identical. No old case was
replaced. Native density/neighbor fixtures retain below-cap 65 B/4 KiB coverage; no extra IC
below-cap cases were added merely for reassurance.

## IC instructions

All chunk columns use default geometry. Shrink means input→65 B.

| Input bytes | Operation | Whole | C4KiB | C16KiB | C64KiB |
| ---: | --- | ---: | ---: | ---: | ---: |
| 65536 | read | 217870 | 267661 | 227152 | 217942 |
| 65536 | rewrite | 449353 | 2180407 | 990209 | 449472 |
| 65536 | shrink | 330661 | 2185044 | 622103 | 330780 |
| 1048576 | read | 3374222 | 4282108 | 3597363 | 3432642 |
| 1048576 | rewrite | 6943634 | 28319864 | 23887453 | 26750515 |
| 1048576 | shrink | 5057917 | 38270666 | 26713422 | 25883933 |

At 64 KiB, C64KiB has one entry and is close to the shared Whole path. At 1 MiB it has 16 chunks:
replacement is again materially amplified. Fewer chunks alone do not determine the best cap;
C64KiB replacement is slower than C16KiB despite having four times fewer chunks.

For 1 MiB, relative to the shared Whole range/assembly read and full rewrite contract:

| Operation | C4KiB / Whole | C16KiB / Whole | C64KiB / Whole |
| --- | ---: | ---: | ---: |
| read | 1.27× | 1.07× | 1.02× |
| rewrite | 4.08× | 3.44× | 3.85× |
| shrink | 7.57× | 5.28× | 5.12× |

Ordinary Whole point-get remains a distinct, cheaper read baseline: **1274305 instructions** for
1 MiB. Chunk reads cost 3.36×/2.82×/2.69× that API for 4/16/64 KiB respectively. It skips the shared
prefix/length validation and assembly copy; it is not silently substituted into rewrite preflight.
Neither baseline measures Graph Value decoding or complete query execution.

### Neighbor cost

Selected 65 B body beside a 1 MiB body:

| Operation | Whole | C4KiB | C16KiB | C64KiB |
| --- | ---: | ---: | ---: | ---: |
| read | 100356 | 36442 | 51674 | 81131 |
| rewrite | 2621320 | 187883 | 502145 | 1694280 |

Larger chunks give back much of the neighbor isolation. At C64KiB, this rewrite is about nine times
C4KiB's cost, although still cheaper than Whole. These are **external-body** controls: a directory
threshold T>=65 removes this specific 65 B selection from the body lane. Directory costs and its
changed body-tree population are unmeasured, not zero. No threshold is chosen from this table.

## Native I/O and retained space

These are Memory-interface calls/bytes, **not** IC syscalls, instructions, latency or total heap.
The 1 MiB neighbor-series target retains the original prior growth/order history.

| Configuration | 1 MiB read calls / bytes | Full rewrite read calls / bytes | Full rewrite write calls / bytes |
| --- | ---: | ---: | ---: |
| Whole | 2076 / 1060002 | 4154 / 2120087 | 1045 / 1048708 |
| C4KiB | 2925 / 1066618 | 33878 / 6861313 | 10472 / 5559316 |
| C16KiB | 2270 / 1061530 | 25936 / 8093525 | 8241 / 6860248 |
| C64KiB | 2123 / 1060356 | 21467 / 10217406 | 9407 / 9047736 |

Native full-rewrite bytes **increase** with the wider caps even while IC instruction costs improve
relative to C4KiB. Calls, bytes and instructions are different measurements. The upstream
[`Node::save_v2`](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node/v2.rs#L217-L298)
loads/saves all node entries for each save; a node can contain several wider chunks. The same source's
`read_overflows` reconstructs the whole overflow chain. Fewer entries reduce repeated tree operations
but do not remove shared-node copying. These observations are not a phase-level instruction profile
or a claim that exact byte ratios predict speed.

Rewriting just the 65 B neighbor writes 16582/65734/262342 bytes for C4/C16/C64KiB, versus 1048708
for Whole. Larger caps also enlarge potential per-node work. Owner-capped values do not by themselves
bound tree height, all metadata/allocator work or arbitrary corrupt upstream lengths.

All four default-geometry configurations use **65536 B** after inserting 256 distinct 65 B bodies
(16640 B payload). Their density-series native counters match. This excludes a directory, its codec,
keys, extra regions and MemoryManager floors. The existing declared-geometry controls still consume
2 pages for C256 and 27 for C4096; no large declared maximum is newly selected here.

Hot-body fixture: 65 B→1 MiB→65 B, same-type reopen, eight alternating replacements (four grow/shrink
pairs), then delete. Repeated-operation counters after reopen:

| Configuration | Retained backing bytes | Grow write bytes | Shrink write bytes |
| --- | ---: | ---: | ---: |
| Whole | 1114112 | 1134526 | 66350 |
| C4KiB | 1179648 | 12455003 | 10409385 |
| C16KiB | 1179648 | 10867263 | 9582735 |
| C64KiB | 1114112 | 7814173 | 9605237 |

All eight repeated operations reuse capacity without growth. C16KiB's **initial shrink** grows
1114112→1179648 B (17→18 pages); its high-water table includes this, rather than hiding it in a
no-growth claim. Final delete leaves zero entries but does not physically shrink backing. C64KiB's
Whole-sized retained backing does not imply Whole-like mutation cost or graph-wide space parity.

## IC page observations and limits

At 64 KiB input, all four primary configurations start/end with two stable pages. At 1 MiB:

| Configuration | Before | After read/rewrite | After shrink |
| --- | ---: | ---: | ---: |
| Whole | 17 | 17 | 17 |
| C4KiB | 18 | 18 | 18 |
| C16KiB | 17 | 17 | 18 |
| C64KiB | 17 | 17 | 17 |

Thus 47 cases have zero stable-page increase; **C16KiB 1 MiB shrink has +1 page**. All 48 report zero
heap-page increase. Prechecks/seed already allocate heap and tree capacity, so zero delta is neither
zero allocation nor zero dirty pages/peak heap. The 16 KiB native and IC shrink-growth observations
are distinct fixtures; the IC query does not establish durable update commitment.

Canbench exports queries backed by real stable64 calls. It does not prove IC update rollback,
reply-loss safety, upgrade or persistent descriptor co-write. The fixed 2021 canister-clock prefix
in output is not this report's verification date. No Graph policy, directory, Value, journal/outbox,
Candid request decoding, MemoryManager or cross-canister path is measured. Whole is an unbounded
measurement control, not an accepted bounded production implementation.

## Disposition

- Retain C4KiB as the earlier reference, with C16KiB/C64KiB as measured sensitivity points, not runtime
  configuration knobs or approved alternatives. Wider chunks do not eliminate the mutation problem.
- Do not choose a cap from an unobserved read/write mix, a tiny demo maximum or the favorable single-
  chunk 64 KiB result. Large rewrite/shrink, neighbor spillover and shrink-time allocation remain
  explicit costs. Numerical per-node/whole-row budgets still need an owning production contract.
- A further optimization should target the repeated node-save mechanism or supply an application
  workload/requirement, rather than automatically sweeping more powers of two. No existing-library
  batch API, dependency fork or custom allocator was implemented or justified by this slice.
- Production storage and bulk edge SET remain unchanged/unavailable respectively. Custom extents
  remain deferred and unmeasured; their safety/maintenance cost requires separate authorization.

## Reproduction and validation

Use the isolated manifest/config recipe in the
[original IC reproduction](2026-09-13-property-body-btree-comparison.md#reproduction-and-validation)
and the same [transitive lock](artifacts/2026-09-13-property-body-btree-canbench.Cargo.lock). The shared
source now runs seven native configurations and 48 IC cases. Keep a new output directory; do not
replace the historical 32-case YAML with expanded results.

Exact evidence directory: `/tmp/gleaph-body-chunk-size-f3t1g3lj/`.
From repository root (`probe` set to that directory):

```sh
cargo test --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$PWD/target" --lib -- --nocapture
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$PWD/target" --lib --tests -- -D warnings
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$PWD/target/canbench" --target wasm32-unknown-unknown --release --lib -- -D warnings
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-property-body-btree-probe.rs
(cd "$probe" && canbench --persist --csv --show-canister-output --runtime-path /Users/yota/dev/gleaph/crates/ic-stable-lara/.canbench/pocket-ic)
python3 "$probe/final_checks.py"
python3 /Users/yota/.agents/skills/plan/scripts/validate_plan.py plans/0355-property-body-chunk-size-sensitivity.md --phase final
```

Both existing native tests passed, with the two new caps also running complete malformed/budget
preflight, exact previous/new/guard bytes, suffix removal and same-type reopen checks. There are
27 B-tree creations and 21 reopens per suite (previously 19/15), maximum 4097 entries/256 bodies in
any fixture. The distinct-chunk assertion now includes all four capped widths. Final run: 1.56s build,
2.88s runtime, two passed; the initial run also passed and produced identical 260 rows. No semantic
red-to-green or production algorithm optimization is claimed.

Native Clippy passed (0.79s), Wasm release Clippy passed (0.92s), rustfmt passed. One unfiltered
canbench run completed/persisted all 48 cases after a 4.42s release build: 48 evaluations/Store
constructors, each initially zero stable pages. No manual PocketIC/federation setup or separate E2E
suite, timeout, thread override, fallback, workspace build or production canbench update. Existing
rustc `1.100.0-nightly (c656540d6 2026-08-21)`, canbench 0.7.0, fat LTO/codegen-units=1 and repository
`+simd128` are unchanged. No shared target cleaning or workspace manifest/lock edit.

Lizard 1.23.0, unexpanded source: 32→32 functions, max CCN 9→9, sum 68→68, function NLOC 641→649.
Generated benchmark wrappers are not expanded in those metrics. The source diff adds eight native
fixture invocations and 16 benchmark exports, plus extends the distinct-width list; no new types,
flags, parameters or helper signatures. The seven-path slice preserves the original 32-case YAML,
its lock, prior probes/plans/workload artifacts, thirteen prior-only tracked dirty paths, HEAD and
empty index. Review is same-assistant, not independent/security approval; no staging or commit.

Measured source SHA-256: `69a13c8abf2e0a2203a53c1c660eb3526829a314bd59b169a71670c8ed87604b`.
Wasm SHA-256: `4a907351c3b85c032796f392a696d8990090561ca4fa8bb173aa98a556c9b2fd` (841032 B).
Full logs, input snapshots, final checks, scoped diff and review are retained in the evidence directory.
