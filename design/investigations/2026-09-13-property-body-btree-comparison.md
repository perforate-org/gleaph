# Property bodies: whole versus chunked B-tree storage

Status: Experimental — native and IC stable-memory comparisons complete; no production storage selected.

Design anchor: 2026-09-13 09:20:29 UTC +0000.
Native verified: 2026-09-13 09:36:59 UTC +0000.
IC runtime verified: 2026-09-13 11:08:59 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

Native reuse update (2026-09-14 01:10:06 UTC +0000): the [full PropertyStore comparison](2026-09-13-property-store-native-comparison.md)
adds real key widths, Value codec, directories and moves. The shared body source gained canonical-key
and private prepare/publication support;260+36 raw rows still reproduce. Historical IC numbers are
not measurements of this changed codegen. Exact0357 source bytes are retained in the
[provenance archive](artifacts/2026-09-13-property-body-0357-sources.tar.gz); use the linked managed
report's frozen-source recipe for its IC reproduction. No production implementation is selected.

## Decision boundary

This investigation compared reuse of the installed B-tree before implementing the
[custom extent proposal](2026-09-13-property-body-extent-design.md). A separately authorized
[native extent follow-up](2026-09-13-property-body-extent-comparison.md), verified on
2026-09-13 14:02:33 UTC +0000, now measures that prototype; it does not change these historical
B-tree results or establish IC/production acceptance. The later
[managed IC comparison](2026-09-13-property-body-extent-canbench.md), verified on
2026-09-13 20:50:20 UTC +0000, now compares extents with these body algorithms under one memory
manager/policy. It retains adverse point-read, fragmented-allocation and extra-region costs.
The earlier
[fixed-block experiment](2026-09-13-property-value-separation-design.md#8-probe-results-and-disposition)
remains complete and unselected; its full-store measurements are not an extent benchmark.

**Result:** capped chunks reduce the effect of an unrelated large body, but repeated per-chunk
insertion/removal amplifies large-body writes. Declaring a large Storable maximum also wastes space
in a short-body population under 0.7.2's constructor geometry. Owner-capped chunks with the default
geometry avoid that particular density penalty, not the write amplification. No candidate is an
unconditional winner and no custom-allocator performance claim follows.

The [IC follow-up](#ic-stable-memory-follow-up) confirms the trade-off: C4096/default reduces the
small-neighbor rewrite's instructions about 14×, but costs 4.08× for a full 1 MiB replacement and
7.57× for shrink. Those are body-lane measurements, not Graph workload or latency ratios.

The [repository workload profile](2026-09-13-property-body-workload-profile.md) verified on
2026-09-13 11:35:15 UTC +0000 finds no Value above 143 encoded bytes in Knowledge or Social demo
load inputs. These corpora do not exercise the large-body cases, and static property counts are
not read/rewrite/shrink frequencies. Threshold sensitivity is reported separately; it selects
neither a directory threshold nor a production body implementation.

The [wider-chunk follow-up](2026-09-13-property-body-chunk-size-sensitivity.md), verified on
2026-09-13 12:30:53 UTC +0000, adds 16/64 KiB default-geometry chunks. Larger chunks improve some
instruction costs but retain large-mutation amplification and worsen neighbor isolation; the
16 KiB 1 MiB shrink grows stable memory by one page. No production cap is selected.

**Recorded runs versus shared source:** the sections below retain the original five-configuration
188-row native run and 32-case IC run. The shared source/reproduction commands now execute seven
native reference configurations (260 rows), plus four extent tests/36 rows: six native tests total,
with all native counters preserved. IC commands now execute91 managed cases, including all48 prior
control names but with remeasured managed counters, not the historical raw-backend numbers. The
canonical managed reproduction and full result live in that follow-up. The source hash at the end
records the earlier32-case raw revision; neither old32/48-case YAML is overwritten.

## What was compared

[Diagnostic source](artifacts/2026-09-13-property-body-btree-probe.rs), exact
`ic-stable-structures = "=0.7.2"`, native VectorMemory. Five configurations:

| Name | Actual per-entry body cap | Storable declaration | B-tree node page selection |
| --- | ---: | --- | --- |
| Whole | No chunk cap | Unbounded | Default 1024 B |
| C256/declared | 256 B | Bounded, max 256, variable length | Derived from key/value maxima |
| C256/default | 256 B | Unbounded | Default 1024 B |
| C4096/declared | 4096 B | Bounded, max 4096, variable length | Derived from key/value maxima |
| C4096/default | 4096 B | Unbounded | Default 1024 B |

`Chunk::check_width` enforces the actual chunk cap in both encoding entry points and decoding.
Unbounded in the two **default** configurations is the upstream constructor declaration, not
permission to write arbitrarily large chunks. No padding to the declared maximum is performed.
The whole-body control has no chunk cap; all real fixture bodies are at most 1 MiB.

Every map uses the same fixed 18-byte key: 14-byte opaque test body prefix plus u32 BE ordinal.
Whole bodies use ordinal zero. Holding key width equal isolates the storage choice; a real
whole-body-only map could omit those four ordinal bytes. Neither case is the production 8/14-byte
property directory or its one-byte Value envelope. Raw bodies have no extra version envelope here.

Total length comes from a caller-supplied test descriptor, **not a new persisted length map**.
Reads use one ordered prefix range, not a repeated root lookup per chunk. They require consecutive
ordinals, exact interior/final lengths and complete termination; no partial body is returned.
Supplementary ordinary `get` controls for Whole preserve that API's cost comparison: their native
read counts/bytes matched the range read in the two-body fixtures, though output allocation differs.

Rewrites acquire and validate the complete previous body before the first write, overwrite all new
chunks, then remove obsolete suffix chunks. Every replacement changes every byte; no unchanged-chunk
elision or partial-patch API is measured. Empty replacement deletes the body; a stored empty body
is outside this diagnostic. All configurations include the previous-value read in rewrite counters.
The same producer/ordering/cap implementation serves the five configurations; these are test axes,
not production configuration flags.

## Measurements

Numbers in this Measurements section are **native Memory-interface calls/bytes**, not IC syscalls, Wasm instructions,
latency, total heap usage or Graph execution. Input construction, initialization/reopen, audits and
result assertions are outside operation counters. The 65-byte sanity operation in the density map
is also outside measurement; it leaves reusable node capacity, not a second fixture.

There are 188 rows: 36 per configuration plus eight Whole point-get controls. `input_bytes` means
neighbor size throughout the neighbor series, per-body size in the density series, and new body
size in hot rewrites. `body_buffer_bytes` counts explicit assembled-body Vec requests only;
upstream extraction buffers, returned old chunks and native allocator overhead are not counted.
A zero value in the ordinary point-get control is **not zero heap allocation**.

### Short-body density

256 distinct 65-byte bodies, 16640 payload bytes total. These are body-lane entries; no directory
threshold is chosen. A higher eventual small-cell threshold could keep these bodies out of the
body lane entirely and changes the relevant population.

| Configuration | Page-backed bytes after insertion | Native 64 KiB pages |
| --- | ---: | ---: |
| Whole | 65536 | 1 |
| C256/declared | 131072 | 2 |
| C256/default | 65536 | 1 |
| C4096/declared | 1769472 | 27 |
| C4096/default | 65536 | 1 |

The 27-page result is a constructor/node-occupancy effect, not 4 KiB padding of each value.
Bounded maxima select a larger node allocation unit even when actual entries are short. Default
geometry had identical read/write counts to Whole in this one-chunk-per-body density fixture.
Deleting alternate bodies, reopening and refilling the holes preserved exact values and required
no further growth in all five configurations. These are observations for this shape, not a
universal no-growth or 27× storage ratio.

### A 65-byte body beside an increasing neighbor

Selected read calls / bytes:

| Neighbor bytes | Whole | C256/default | C4096/default |
| ---: | ---: | ---: | ---: |
| 65 | 7 / 124 | 7 / 124 | 7 / 124 |
| 4096 | 11 / 168 | 18 / 218 | 11 / 168 |
| 65536 | 71 / 828 | 37 / 473 | 37 / 427 |
| 1048576 | 1042 / 11509 | 57 / 692 | 94 / 1100 |

Chunking limits each node's contribution from payloads, but adding a large body adds chunk entries
and can increase tree height. Thus tiny reads are **not constant work** as the map grows. The
C4096/default missing-prefix control reads 1035 B at the largest neighbor versus Whole's 11444 B.

At the 1 MiB neighbor, rewriting only the 65-byte selected body:

| Configuration | Read bytes | Write bytes |
| --- | ---: | ---: |
| Whole | 1071594 | 1048708 |
| C256/declared | 2356 | 1222 |
| C256/default | 2444 | 1222 |
| C4096/declared | 17300 | 16582 |
| C4096/default | 18620 | 16582 |

The C4096/default write volume is about 63× smaller, but not isolated to 65 B: other chunks in the
same B-tree node are still rewritten. This is distinct from independent raw bodies, which do not
share a node's payload rewriting. A separate Whole-body B-tree would isolate directory-only small
updates, but this body-lane coupling would remain when the selected body itself is external.

### Selecting and rewriting the 1 MiB body

Same two-body fixture, same previous-value acquisition and changed-byte sequence:

| Configuration | Get calls / read bytes | Rewrite read bytes | Rewrite write bytes |
| --- | ---: | ---: | ---: |
| Whole | 2076 / 1060002 | 2120087 | 1048708 |
| C256/declared | 14066 / 1157508 | 8843825 | 5860800 |
| C256/default | 15699 / 1166495 | 9024225 | 5860800 |
| C4096/declared | 879 / 1055365 | 6677569 | 5559316 |
| C4096/default | 2925 / 1066618 | 6861313 | 5559316 |

C4096/default reads about 0.62% more bytes for the large get, but uses more calls. Its full rewrite
writes about **5.30×** as many bytes as Whole. C4096/declared reduces read calls but trades away the
short-body density above. C256 adds many more entries/calls without solving full-rewrite amplification.

The source explains the mechanism: `Node::save_v2` loads all entries and writes all values in the
node. Inserting successive chunks can therefore save the same node repeatedly. This probe already
uses ordered range reads; it does not manufacture the regression with per-chunk root `get` calls.
A node-batched mutation API, changed-chunk-only updates, or a library-owner change would be separate
candidates, not an optimization silently included in these measurements.

### Fresh hot-body grow/shrink and reopen

A separate map starts with 65 bytes, grows to 1 MiB, shrinks to 65 bytes, reopens, and performs eight
alternating grow/shrink rewrites. The table gives retained backing after initial warm-up and write
bytes for the repeated operations after reopen:

| Configuration | Retained backing bytes | Grow to 1 MiB: write bytes | Shrink to 65 B: write bytes |
| --- | ---: | ---: | ---: |
| Whole | 1114112 | 1134526 | 66350 |
| C256/declared | 2031616 | 14328299 | 12301116 |
| C256/default | 1703936 | 14515391 | 12372597 |
| C4096/declared | 1835008 | 12211187 | 10206174 |
| C4096/default | 1179648 | 12455003 | 10409385 |

All eight repeated operations reused capacity with zero growth; final deletion left zero entries
but did not shrink memory. C4096/default retained only one additional 64 KiB page versus Whole,
**not** a two-body replacement peak, but paid substantial insert/remove/rebalance I/O.

C4096/declared grew from 1769472 to 1835008 backed bytes during the **initial shrink**. A shrinking
logical body therefore did not guarantee zero allocation in the tree mutation. The warm-up includes
that observed extra page; the post-reopen no-growth assertions do not hide it.

## What this says about the custom proposal

| Consideration | Chunked B-tree | Contiguous extent prototype |
| --- | --- | --- |
| Physical allocation/reclamation owner | Existing library; tested native reuse | New tag/free-index co-write code; native/managed-IC checkpoints complete, production pending |
| Whole large-body I/O | Measured chunk/node amplification above | Native payload I/O and managed IC instructions measured separately |
| Space | Measured key/node/overflow/high-water costs | Native two-region floor, tags, free-index traffic and fragmentation measured separately |
| Resize | Library owns page reuse; repeated tree edits can be expensive | Native in-place/right-neighbor reuse verified; relocation still retains old+new capacity |
| Remaining production work | Select/integrate directory, chunk format and key/move ownership | Select/integrate directory/moves and prove whole-row allocation/failure safety |

A body B-tree needs one collection/region per PropertyStore rather than the extent proposal's raw
arena plus free index. With two stores and an assumed four-page-per-region backing policy this is
conditionally 512 KiB versus 1 MiB of new-region initial backing. Neither region policy nor MemoryIds
are selected; these are not the native page figures above or measured production footprints.

**Disposition:** keep C4096/default as a useful measured existing-library reference, not an accepted
production choice. Do not select C4096/declared for the tested short-body population, or assume
C256's smaller chunks make it a general improvement. Whole-body storage retains the demonstrated
unselected-body coupling. Frequent large replacements/shrinks are a material drawback for chunking.
The next decision needs representative body-size/read/rewrite/shrink workloads and bounded Graph
instruction/write admission. The separately scoped native extent comparison is complete, but its
implementation/safety cost and the managed IC trade-offs still need production evaluation; this B-tree slice selects neither.

## Contract evidence and omissions

Two tests passed: `body_btree_matrix` and `body_btree_rejects_incomplete_or_over_budget`.
The first has three independent maps per five configurations (15 creations/15 loads); the second
has four additional chunk maps (4 creations/no loads): **19 creations, 15 successful same-type
reopens, zero PocketIC constructors or installs**. At most 256 live bodies, 1 MiB/body and 4097 actual
entries in a map in this run (planned ceiling 8192). Fixtures are dropped sequentially.

Assertions cover exact distinct bytes and old values, all measured reads being write/grow-free,
missing-prefix lookup, nonzero and maximum body keys, same-size/grow/shrink/delete behavior, complete
chunk count/width and removal of stale suffixes. Reopen preserves stored bytes and library allocation
state. Full audits run outside counters. A fake no-op rewrite or skipped suffix deletion fails them.

Negative tests seed an independent maximum-key body and reject exact structural faults: missing
interior ordinal, missing final chunk, extra maximum ordinal, wrong interior width, overflow and
one-short read/replacement/deletion allowances. Failed operations leave serialized memory and map
length unchanged. An insufficient allowance skips the deliberately malformed chunk; adequate
allowance reaches its real width error. No synthetic Memory reads or panic-catching rollback test.
The initial implementation passed; no production red-to-green bug claim is made.

Important exclusions:

- Descriptors/lengths are test-caller inputs. A persistent directory, versioned chunk-size contract,
  body-key choice and property-key move/ownership transfer are **not implemented**. The prototype's
  P is compiled into the test type, not persisted in a new format header. Reopening with a different
  configuration is not a tested or supported contract. Key-coupled chunks would require chunk
  rekeying on moves; private indirection would require its own justified ownership design.
- Logical-byte rejection before lookup is not a total I/O/instruction allowance. Even capped chunks
  require a bound on tree height, metadata, writes/rebalance and memory-manager work. `from_bytes`
  checks occur after upstream extraction and do not protect against arbitrary corrupt lengths.
- No Value decoding/workspace bound, whole-row preparation, persistent directory co-write, outbox,
  canonical journal, reply-loss or IC rollback/upgrade proof. No stored NULL/empty semantics, edge
  occurrence selection or Graph public API behavior is changed.
- Native writes cannot simulate IC atomic rollback. Ordinary validation completes before the first
  insert/remove, with no recoverable error afterward; production integration still needs the owning
  Graph fence and resource checks. No heap-only state is proposed as production authority.

## Native reproduction and validation

From repository root:

```sh
repo="$PWD"
probe="$(mktemp -d /tmp/gleaph-body-btree-compare-XXXXXX)"
printf '%s\n' \
  '[package]' 'name = "gleaph-property-body-btree-probe"' 'version = "0.0.0"' \
  'edition = "2024"' 'publish = false' '[lib]' \
  "path = \"$repo/design/investigations/artifacts/2026-09-13-property-body-btree-probe.rs\"" \
  '[dependencies]' 'ic-stable-structures = "=0.7.2"' \
  '[profile.dev]' 'debug = "line-tables-only"' \
  '[profile.test]' 'debug = "line-tables-only"' > "$probe/Cargo.toml"
cargo test --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib -- --nocapture
cargo clippy --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --lib --tests -- -D warnings
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-property-body-btree-probe.rs
```

Verified run: 2 native tests passed (1.70s build + 2.49s runtime); scoped Clippy passed (0.31s)
and artifact rustfmt check passed. All 188 rows
were unchanged after centralizing codec cap validation. Native I/O was the first slice's measurement;
that slice did not run canbench, Graph/LARA/workspace tests or PocketIC. The IC follow-up below is
separate evidence, not a reinterpretation of these native results. Lizard 1.23.0: 28 functions,
max CCN 9, aggregate CCN 60, function NLOC 525; this new diagnostic has no source baseline or
production complexity delta. Helpers own concrete measurement, storage or fixture phases; no new
production API or signatures with five or more meaningful inputs were added.

Evidence: `/tmp/gleaph-body-btree-compare-jqnamg2j/` contains manifest/lock, native/Clippy transcripts,
188-row CSV, snapshots, checks and same-assistant review. This is not independent/security approval.
The source and commands reproduce the experiment without that evidence directory. No workspace
manifest/lock, production Rust/storage or previous probe was changed; no staging or commit.

## IC stable-memory follow-up

**Historical raw-backend checkpoint.** This section records the original32-case run. The shared
source's IC backend and reproduction are now owned by the linked91-case managed follow-up.

Whole and C4096/default execute the **same** key/cap/read/rewrite implementation as the native
probe. Native tests still use CountedMemory/VectorMemory; the Wasm diagnostic binds directly to
`Ic0StableMemory`, including its upstream `read_unsafe` implementation. There are no native counter
wrappers or synthetic memory operations in the timed Wasm path. This is an isolated diagnostic
canister, not a production Graph change or an additional storage implementation.

### Fixture and measurement boundaries

32 cases: two stores × four sizes × read/rewrite/shrink (24), four ordinary Whole point-get controls,
and four small-target neighbor controls. Each case requires empty physical stable memory and uses
one B-tree. It first sanity-checks read, changed-byte replacement, shrink and delete on a 65-byte
body, leaving an empty tree with reusable small-node capacity. It then seeds two bodies and checks
all exact bytes and global entry count before measurement. The prelude does **not** perform or warm
up a large-body replacement/shrink.

Primary cases select prefix `[2;14]`, with a 65-byte guard at `[1;14]`. Neighbor controls select
65 bytes at `[2;14]` beside a 1 MiB guard at `[1;14]`. The native neighbor series selected the lower
prefix instead: these IC results are not byte-for-byte cost reproductions of that native fixture.
Same-size replacements change every byte. Shrink targets are 65 B except 65→32 B, ensuring every
shrink case actually shrinks. Maximum live payload is 1 MiB + 65 B; maximum map size is 257 chunks.

`#[bench(raw)]`/`bench_fn` excludes initialization, payload generation, sanity/prechecks, result
assertions, logging and destruction of the returned Vec. Read output or the rewrite's complete old
body is retained outside the closure and compared exactly. Writes include previous-body acquisition,
logical admission, every chunk insertion and obsolete suffix removal. Full postchecks verify target,
unchanged guard and exact map entry count. Inputs and results pass through `black_box`.

Every fixture checks raw IC stable pages are zero before construction and positive after seeding.
Reported stable-page deltas must equal the raw backend's before/after difference; read deltas must
be zero. Query exports and canbench's runtime measure real stable64 operations, but **not durable
update-message commit**, reply loss, rollback or upgrades. The runtime's fixed 2021 canister-clock
log prefix is not the verification date above. No Graph directory, Value codec, MemoryManager,
policy, journal/outbox, Candid request decoding or cross-canister execution is included.

### Instruction results

Exact counters from the [complete 32-case artifact](artifacts/2026-09-13-property-body-btree-canbench-results.yml):

| Input bytes | Operation | Whole instructions | C4096/default instructions | Chunk / Whole |
| ---: | --- | ---: | ---: | ---: |
| 65 | read | 5592 | 5664 | 1.01× |
| 65 | rewrite | 12271 | 12390 | 1.01× |
| 65 | shrink | 12259 | 12378 | 1.01× |
| 4096 | read | 19027 | 19099 | 1.00× |
| 4096 | rewrite | 40606 | 40725 | 1.00× |
| 4096 | shrink | 33121 | 33240 | 1.00× |
| 65536 | read | 217870 | 267661 | 1.23× |
| 65536 | rewrite | 449353 | 2180407 | 4.85× |
| 65536 | shrink | 330661 | 2185044 | 6.61× |
| 1048576 | read | 3374222 | 4282108 | 1.27× |
| 1048576 | rewrite | 6943634 | 28319864 | 4.08× |
| 1048576 | shrink | 5057917 | 38270666 | 7.57× |

Up to one chunk, the shared algorithms are close, not identical. Multi-chunk writes/shrinks amplify
instructions as well as native I/O. The 5.30× native rewrite-byte ratio is **not** a 5.30× instruction
or latency claim. These costs belong to the measured algorithm/fixture, not a deployed regression.

Ordinary Whole `BTreeMap::get` returns the decoded Vec directly, avoiding the complete prefix-range
validation and assembled output copy. Its controls are deliberately retained:

| Body bytes | Whole point-get instructions |
| ---: | ---: |
| 65 | 2851 |
| 4096 | 8207 |
| 65536 | 84033 |
| 1048576 | 1274305 |

Thus the 1 MiB chunk read is 1.27× the **shared range/assembly** Whole read, but **3.36× the ordinary
Whole point-get**. Equal native read bytes did not imply equal instructions or allocation work.
The point-get controls do not validate the caller's complete prefix/length contract; no corresponding
point-get optimization is silently applied to the measured rewrite preflight. Neither read baseline
is a Graph Value decode benchmark.

For a selected 65-byte body beside a 1 MiB neighbor:

| Operation | Whole instructions | C4096/default instructions | Whole / Chunk |
| --- | ---: | ---: | ---: |
| Read | 100356 | 36442 | 2.75× |
| Rewrite | 2621320 | 187883 | 13.95× |

This advantage concerns an **external body**. If a future small-cell threshold keeps 65-byte values
in the directory, these particular operations would not enter the body lane at all. No threshold
or observed workload frequency is supplied by this experiment.

### Memory and interpretation

All 32 measured operations increased heap and stable memory by **zero 64 KiB pages**. That is not
zero allocation: prechecks assemble full bodies, seeding has already grown tree/heap capacity, and
returned/body/node buffers allocate and reuse heap within the measured operation. Initial growth,
temporary allocations, total heap and dirty-page volume are not the reported deltas. No large
replacement or shrink was used as a hidden capacity warm-up.

Raw stable pages immediately before and after each primary operation:

| Input body bytes | Whole pages | C4096/default pages |
| ---: | ---: | ---: |
| 65 | 1 | 1 |
| 4096 | 1 | 1 |
| 65536 | 2 | 2 |
| 1048576 | 17 | 18 |

Point controls have the corresponding Whole footprint. The 1 MiB-neighbor controls have 17/18
pages. Shrink leaves that backing allocated; logical deletion is not physical memory shrinking.
The extra page is a two-body raw-memory observation, not a whole-graph growth ratio or a production
MemoryManager region-floor measurement. The native declared-geometry shrink-time allocation remains
an adverse result; this follow-up did not include that configuration.

**Disposition:** retain C4096/default as a reference, not an unconditional choice. Frequent external
small-body operations beside large bodies benefit; full large replacements/shrinks have materially
higher costs. Whole remains an unbounded-body control with neighbor coupling. Representative body
sizes and operation frequencies, and concrete Graph total-work budgets, are needed before adoption.
These IC ratios do not establish extent speed or safety. The separately authorized native extent
follow-up and managed IC comparison are complete; production integration remains pending.

### Reproduction and validation

The [retained raw-run lockfile](artifacts/2026-09-13-property-body-btree-canbench.Cargo.lock) records
this historical checkpoint. For the shared source, use the
[canonical managed reproduction](2026-09-13-property-body-extent-canbench.md#reproduction-and-provenance)
and its new lock; the old lock lacks the manager dependency. The measured raw-run compiler was rustc `1.100.0-nightly (c656540d6 2026-08-21)`, LLVM 23.1.0, canbench 0.7.0.
Release fat LTO/codegen-units=1 matches the workspace; building from the repository root retains
`.cargo/config.toml`'s `+simd128`. Other compiler/dependency/backend configurations may change counters.
Offline commands assume the pinned dependencies and the canbench runtime are already cached.

The superseded raw-backend build recipe is replaced by that single canonical managed recipe;
its runtime counters must not be compared as though memory conditions were unchanged.

Final native validation passed (2 tests, 1.51s build + 2.90s runtime), with all **188 rows exactly
unchanged** and the same 19 creations/15 native reopens per suite. Final native Clippy (0.31s), Wasm
release Clippy (1.10s), and rustfmt passed. The smoke case passed after a 16.58s Wasm build. Both
unfiltered isolated-crate runs completed all 32 cases and persisted YAML/CSV; the second followed a
fixture-strengthening review and a 4.75s rebuild. All 32 final results were byte-for-byte unchanged.
There were 65 benchmark evaluations/65 store constructors across the two full runs and smoke, no
Wasm reopen or production E2E fixture. Canbench ran its own PocketIC runtime; no separate PocketIC
test suite was run. No timeout, thread-count override, sandbox fallback or deferred runtime pass
is claimed.

Review found the old payload repeated every 256 bytes, making swapped equal-length chunks invisible
to exact-byte comparisons. A new assertion inside the existing matrix test requires every 256/4096 B
chunk of a 1 MiB fixture to be distinct. It failed with 1 distinct chunk versus 4096 expected before
the fixture fix. The shared payload generator now embeds successive u32 positions plus a salt;
chunk order is observable and changing the salt still changes every byte. Both tests then passed.
The native 188 I/O rows and 32 canbench results stayed identical, not the fixture-byte hashes.
This is a fixture-quality red-to-green, not a production storage bug or new B-tree constructor.

Lizard 1.23.0 (unexpanded source): 28→32 functions, max CCN 9→9, aggregate CCN 60→68, function NLOC
525→641. Four new helpers own fixture setup, range read, rewrite and point-get control; generated
benchmark wrappers are not counted as expanded functions. No production complexity delta or new
helper with five or more meaningful arguments. The only body-algorithm edits are backend/counter
compilation gates; no storage rule was copied into a second implementation.

Source SHA-256 for the recorded 32-case run, before wider-chunk fixture additions:
`b727c1a8d417fe264ef285e03815f8fafdf5d43988a06db2f0dd7e061ef0c01e`.
Wasm SHA-256: `c529186ec657b20b3e6612b79d95c1a95e6a09e2a37e877e3e982dbdf6cbde71` (743200 B).
Evidence: `/tmp/gleaph-body-canbench-x0i5_yaj/`, including raw page logs, manifest/config/lock,
validation, snapshots and same-assistant review. No independent/security approval, production
layout/activation, staging or commit is implied.

## Primary source support

Installed 0.7.2 source, pinned by its `.cargo_vcs_info.json` to
`72b02ab65b6c6fbf53fbabf752732d2726539377`:

- [BTreeMap constructor and load](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap.rs#L326-L366):
  bounded-max/default page selection. The probe uses `new` and `load`, not magic-resetting `init`.
- [Node V2 save and overflow discovery](https://github.com/dfinity/stable-structures/blob/72b02ab65b6c6fbf53fbabf752732d2726539377/src/btreemap/node/v2.rs#L217-L319):
  all-entry node saving, overflow list loading, and the source of chunk-node write coupling.
- [Prior owner/source investigation](2026-09-13-sidecar-bounded-read-options.md): original sidecar
  problem and the distinction between decode-time checks and pre-read work admission.
