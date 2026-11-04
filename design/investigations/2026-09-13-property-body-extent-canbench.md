# Property bodies: managed IC extent comparison

Status: Experimental — native and managed IC comparison complete; production integration unselected.

Design anchor: 2026-09-13 20:29:14 UTC +0000.
IC verified: 2026-09-13 20:50:20 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## Decision

The native I/O advantage survives IC instruction accounting for the measured mutation paths.
A 1 MiB full replacement costs **3.15M instructions** with extents, versus 7.36M Whole and
26.71–32.31M with chunks. Shrink costs 2.11M versus 5.86M Whole and 29.54–44.75M with chunks.
Small-neighbor updates also improve substantially.

This is not an unconditional win:

- Extent 1 MiB read costs **2.10M**, slower than Whole's ordinary point-get at **1.43M**.
- Allocation from an already-coalesced hole at F128 costs **68.65K**, versus 38.08–38.12K for
  B-tree insertion. The extra allocator/free-index work is material.
- The extent's extra region retains **four more physical pages (256 KiB)** in these small/1 MiB
  fixtures. Including this manager's metadata, the small fixture is ten pages versus six, not a
  free or amortized-away index. This does not select a production region policy.

The result supports a **separately reviewed PropertyStore integration design**, not production
activation. The [native full-store checkpoint](2026-09-13-property-store-native-comparison.md),
verified2026-09-14 01:10:06 UTC +0000, now includes real keys, Value codec, directory and moves.
The [full-store IC comparison](2026-09-17-property-store-managed-comparison.md), verified2026-09-17
22:24:02 UTC +0000, now measures those costs. Direct ordinary get and same-size replacement beat
extents in that full-store fixture; whole-row resource/failure bounds and real update lifecycle remain
prerequisites.
The working body sources changed for that native reuse; the historical91 IC results below remain
measurements of the frozen0357 source revision only. Bulk edge SET is still unavailable.

## Compared owners and memory boundary

The [B-tree source](artifacts/2026-09-13-property-body-btree-probe.rs) and
[extent source](artifacts/2026-09-13-property-body-extent-probe.rs) retain one storage algorithm per
owner. Native uses the existing counted VectorMemory; Wasm uses the existing
[variable memory manager](../../crates/ic-stable-variable-memory-manager/src/lib.rs) over direct
`Ic0StableMemory`, **without native instrumentation wrappers**. This is the manager selected by
Graph unless its standard-manager benchmark feature is enabled; no manager implementation changed.

One fixture constructor owns a four-page bucket policy for every managed region. Whole/C4KiB/
C16KiB/C64KiB use region0 for the body B-tree; extent uses region0 for its arena and region1 for the
free index. B-trees are not charged for a fake second collection, and the extent index is not omitted.
These IDs belong only to the isolated benchmark canister, not Graph's registered layout.

Each query starts with zero raw stable pages and one new manager/body owner. Manager initialization
logs two physical metadata pages; each region then acquires backing in four-page increments.
`region0`/`region1` log requested virtual pages. `before`/`after` and canbench's stable-memory delta
refer to **underlying physical pages**, not virtual growth. For these fixtures, the verified total is
`2 + 4*ceil(region0/4) + 4*ceil(region1/4)`; region1 is zero for B-trees. This follows the measured
manager metadata/policy and monotonic region sizes, not a copied persisted-header parser.

The allocator's [format and rules](2026-09-13-property-body-extent-design.md) are unchanged. Raw alias
checks remain in native tests; the managed fixture owns distinct region IDs by construction before
entering the common pair initializer. This is not a general alias detector. Manager setup is outside
allocator preflight/timing. Caller descriptors and the oracle map remain ephemeral, not a persistent
PropertyStore directory or an upgrade-surviving identity registry.

## Measurement contract

The [complete result artifact](artifacts/2026-09-13-property-body-extent-canbench-results.yml) contains
**91 query benchmarks**:

- 48 existing B-tree control names/operations, remeasured on managed memory;
- eight extent read/rewrite/shrink65B/1MiB and neighboring-body controls;
- 15 hot cases: initial growth and post-warm local-reload growth/shrink for each of five owners;
- 20 fragmented-allocation cases: F8/F128 × no-fit/already-coalesced × five owners.

The prior [48-case raw-backend results](artifacts/2026-09-13-property-body-chunk-size-results.yml)
remain unchanged. They are **not** matched baselines for this run. Backend/translation and compiled
fixture context changed; differences against that artifact are not isolated manager-overhead measurements.

Initialization, payload generation, operation sanity checks, full-state audits, local reload, logging
and returned-vector destruction are outside `bench_fn`. Mutation includes complete previous-value
acquisition and changes every new byte. Position-bearing payloads detect chunk reorder. B-tree
oracles check exact bytes and total entries; extent oracles check exact bytes, live ownership,
partition coverage, free-index equality and no adjacent Free spans. No policy or consistency checks
are disabled to improve a timing result.

Read contracts remain distinct: Whole range/assembly validates a complete body sequence; ordinary
Whole `get` skips that validation/assembly. Extent checks caller length/capacity/tags and reads one
contiguous **virtual** payload; the manager can split that into several physical operations. Native
Memory-call counts are not IC syscall or instruction counts.

The fixture-only B-tree `seed` is used for absent-body fragmented insertion, while extent uses
`change(None, ...)`. Both create the same preverified fresh logical body, but this is not equivalence
of production create/update APIs or an absence-lookup benchmark. Directory, Value codec, indexes,
outbox, authorization, response encoding and Graph transaction costs are excluded.

## Results

All table entries below are IC instructions. C4/C16/C64 use owner caps with the default B-tree node
geometry, not large declared maxima. Whole remains an unbounded control, not an approved bounded API.

### 1 MiB body beside a 65-byte guard

| Operation | Whole | C4KiB | C16KiB | C64KiB | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Read with owner validation | 3535649 | 4533011 | 3778366 | 3588553 | 2099770 |
| Full replacement | 7364196 | 32306956 | 26709405 | 29248603 | 3152944 |
| Shrink to65B | 5855017 | 44749081 | 30820003 | 29537505 | 2108360 |

Extent replacement is 2.34× cheaper than Whole and 8.47–10.25× cheaper than the chunks in this fixture.
Shrink is 2.78× cheaper than Whole and 14.01–21.22× cheaper than the chunks. Shrink still reads the
old1MiB body; the native260-byte write result never implied negligible total execution.

Whole ordinary point-get is **1427392** instructions: extent read is about **1.47× more expensive**.
The paths construct buffers differently (extent reserves/zero-initializes its output, while Whole
uses upstream value extraction), and Whole omits sequence assembly. No phase-level attribution or
optimization claim is made from the total. Do not substitute point-get into the full-replacement
baseline, which deliberately obtains old bytes before any writes.

### Selected65B body beside1MiB

| Operation | Whole | C4KiB | C16KiB | C64KiB | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Read | 180756 | 48807 | 79751 | 138749 | 2225 |
| Full replacement | 2953464 | 225749 | 592365 | 1922760 | 6009 |

Extent's rewrite is about37.57× cheaper than C4KiB here. It does not read/copy the neighbor's payload;
small differences versus the all-small fixture include managed-address/cache placement. A future
small-cell threshold of at least65 would remove this external65B case; its benefit must not be
counted as an observed workload frequency or a production hit rate.

For two65B bodies, Whole read/rewrite/shrink-to32B costs6074/14270/14204 instructions; extent costs
1986/5811/10288. The raw small-body density experiment remains separate; these are two-body IC cases,
not a256-property population benchmark.

### Hot growth and reuse

One body, no guard. Initial growth measures65B→1MiB without a large prewarm. Reuse cases prepare
65B→1MiB→65B, reload the owning collections over their existing managed regions, then measure growth
or prepare a large body and measure shrink. The manager itself is not reconstructed; this is not a
canister upgrade test or measurement of reopening all manager extents.

| Operation | Whole | C4KiB | C16KiB | C64KiB | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial grow | 4294064 | 52772646 | 35344629 | 23289881 | 1313319 |
| Reuse grow | 3795138 | 53294165 | 35254949 | 22809380 | 1062909 |
| Reuse shrink | 5851169 | 48506841 | 33377536 | 29822095 | 2108473 |

All ten reuse cases preserve both virtual and physical capacity. All five initial-grow cases add
16 physical pages. Heap-page increases during initial grow are Whole17, C4KiB0, C16KiB4, C64KiB12,
extent0. Returned buffers are kept alive across measurement and checked afterward. These page
increments are not peak heap/allocation counts; setup has already allocated the new input and
performed the small sanity sequence.

### Fragmentation and free-index cost

Prepare F alternating33B bodies/65B guards, then remove the33B bodies. No-fit measures a fresh65B
allocation; every extent hole is too small despite sufficient aggregate free bytes. Coalesced
measures a fresh160B allocation after deleting the first intervening guard **outside timing**.
This measures consuming already-coalesced space, not the cost of the coalescing deletion itself.
The two phases use independent fixtures; unlike the native row sequence, the IC coalesced case does
not first append the no-fit65B body. All five owners use this same logical history.

| Operation / F | Whole | C4KiB | C16KiB | C64KiB | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| No-fit /8 | 27822 | 27866 | 27866 | 27866 | 11199 |
| No-fit /128 | 37937 | 37981 | 37981 | 37981 | 22617 |
| Already-coalesced /8 | 24340 | 24384 | 24384 | 24384 | 31830 |
| Already-coalesced /128 | 38075 | 38119 | 38119 | 38119 | 68647 |

Extent loses the already-coalesced case: about1.31× atF8 and1.80× atF128 versus Whole. Source review
traces the extra fit/membership checks and removal of a fixed-key free-index entry, potentially
saving/rebalancing index nodes, versus one fixture B-tree insertion. Native evidence also shows
substantial index traffic. This is a measured cost of the selected allocation/consistency mechanism,
not an upstream defect; exact per-phase instruction attribution is not established. Removing those
checks merely to beat the insertion control would weaken the retained rejection contract.

Extent no-fit appends88 logical bytes, ending at1304/18584, retaining8/128 Free spans. Coalesced
allocation keeps logical end1216/18496 and leaves6/126 Free spans; the24-byte remainder is absorbed.
No fragmented case adds a physical page. Logical extension inside reserved capacity is not zero
fragmentation or zero allocation, and these two F points do not bound worst-case fragmentation.

## Physical and virtual capacity

| Fixture | B-tree physical pages | Extent physical pages | Virtual region pages |
| --- | ---: | ---: | --- |
| Small primary / fragmentation | 6 | 10 | B-tree1; extent arena1 + index1 |
| Primary1MiB / large neighbor | 22 | 26 | B-tree17 or18; extent arena17 + index1 |
| After hot warm/reuse | 22 | 26 | B-tree17 or18; extent arena17 + index1 |

A physical page is65536 bytes. The large fixture retains1441792 versus1703936 bytes; the small fixture
393216 versus655360. The measured extra256KiB is one four-page region, not an identity byte per edge.
Manager metadata would be shared with other collections in a real Graph, so multiplying these entire
per-fixture totals by property-store count is not a production footprint estimate.

The C16KiB primary1MiB shrink ends at18 virtual pages instead of17, but remains at22 physical pages
because its20-page body backing already suffices. The earlier raw run's +1 stable-page result is
preserved in its own artifact; it is not contradicted by this managed run's zero physical increment.

Across91 cases: **five stable deltas are+16 pages;86 are zero**. Three heap deltas are positive (the
initial Whole/C16/C64 grows);88 are zero. Zero deltas do not mean no temporary allocation, dirty pages,
manager work or peak heap. Region capacity is not returned to the platform on shrink/free. Extent
relocation can still retain old+new capacity; this IC matrix measures in-place primary/hot operations
and fragmented fresh allocation, while forced relocating replacement remains in native contracts.

## Validation and implementation review

Six native tests pass, with all260 B-tree and36 extent numeric rows unchanged. Native fixture budget
remains9 arenas/36 B-tree creations/30 successful B-tree loads. The complete IC run has91 fresh
manager/body-owner constructions, of which15 are extent pairs and76 are B-tree owners;10 cases
reload local collections outside timing. One additional focused smoke evaluation passed. These
are canbench query evaluations, not a manual PocketIC/federation lifecycle suite.

Wasm Clippy initially exposed a `Result` alias imported by a wildcard into the canbench macro scope.
Explicit fixture imports fixed that name collision; allocator errors were not made Candid values and
no codec, test guard or storage algorithm changed to accommodate it. Native/Wasm Clippy and rustfmt
pass. The smoke built in8.29s; the complete unfiltered run reused that build and persisted all91 cases.
There was no runtime failure, timeout, process-chain fallback or thread override.

Lizard1.23.0 across the two artifacts:68→78 functions, maxCCN22 unchanged, sumCCN253→289,
functionNLOC1602→1967. The additions are one managed constructor and owner-specific fixture helpers;
the prefix generator moved to the existing parent. No strategy trait, runtime owner flag, generic
allocator framework or new production module was introduced. `Arena::commit` still has five
parameters including self; no new five-argument interface. Small mechanical fixture duplication
preserves the distinct prefix-versus-address APIs without duplicating storage rules.

Same-assistant implementation/self-review only, not independent/security approval. The ten-path
slice preserves production Rust, manager implementation, workspace manifests/lock, prior CSV/YAML/
lock/workload artifacts, closed plans and unrelated work. No staging/commit or deployment occurred.

## Reproduction and provenance

This is the canonical reproduction of the **measured0357 managed IC revision**. Extract the
[frozen measured sources](artifacts/2026-09-13-property-body-0357-sources.tar.gz), rather than silently
compiling the later native-only key/preparation changes. Updated raw-native checks can instead point
the lib path at the working artifact; that is not reproduction of these historical IC instructions.
Run from repository root:

```sh
root="$PWD"
probe="$(mktemp -d /tmp/gleaph-body-managed-repro-XXXXXX)"
tar -xzf "$root/design/investigations/artifacts/2026-09-13-property-body-0357-sources.tar.gz" -C "$probe"
python3 - "$probe" <<'PY'
import hashlib, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
for name, digest in json.loads((root / 'sha256.json').read_text()).items():
    assert hashlib.sha256((root / name).read_bytes()).hexdigest() == digest
PY
cat > "$probe/Cargo.toml" <<EOF
[package]
name = "gleaph-property-body-managed-canbench"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
path = "$probe/2026-09-13-property-body-btree-probe.rs"
crate-type = ["cdylib", "rlib"]

[dependencies]
ic-stable-structures = "=0.7.2"

[target.'cfg(target_arch = "wasm32")'.dependencies]
canbench-rs = "=0.7.0"
ic-cdk = "=0.20.1"
candid = "=0.10.35"
ic-stable-variable-memory-manager = { path = "$root/crates/ic-stable-variable-memory-manager" }

[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
[profile.release]
lto = "fat"
codegen-units = 1
EOF
cp design/investigations/artifacts/2026-09-13-property-body-extent-canbench.Cargo.lock "$probe/Cargo.lock"
cat > "$probe/canbench.yml" <<EOF
build_cmd: "sh -c 'cd $root && cargo build --offline --locked --manifest-path $probe/Cargo.toml --target-dir $root/target/canbench --release --target wasm32-unknown-unknown'"
wasm_path: "$root/target/canbench/wasm32-unknown-unknown/release/gleaph_property_body_managed_canbench.wasm"
EOF
cargo test --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$root/target" --lib -- --nocapture
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$root/target" --lib --tests -- -D warnings
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$root/target/canbench" --target wasm32-unknown-unknown --release --lib -- -D warnings
rustfmt --edition 2024 --check "$probe"/2026-09-13-property-body-{btree,extent}-probe.rs
(cd "$probe" && canbench --persist --csv --show-canister-output --runtime-path "$root/crates/ic-stable-lara/.canbench/pocket-ic")
```

Dependencies are exact stable-structures0.7.2/canbench-rs0.7.0/ic-cdk0.20.1/candid0.10.35 plus the
unchanged local manager0.1.0. All registry package versions/checksums match the previous IC lock;
only the root package and existing manager entry differ. The retained lock pins this experiment.
Compiler:1.100.0-nightly(c656540d6,2026-08-21), aarch64-apple-darwin; repository SIMD flags and release
fatLTO/codegen-units1 retained. Canbench's fixed2021 canister clock is not the verification timestamp.

SHA256 of the measured0357 revision (the two sources are archive members, not the updated working files):

- B-tree source: `4cc5f221afedcb1ae298836fb3967dcf3d4f0144632640624e3fe599c7fe31da`.
- Extent source: `8baa6bcedf31fdf2287e8d7e3656e4915011fe9da4de685bfb0b24c7b2eb3d30`.
- Manager source: `2d5d29ddada0d4c5a336f89207d0483178e80e7873d58b00ccd70b05ea5acbac`.
- Measured Wasm: `5e6650ce23e9f0f82a6ff56804d5f5fe6ca2a0e8169079af66010b7670d14f64` (1323548 bytes).

Evidence directory: `/tmp/gleaph-extent-ic-qp2ogkcj/`, including draft plan, initial snapshots,
native/lint/smoke/full-canbench logs, CSV/YAML, manifest/lock/config, source/fixture/numeric checks,
Lizard and final review/plan records. Full raw source is measured directly, not a rewritten allocator.

## Remaining gates

Before production, select the directory cell/threshold/version and reference-transfer semantics
from the unselected experiments, retain Value/NULL/previous-value behavior, prove interacting whole-row allocations and local
edge-slot remaps, admit complete tree/allocator/decoder/outbox work and retain deterministic outcomes.
Validate real Graph updates, rollback and same-format upgrade with the owning lifecycle fixture.
Choose coherent fresh-state activation in an ADR/inventory update, not a compatibility shim.

Runtime operation frequencies remain unavailable. The demo's143-byte Value maximum is neither a
product cap nor evidence that the large-body cases can be ignored. The measured instruction gains,
point-get loss, fragmented-allocation loss and extra region all remain inputs to that later decision.
