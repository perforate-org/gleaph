# Managed IC PropertyStore comparison

Status: Experimental — full-store IC comparison completed; production layout unselected.
Anchor:2026-09-17 22:03:10 UTC +0000. Verified:2026-09-17 22:24:02 UTC +0000.
Repository:95d7910878a5ee8af95af878f2b0df952edd70dc, including the preserved uncommitted prerequisites.

## Result

The custom extent owner is **not an unconditional improvement over the existing PropertyStore**.
Including real keys, Value validation/codec, the directory and returned values changes the conclusion
one might draw from body-only replacement measurements:

- Direct is cheaper for scalar-dominated operations, ordinary1MiB get and same-size replacement.
- All separated candidates isolate directory-small values from a large neighboring body.
- Extents improve large growth/relocation and reference-transfer moves, and avoid chunk rewrite costs.
- Extent fragmented allocation, extra independently reserved regions and relocation high-water remain
  material costs. Chunks are not an attractive large-mutation substitute in these fixtures.

No workload-weighted winner or production adoption is selected. These are IC instructions, not
wall-clock latency, throughput, peak heap or a whole-Graph transaction cost.

Scalar cost attribution (record not retained) identifies the additional directory
lookup as the dominant small-value cost. It preserves these results and storage algorithms; its final
current-worktree native rerun is blocked by concurrent LARA changes.

Artifacts: 210 full-store rows (measurement dump, not retained),
complete301-case canbench results (measurement dump, not retained),
exact lock (measurement dump, not retained).

## Boundary and measurement

The [native contract](2026-09-13-property-store-comparison-design.md) remains unchanged: actual8-byte
vertex/14-byte edge keys; Graph StoredPropertyValue V1 and IC extension decoding; experimental S≤65
small cells, external S≤1MiB,2MiB selected-byte allowance and complete enumeration≤256. S includes
the envelope. Direct remains the unbounded-admission reference, not a candidate with hidden length
metadata or extra preflight lookups.

The isolated harness (measurement dump, not retained) source-includes the actual
Graph key/store/validator files. It does not compile the Graph canister or expose its private APIs.
The [shared implementation](../../crates/graph/src/facade/stable/property_store_probe/mod.rs) uses the same algorithms as
native tests, with the existing body-memory alias selecting counted VectorMemory or VirtualMemory
on Ic0StableMemory. Native alias rejection remains; the private Wasm constructor supplies distinct
IDs itself. There is no arbitrary supplied-Wasm-memory alias-detection API.

Each query begins with one fresh variable memory manager, two metadata pages and four-page region
policies. Direct owns region0; chunks own directory0/body1; extent owns directory0/arena1/free-index2.
All are included in actual physical backing. No region equalizes costs artificially, and no Graph
production MemoryId or64-page property policy is changed.

The [fixtures](../../crates/graph/src/facade/stable/property_store_probe/benches.rs) include21 cases per owner/key.
They verify exact pre/post Values via canonical bytes, all directory/body ownership, guards, extent
partition/free index, and correct relocation addresses. NULL, NaN, composite, Unicode and Principal
values appear in the mixed-scalar cases. Small-only sanity does not prewarm external bodies.

Timing includes validation/encoding, actual get/set/remove/enumerate/rekey and returned Value decode.
Generation, expected-map operations, audits, printing and returned-value destruction are outside.
Batch rows include sequential calls/collection and consumption of prepared inputs; they are not
atomic batches or per-call averages. Audit reads can warm metadata caches: **cold means backing not
previously grown for a large body**, not a CPU/cache-cold or zero-heap-history claim.

Canbench mutates query-local state. These runs do not prove durable updates, reply-loss recovery,
upgrade, manager reconstruction or rollback. Reopen cases reload local collections using the existing
manager. First-effect/preflight/publication/retirement semantics are unchanged; unexpected commit
failure traps, not a recoverable post-write Err. Individual operations still do not prove whole-row
allocation or multi-property moves. Edge moves keep owner/label/property and change slot; vertex
moves are synthetic key-width controls. Direct rekey is the documented get/remove/set adapter,
not production `move_all_for_edge`.

## Instructions

Exact counts below use edge keys. Vertex results are separately retained in the CSV and have the
same qualitative trade-offs. C4/C16/C64 denote4/16/64KiB chunk caps with default B-tree geometry.

| Operation | Direct | C4 | C16 | C64 | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Mixed scalar insert256 | 11190674 | 15372372 | 15372372 | 15372372 | 15363983 |
| Mixed scalar get256 | 4020518 | 4256850 | 4256850 | 4256850 | 4271679 |
| Mixed scalar replace256 | 7293332 | 11701532 | 11701532 | 11701532 | 11668594 |
| Mixed scalar remove128 | 8446852 | 9985271 | 9985271 | 9985271 | 9992458 |
| Enumerate51 properties | 152368 | 240726 | 240726 | 240726 | 243292 |
| Ordinary get1MiB | 5361270 | 8539336 | 7722782 | 7529842 | 6036776 |
| Replace1MiB | 9853882 | 40303690 | 33583797 | 34359618 | 10249579 |
| Shrink1MiB→65 | 6247351 | 58523999 | 41381721 | 36505629 | 6059975 |
| Get65 beside1MiB | 179028 | 6467 | 6467 | 6467 | 6523 |
| Replace65 beside1MiB | 2776575 | 19521 | 19521 | 19521 | 20398 |
| Get66 beside1MiB | 179046 | 53840 | 84828 | 143707 | 7283 |
| Replace66 beside1MiB | 2776597 | 253631 | 620841 | 1951597 | 25106 |
| Cold65→1MiB growth | 6389060 | 59189345 | 39227186 | 26579424 | 4474508 |
| Warm65→1MiB growth | 5892042 | 60170007 | 38882484 | 26117668 | 4226571 |
| Warm1MiB→65 shrink | 6249166 | 59352236 | 41085650 | 36477754 | 6057049 |
| F128 no-fit insert97 | 42083 | 110169 | 110169 | 110169 | 68717 |
| F128 coalesced insert257 | 46807 | 122633 | 122633 | 122633 | 118356 |
| Move65 over97 | 29335 | 45924 | 45924 | 45924 | 47094 |
| Move1MiB over1MiB | 26553834 | 102990892 | 76864908 | 66687460 | 12109508 |
| Forced1MiB relocation | 9220849 | 61499104 | 42514458 | 31450555 | 4489637 |
| Remove relocated1MiB | 9068078 | 64935653 | 50688730 | 42326493 | 6064298 |

For example, extent scalar insert/replace costs about37%/60% more than Direct; ordinary1MiB get
costs about13% more and same-size replacement4% more. Its large move costs about54% fewer
instructions, while coalesced allocation costs about153% more. Separating small cells helps every
candidate, not extents alone. The body's raw-byte replacement advantage is not the same as improving
the actual Direct setter: Direct does not add the raw Whole control's separate validated-old read.

### Fixture histories

- Density uses256 values across five scopes51/51/51/51/52, with independent edge slot/label/owner
  foils and maximum IDs. Enumeration selects the first51. Replacement uses a shifted scalar corpus.
- Point cases have two properties: selected plus a65-byte or1MiB guard. S65 and S66 deliberately
  straddle the one fixed directory threshold, without treating it as a selected production policy.
- Warm cases grow/shrink, reopen locally, then grow or grow+shrink. Same histories per owner.
- Fragmentation starts with128 S66 bodies interleaved with128 S97 guards. Removing the former leaves
  128 holes of88 bytes; extent end26688/live15360/free11264. No-fit S97 appends120 bytes. The coalesced
  case performs that insertion first, deletes the first guard outside timing, then puts S257 into
  the296-byte span (16-byte absorbed remainder), leaving126 holes/free11088/end26808.
- Moves start independently with source, distinct displaced target and one65-byte guard. They are
  not the native report's heavily warmed move history. Extent large moves preserve the source address
  but still read/decode both source and displaced Values before committing.
- Forced relocation starts with S66 source, S97 blocker and a1MiB guard. The extent source moves to
  the prior end1048864; logical end becomes2097456. Deletion follows that same history but leaves
  blocker/guard alive. Neither deletion nor release shrinks stable backing.

## Retained capacity, not just payload

These are actual64KiB pages, including manager metadata and every region. Both key widths give the
following page counts. Brackets show requested virtual pages after the operation.

| State/operation | Direct | Chunks | Extent |
| --- | --- | --- | --- |
| Small-only density | 6 [1] | 10 [1:1] | 14 [1:1:1] |
|1MiB point fixture | 22 [17] | 26 [1:17 or18] | 30 [1:17:1] |
| Cold growth | 6→22 | 10→26 | 14→30 |
| Warm growth/shrink | 22→22 | 26→26 | 30→30 |
| Large move | 38→38 [33] | C4/C16:42→42 [1:35]; C64:42→46 [1:40] | 46→46 [1:33:1] |
| Forced relocation | 22→38 [33] | C4/C16:26→42; C64:30→42 [1:34] | 30→46 [1:33:1] |
| Remove relocated body | 38→38 | C4/C16:42→42; C64:42→46 [1:40] | 46→46 |

The extent's small/point fixture reserves eight more physical pages (512KiB) than Direct: two
additional four-page regions, directory/body/index versus one direct tree. Against a chunk candidate
it has one extra four-page region. This is owner-specific capacity, not512KiB of payload, per-edge
identity or automatically wasted bytes. The index's capacity cannot hold arena payload. Future reuse
is possible, not guaranteed; release cannot shrink physical stable memory.

C64 large move and final deletion each grow four physical pages despite reducing logical properties.
Some virtual growth remains inside existing reservations: e.g. C16 warm body19 pages still occupies
20 physical body pages. Warm pairs retain backing in this measured history, not in all workloads.

Of210 full-store cases,17 have nonzero Wasm heap growth: all ten cold grows add17 pages, both C64 warm
grows add17, and the five vertex scalar-replacement batches add1. The other193 zero deltas do not
mean zero temporary allocation or a bounded peak. Twenty-four rows grow physical stable memory.
Logical map bytes, external payload, live/free spans and body key bytes accompany every CSV row;
opaque B-tree internal overhead is represented in backing, not guessed from serialized KV bytes.

## Validation and review

- Isolated native10 tests passed (27.95s build/3.24s execution); Graph native10 passed (40.71s/3.43s).
  Every2640 full-store row and260+36 raw row matches the preserved native artifacts exactly.
- Native isolated Clippy1.40s, Graph scoped Clippy16.18s and final Wasm Clippy1.65s passed; all four
  affected Rust paths pass scoped rustfmt. Whole-workspace formatting was not rerun:22 untouched
  failures were established by0358, and no full-workspace pass is claimed.
- Initial isolated resolution rejected an unnecessary `ethnum=2` dev dependency; actual included
  sources needed neither added dev dependency, so both were removed before compilation.
- Wasm lint caught one missing output format placeholder and an unused import; both were corrected.
- First smoke built in61s but installation rejected exported-name total24747>20000. Only new export
  names were shortened (16627 bytes); no fixture was removed or assertion weakened. Corrected smoke
  built in27.17s and passed `ps_e_x_r1m` at6036776 instructions. This failed installation ran no fixture.
- The unfiltered run reused the build (0.34s), passed all301 cases and persisted the complete result.
  No timeout, process-chain fallback, thread override or shared target cleanup occurred.
- One complete run creates210 full stores (378 B-trees/42 arenas), plus91 existing body controls
  (76 B-tree body owners and15 free-index B-trees/15 arenas): **469 trees/57 arenas** total. Thirty
  cases reopen locally (20 full-store=36 tree loads;10 raw=10 tree loads). The successful smoke adds
  one manager, two trees and one arena. Manual PocketIC constructors/federation installs are zero;
  these counts do not pretend canbench has no internal PocketIC canister installation.
- The two native runs each retain40 full stores/108 total tree creations/17 arenas,102 successful
  tree loads plus48 rejected metadata loads. Compile/lint are not additional fixture executions.
- Original workspace lock was restored after saving the known generated reconciliation:144 identities
  pruned, none added, retained checksums unchanged. The isolated experiment has its own retained lock.
- After the verification anchor, concurrent LARA benchmark changes appeared in
  `crates/ic-stable-lara/src/labeled/bench.rs` and `crates/ic-stable-lara/src/lara/edge/insert.rs`.
  They remain untouched and are separately recorded, not included in this validation claim. The
  harness uses VertexId, not adjacency insertion, and does not enable LARA's canbench feature.
  The identical generated workspace lock also reappeared later and was recognized/restored; its
  writer was not attributed. The final preservation check covers the recorded concurrent paths.

Lizard1.24.0: B-tree source47 functions/CCNsum111/max9/NLOC911 unchanged. New case implementation
13 detected functions/CCNsum50/max13/NLOC346, maximum arity4;210 thin macro exports add no owning
algorithm. Shared source raw detection112→101/CCNsum265→227/max16 unchanged is **not** a complexity
reduction: nesting the native fixture makes Lizard miss methods. The source adds one managed-memory
factory, shared fixture reuse and cfg boundaries; all mutation algorithms remain unchanged. No new
runtime strategy registry, storage trait, tuning flag, unsafe buffer optimization or library fork.

Same-assistant architecture/test/code-quality review only, not independent/security approval. A timed
no-op setter would fail final byte/ownership audit even if it returned the correct old Value; stale
relocation publication would fail address and ownership assertions; missing directory accounting
would fail all-region page equations. No unresolved blocker within this experimental contract.

## Reproduction and provenance

The working prototype now includes the prepared-change experiment (record not retained).
This report's301-case result and hashes remain pinned to0359. The reproduction below extracts its
eight original source inputs (measurement dump, not retained); the current
source harness instead exports313 cases and must not be mistaken for this historical measurement.

0358 frozen sources (measurement dump, not retained) preserve the pre-sharing
native revision and its hash manifest; they are not an active fallback. The0357 archive/results remain
unchanged. The91 body-only controls in this new artifact use the updated source/compiler/codegen;
they do not overwrite or reclassify historical0357 results. No instruction deltas against those old
binaries are attributed solely to allocator behavior.

Compiler:1.100.0-nightly(574ff7d98,2026-09-14), aarch64-apple-darwin. Canbench/canbench-rs0.7.0,
stable-structures0.7.2, ic-cdk0.20.1, candid0.10.35; local manager unchanged. FatLTO/codegen-units1,
repository SIMD flags. The canbench2021 clock is not the verification date. Run from repository root:

```sh
root="$PWD"
p="$(mktemp -d $TMPDIR/gleaph-property-store-repro-XXXXXX)"
mkdir "$p/source"
tar -xzf design/investigations/artifacts/2026-09-18-property-store-0359-sources.tar.gz -C "$p/source"
python3 - "$p/source" <<'PY'
import hashlib, json, pathlib, sys
r = pathlib.Path(sys.argv[1])
for name, expected in json.loads((r / 'sha256.json').read_text()).items():
    assert hashlib.sha256((r / name).read_bytes()).hexdigest() == expected, name
PY
cat > "$p/Cargo.toml" <<EOF
[package]
name = "gleaph-property-store-managed-canbench"
version = "0.0.0"
edition = "2024"
publish = false
[lib]
path = "$p/source/design/investigations/artifacts/2026-09-17-property-store-canbench.rs"
crate-type = ["cdylib", "rlib"]
[dependencies]
ic-stable-structures = "=0.7.2"
ic-stable-lara = { path = "$root/crates/ic-stable-lara" }
gleaph-gql = { path = "$root/crates/gql", features = ["f128", "f256", "cypher"] }
gleaph-gql-ic = { path = "$root/crates/gql-ic" }
gleaph-graph-kernel = { path = "$root/crates/graph-kernel" }
candid = "=0.10.35"
[target.'cfg(target_arch = "wasm32")'.dependencies]
canbench-rs = "=0.7.0"
ic-cdk = "=0.20.1"
ic-stable-variable-memory-manager = { path = "$root/crates/ic-stable-variable-memory-manager" }
[profile.dev]
debug = "line-tables-only"
[profile.test]
debug = "line-tables-only"
[profile.release]
lto = "fat"
codegen-units = 1
EOF
cp design/investigations/artifacts/2026-09-17-property-store-canbench.Cargo.lock "$p/Cargo.lock"
cat > "$p/canbench.yml" <<EOF
build_cmd: "sh -c 'cd $root && cargo build --offline --locked --manifest-path $p/Cargo.toml --target-dir $root/target/canbench --release --target wasm32-unknown-unknown'"
wasm_path: "$root/target/canbench/wasm32-unknown-unknown/release/gleaph_property_store_managed_canbench.wasm"
EOF
cargo test --offline --locked --manifest-path "$p/Cargo.toml" --target-dir "$root/target" --lib property_store_probe -- --nocapture
cargo clippy --offline --locked --manifest-path "$p/Cargo.toml" --target-dir "$root/target" --lib --tests -- -D warnings
cargo clippy --offline --locked --manifest-path "$p/Cargo.toml" --target-dir "$root/target/canbench" --target wasm32-unknown-unknown --release --lib -- -D warnings
(cd "$p" && canbench --persist --csv --show-canister-output --runtime-path "$root/crates/ic-stable-lara/.canbench/pocket-ic")
```

Export prefixes `ps_v`/`ps_e` select vertex/edge; d/c4/c16/c64/x select owner. Case labels in the
210-row CSV are descriptive and linked to each exported name. `PROPERTY_IC` log fields follow the
CSV header after `name`; join each to its following `Benchmark:` line and verify metrics against
canbench's CSV/YAML. The raw91 controls remain in the complete YAML, not the enriched full-store CSV.

SHA256:

| Artifact | Hash |
| --- | --- |
| Shared full-store source | `fd5843b76a0efaea8c2533164a0d6cd94a63eceeda35337cf4909bc4109d8b04` |
| Shared B-tree source | `aaeee89c964cd8fbc329a85e25da445ea91b6f49308fb3ead8da49b99a6dbb32` |
| Unchanged extent source | `6043e4ac37bedf7bf8f8642f985637b1db1cbfddf8171975aedd63a4d3a74d3d` |
| Harness root | `b354124642db1b2aa5362a4219d45836da158212c146558947f780d079d789a4` |
| IC cases | `7b60e85e04d11efe5bbb7ec4cf783af6a2ca6af0a10932f9c8ec114e6185fcca` |
| Enriched CSV | `fd67fd2655d6c4c94b91be4e0b440d9143ebd8c335e92d88a71200e728f51a7f` |
|301-case YAML | `e5dd52eedb3d62c02fc8c37dbe3c568eba378b944e1490a7928733c92824d31c` |
| Isolated lock | `1b8648f83b24a8d55d4dd2b86ff61f0ee901d64ef34af39d04220f6f4c306ed5` |
|0358 source archive | `4a2edd8afbff9757603c4b110836f3aa8dcf95c4910f6a24d7bf8386054dfb20` |
| Measured Wasm (16089677 bytes) | `25275dc60d99177bf9e7ea0820810c661acc4b9dadca604fa72c991e4a7c0a8b` |

Evidence: `$TMPDIR/gleaph-property-store-ic-irkcps5c/`: snapshots, source/lock diffs, complete logs,
manifest/config/lock, checked extraction, Lizard, review and plan validation. No staging/commit.

## Remaining decisions

Production still needs workload frequencies, complete decoder/tree/allocator/outbox work admission,
interacting whole-row allocation/move plans, durable positive/zero/rejected outcomes, real update
rollback/upgrade validation, and a coherent fresh-state ADR/layout decision. Fine one-page-index,
zero-fill and selected-key optimizations remain deferred. Bulk edge SET is still unavailable.
