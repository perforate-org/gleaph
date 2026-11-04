# Native PropertyStore comparison

Status: Experimental — native checkpoint completed; production unselected.
Follow-up verified2026-09-17 22:24:02 UTC +0000: [managed full-store IC comparison](2026-09-17-property-store-managed-comparison.md).
Native numbers and historical source hashes below remain the0358 measurements; the
frozen0358 sources (measurement dump, not retained) retain that revision.
The shared working source now also supports managed IC, with all native numeric rows reproduced.
Investigation started2026-09-13. Native verification: **2026-09-14 01:10:06 UTC +0000**.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## Result

The [comparison contract](2026-09-13-property-store-comparison-design.md) is implemented in a
[Graph-owned prototype](../../crates/graph/src/facade/stable/property_store_probe/mod.rs), included as a native test-only Graph child. It compares actual
VertexPropertyStore/EdgePropertyStore operations against directory+B-tree4/16/64KiB and
directory+custom extents. The question is **body storage through the existing B-tree versus custom
contiguous extents**, not alternative free-index algorithms.

The results retain costs on both sides:

- Separation protects small directory-resident values from large neighbors, for all four candidates.
- Small-only mutations require more directory reads than the direct stores, and empty external lanes
  retain additional backing. Small-only does not mean a free directory abstraction.
- Chunk replacement/shrink/movement still rewrites substantially more bytes than extents.
- Extent moves transfer the actual reference, but acquire source and displaced Values before writes.
- Extent relocation retains holes and can nearly double arena backing in the blocked-growth history.
- Library B-tree shrinking/deletion can itself grow backing. No library fork or speculative allocator
  optimization was introduced to hide this result.

These are **native Memory calls/bytes**, not throughput, latency, IC instructions or peak heap.
The earlier managed body-only ordinary point-get result still favors Whole over extents; it is not
superseded by this different native experiment. No traffic-weighted winner is selected.

## Contract and measured scope

All five configurations use real8-byte vertex/14-byte edge keys and Graph's owning StoredPropertyValue
codec, including its V1 envelope and IC extension decoder. **S** is the complete stored byte length.
Candidates use S≤65 in their bounded directory, external S≤1MiB, a2MiB selected-byte allowance and
complete enumeration with a256-property limit. These are experimental limits, not product/work caps.
The direct reference retains its actual ordinary get/set/remove paths, without a directory or extra
preflight lookup. It **cannot** enforce candidate pre-body length admission.

Four fixture families create40 full stores: five configurations × two key types × four families.
Each has one successful reopen. They cover:

1.256 small values across five scopes (51/51/51/51/52), including independent edge slot-, label- and
   owner-only foils, maximum IDs, reserved metadata exclusion, NULL/absence, Unicode, a bounded
   composite, Principal and NaN byte-roundtrips; replacement, alternating removal and refill.
2. Cold65→1MiB growth, full replacement,65/66-byte neighbors,64KiB operations, reopen/four grow-shrink
   pairs, then a short source/live blocker forcing relocation and final removal.
3. Cold F128 fragmentation, no-fit/coalesced allocation, then warmed missing/self and small/external
   source/destination moves, overwritten targets, source recreation, destination deletion and reuse.
4. Exact/one-short allowances, reserved IDs, unencodable extension, oversized input, malformed cells,
   missing/extra/wrong-width chunks, invalid extents, alias/partial memories and six metadata failures.
   Invalid source **and displaced-target** Value envelopes trap before effects. Cross-property moves,
   and edge cross-label/owner moves, fail before effects.

Every measured operation is checked against exact canonical bytes and complete ownership afterward.
B-tree body prefixes/count/sequence, extent partition/tags/free index, and directory reachability are
checked by fixture-only full audits, not scans used for allocation or reopening. Source and target
with an aliased extent reference reject before effects. Same-key/missing-source moves preserve all
memory bytes with zero writes/grows; external moves retain the source offset/length at the target.

There is no production Graph singleton, registered MemoryId, Router/index/outbox/journal operation,
public interface or GQL change. The hook is `cfg(all(test, not(target_arch = "wasm32")))`.
The reference rekey adapter composes existing get/remove/set; it is not an existing production
single-property move API or a claim about whole-edge/multi-property transactions.

## Reading the artifact

CSV (measurement dump, not retained): **2640 region rows /1320 operation samples**.
`kind`0=Direct,1=chunks,2=extent; `cap` is the chunk cap or0. `occurrence` is the per-configuration/case
ordinal, not traffic frequency. Rows are sorted by key width/kind/cap/case/occurrence/region.

- Region0: direct property tree or candidate directory; region1: chunk tree or extent arena;
  region2: extent free-index B-tree. IDs are fixture positions, not Graph MemoryIds.
- Calls/bytes/grows cover the owning Memory interface. `grows` counts calls, **not pages**.
  Before/after bytes are Memory::size×65536, not the native Vec's heap capacity.
- `stored_bytes` sums canonical property S; `map_entry_bytes` includes actual serialized keys and
  values in the direct tree/directory, including the candidate metadata row.
- `external_bodies`/`external_payload_bytes` describe directory-selected external bodies.
  `body_entries` counts the direct tree, chunk tree or extent free-index entries respectively;
  `body_key_bytes` charges chunk/free keys separately (direct keys are already in map_entry_bytes).
- Extent `logical_end = 64 + live_span_bytes + free_span_bytes`; spans include tags/alignment/slack.
  Free spans are reusable within the arena, not deleted platform memory. Free-index entries equal
  free-span count. Do not add external payload again to live-span bytes.
- All regions are charged in `total_backing_bytes`. B-tree internal headers/nodes/allocator metadata
  and slack are included there, but are **not separately decoded or claimed as exact used bytes**.
- `body_buffer_bytes` counts the existing owners' explicit body assembly/previous-value buffers,
  not all Value/codec/directory/B-tree allocations or peak heap.
- `projected_manager_pages = 2 + Σ 4*ceil(region_pages/4)` is an isolated four-page-manager
  **projection**, not a native physical-manager measurement. Shared Graph metadata must not be
  multiplied per store; production64-page property policies are not substituted here.

Input generation, oracle encoding, small-only sanity, audits, reporting, reopen and returned-value
destruction are outside measurement. Actual validation/encoding, storage operations and returned
Value decoding are inside. The small sanity never primes an external body. Payload text is
position-bearing and changed on replacement; immutable type/length/version bytes need not change.
Batch rows measure sequential independent property operations, not an atomic batch.

## Small-property cost and retained floors

All256 properties are directory-small; all four candidates have identical directory behavior and
zero external-lane I/O. The byte totals below include the full operation, not an inserted preflight
on Direct.

| Keys / operation | Direct read bytes | Separated read bytes | Direct write bytes | Separated write bytes |
| --- | ---: | ---: | ---: | ---: |
| Vertex / insert256 mixed scalars |124473|212030|89296|91193|
| Edge / insert256 mixed scalars |155565|269954|107044|109001|
| Vertex / get256 |86611|87059|0|0|
| Edge / get256 |110077|110621|0|0|
| Vertex / replace256 with S65 |127076|216226|81041|83171|
| Edge / replace256 with S65 |150542|263350|89057|91283|

Candidate mutation reads include the selected cell before the library mutation. Direct's actual
insert/remove already return the old Value; no duplicate lookup is imposed on it. The extra native
reads do not establish a corresponding CPU/latency ratio, and removing them is not part of this slice.

After all256 S65 replacements, direct versus candidate serialized map entries are18688/18961 B
for vertex and20224/20503 B for edge. The candidate adds one byte per small cell plus the metadata
key/value (17 or23 B), **not66-byte padding per entry**.

| All-small store | Raw backing | Four-page isolated projection |
| --- | ---: | ---: |
| Direct |65536 B|6 pages /393216 B|
| Directory+chunks |131072 B|10 pages /655360 B|
| Directory+extent/index |196608 B|14 pages /917504 B|

The extra regions are eagerly initialized under the paired-state contract. Their reservation can
serve their own later body/index growth, not another region. This is a per-store fixture floor,
not a per-property charge. Lazy lanes and one-page policies remain unselected optimizations.

## Large values and neighbor isolation

The table uses14-byte edge keys; the artifact also contains every8-byte counterpart. Numbers are
**read bytes / write bytes**, aggregated over all required regions. Reads return the actual Value.

| Operation | Direct | Chunk4KiB | Chunk16KiB | Chunk64KiB | Extent |
| --- | ---: | ---: | ---: | ---: | ---: |
| Read S1MiB |1060012 /0|1066688 /0|1061589 /0|1060437 /0|1048738 /0|
| Replace S1MiB |1060077 /1048700|6811240 /5509709|7894269 /6663185|9422144 /8260849|1048895 /1048757|
| Shrink1MiB→65 |1076638 /66430|11784108 /11653529|10768283 /10882682|10116544 /11066475|1048874 /360|
| Read small65 beside1MiB |11501 /0|135 /0|135 /0|135 /0|135 /0|
| Replace small65 beside1MiB |1060077 /1048700|284 /157|284 /157|284 /157|292 /165|
| Read external66 beside1MiB |11502 /0|1481 /0|3851 /0|8145 /0|228 /0|
| Replace external66 beside1MiB |1060078 /1048701|44062 /41387|171682 /164267|671790 /655787|332 /194|

Small65 is protected by the **shared directory**, not uniquely by extents. Extent cells are8 bytes
larger than chunk references, which also affects directory mutations beside an external neighbor.
External66 exposes body-owner coupling again. Wider chunks do not remove that coupling.

Full1MiB reads use2077 Memory read calls on Direct versus13 on extents; replacement uses2078 versus24.
Do not turn these call counts into an instruction-speed claim. Full replacement writes are almost
the same byte volume on Direct and extents; the earlier body-only validated Whole loop is not the
actual Direct setter and must not replace this baseline.

The first shrink grows both key-width Chunk16KiB body trees from17 to19 raw pages; four subsequent
same-shape grow/shrink pairs retain backing. Shrink still acquires the old1MiB Value, including on
extents. A selected-byte allowance is not a bound on the much larger chunk-tree I/O.

## Fragmentation, movement and relocation

F128 starts with128 S66 spans of88 B alternating with S97 guards of120 B. Freed holes total11264 B,
but none fits another120-byte span: exact extent end starts26688, then becomes26808 after the
no-fit append. Removing the first guard coalesces296 B;
S257 requires280 and absorbs16 B. The coalesced allocation retains end26808 and126 free spans.
The guard removal is outside the following allocation measurements (and is separately reported).

| Edge-key operation | Direct R/W B | Chunk4/16/64KiB R/W B | Extent R/W B |
| --- | ---: | ---: | ---: |
| F128 no-fit insertion |1205 /1169|2255 /1394|1372 /358|
| F128 already-coalesced insertion |1315 /1361|2443 /1613|2391 /752|
| Move S66 over S97 |647 /441|1284 /757|1005 /554|

The extent coalesced case makes229 reads versus55 on Direct and169 on chunks. Its fewer written
bytes do not establish lower IC cost. Bodies/free-index/directory are all included; maintenance and
reference bookkeeping can lose on small cases.

For a warmed1MiB source over a different1MiB target, with an unrelated66-byte guard:

| Owner | Read bytes | Write bytes | Retained raw backing |
| --- | ---: | ---: | ---: |
| Direct adapter |4268110|2163670|2162688|
| Chunk4KiB |19291672|17672784|2359296|
| Chunk16KiB |19716810|18494055|2359296|
| Chunk64KiB |18806928|17603794|2686976|
| Extent |2097822|462|2293760|

Extent preserves the source address at the destination and retires the displaced body once; it
still reads both complete Values. Chunk movement rewrites destination-prefix payload, then removes
source ownership. The direct adapter's three existing operations are visible, not an optimized
production move. All guards, missing/self cases, source recreation and later deletion are audited.

The separate forced-relocation history deliberately puts an S97 live blocker after a reused S66
source. The new1MiB extent starts at the prior end1048744, not the old source offset; final end is
2097336. Arena backing rises17→33 pages, while directory/index each retain one page: total2293760 B.
The old hole remains reusable; this is real fragmentation/high-water cost, not a leak hidden by the
oracle. Final removal leaves one free span of2097272 B and does not shrink backing.

Chunk64KiB instead grows17→19 body pages during that replacement, then **19→23 on final deletion**,
for both key widths. The initial test incorrectly required no growth on every deletion and failed
with `[1,23] != [1,19]`. It was corrected to the actual contract: no shrink, no remaining properties
or body ownership; exact no-growth for the extent retirement. B-tree merge/save allocation is a
possible mechanism, but internal phase attribution was not measured. This adverse result remains.

## Validation and source review

Final native logs are under `$TMPDIR/gleaph-property-store-native-p9dk3h0c/` (pointer
`$TMPDIR/gleaph-property-store-native-current`). The pre-edit66-file snapshot, supplementary unchanged-
HEAD test snapshot, and preimplementation test map precede their respective edits. Review later
added owner-only scope foils, both-side move preflight, relocation/reference assertions and explicit
external payload accounting; they are not falsely described as original red tests.

| Gate | Terminal result |
| --- | --- |
| Changed-file rustfmt |Pass, five Rust paths|
| `cargo check -p gleaph-graph --tests` |Pass,3.52s|
| `cargo clippy -p gleaph-graph --lib --tests --no-deps -- -D warnings` |Pass,2.91s|
| `cargo test -p gleaph-graph --lib property_store_probe -- --nocapture` |10 passed,6.82s build +3.29s runtime|
| Standalone shared raw probe |6 passed,0.65s build +3.06s runtime|
| Standalone raw Clippy |Pass,0.35s|
| Existing facade test after redundant attribute removal |1 passed,0.67s build +0.08s runtime|
| `cargo fmt --all -- --check` |Failed on22 untouched paths; not a pass|

The workspace-format failure affects account/CLI/ingress-client/operator/PocketIC/router-wire/SDK
files; the full path list and output are retained in `fmt-unrelated-paths.txt` and
`fmt-workspace-blocked.log`. No broad formatting was applied. The only extra tracked Rust change
outside the hook removes a duplicated `#[test]`; the test function/fixture/assertions are unchanged.

A deliberate temporary implementation mutant published the old extent offset after relocation.
The large/reuse test failed with `Extent(Span)` at the directory/body audit. The mutant was restored;
final tests use correct publication. Typed error checks snapshot every supplied/owned memory and
actual directory rows with zero writes/grows; decoder-trap checks verify the specific owner error
and unchanged memory. None is a native rollback claim.

Per final suite:40 full stores =72 B-tree creations +8 arenas;40 successful full-store reopens =72
B-tree loads, plus48 rejected metadata directory loads. The six reused raw tests add36 B-tree
creations/30 successful loads and9 arenas: **108 B-tree creations/17 arenas** per ten-test suite.
The standalone six-test gate is a separate execution, not another40 full stores. There were12
retained runtime-command executions during development/final verification: five full ten-test runs,
two raw runs, one rejection run, two large/reuse runs (one failed invalid assumption), one expected
publication-mutant failure, and one existing facade test. Partial failed runs created4 and5 full
stores respectively. Counts are per execution, not a claim that only one fixture matrix ran.
There were **zero PocketIC constructors, installs, federations, Wasm builds or IC evaluations**.

Lizard1.23.0, explicit pre-edit snapshot versus final sources (new prototype has no baseline):

| Source | Functions before→after | CCN sum | Max CCN | Function NLOC |
| --- | ---: | ---: | ---: | ---: |
| Shared B-tree owner/fixtures |37→47|85→111|9→9|789→911|
| Extent owner/fixtures |41→45|204→209|22→22|1178→1210|
| New full-store prototype/fixtures |0→112|0→265|0→16|0→1608|

Directly touched old-owner functions are7→17 for B-tree and7→12 for extent. Their maximum CCN is
9 and18 respectively; unchanged extent validation retains22. B-tree rewrite7→3 delegates to
prepare7/commit5; extent change18→2 delegates to prepare18. This is relocated required preparation,
not a complexity or performance reduction. Largest new functions are change16, open14 and rekey12.
Maximum arity is5 including self; no arity exceeds5. The prototype's substantial test/adaptor cost
is explicit, not proposed as a production subsystem. No speculative allocator tuning, runtime
strategy registry, copied Graph codec or second compiled body implementation was added.

Same-assistant architecture/test/code-quality review found no unresolved blocker within this native
contract. This is not independent/security approval. Full-store managed execution, total resource
bounds, native/IC failure-model differences, coherent activation and real Graph lifecycle remain.
Bulk edge SET remains unavailable.

## Reproduction and provenance

From the repository root, run the scoped commands above. The Graph test includes the canonical raw
sources. The existing standalone manifest is `$TMPDIR/gleaph-extent-ic-qp2ogkcj/Cargo.toml`; its recipe is
in the [body-only report](2026-09-13-property-body-extent-canbench.md). To test the updated raw sources,
keep its lib path pointed at the working artifact and run ordinary offline/locked native cargo test
and Clippy with the repository's shared target. Do not run its Wasm benchmarks as part of this slice.

Ordinary workspace Cargo commands reconciled the pre-existing lock to the known generated hash
`f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`:144 package identities
were pruned, none added, and retained checksums were unchanged. After validation the original
`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2` bytes were restored,
preserving unrelated GPUI/explorer lock entries. Generated bytes/diff remain in the evidence directory;
they are not the final workspace lock. Native results do not claim a no-reconciliation `--locked`
workspace run. The isolated raw command did use its own retained lock.

For CSV reproduction, capture the ten-test command and extract lines beginning `PROPERTY,`, using
`PROPERTY_HEADER,` for field names. Number occurrences separately by
`(key_width,kind,cap,case,region)` in log order, then sort by
`(key_width,kind,cap,case,occurrence,region)`. The26 measured fields plus the occurrence column use
integer values except case names. Source-defined oracles run before a successful command returns.
The extraction/verification script retained in the evidence directory also checks all2640 rows,
encoded map-byte arithmetic, external payload/spans, region sums/projections and constructor counts.

Old260 B-tree +36 extent numeric rows match byte-for-byte as parsed CSV values in both final Graph
and standalone runs. Historical raw/managed results and locks are unchanged. The measured0357
sources are frozen in the provenance archive (measurement dump, not retained),
with `sha256.json`. The archive is never a second active compiled owner or fallback. Exact historical
IC reproduction must extract those measured sources, not compile the updated revision and
claim it reproduces0357 codegen. Canonical-key encoding and preparation changes may affect allocation
and IC instructions even though native Memory counters match.

| Artifact | SHA256 |
| --- | --- |
|0358 full-store source (frozen) |`6be34cec1af36c95876b0d4c107f4928c239a9d85aebe4217f4093641847d173`|
|0358 B-tree source (frozen) |`9270d521cb674d4307fdbefc59f21d26ed9e5a1498723f0ec3734e24494d0899`|
| Updated extent source |`6043e4ac37bedf7bf8f8642f985637b1db1cbfddf8171975aedd63a4d3a74d3d`|
| CSV (287236 B) |`d533779a708f1c269308910c0f8d61cb4ff9b13867be4886b60266efe019be1c`|
| Frozen0357 archive |`d55c2942aa154b0fba193d5e762d2d22f8b29964ba3d6e9d598ff8341b3ed9a1`|

The separately measured [managed IC full-store follow-up](2026-09-17-property-store-managed-comparison.md)
uses this native-validated implementation, actual direct point-get and every region. Fine index-page,
zero-fill and selected-key optimizations remain deferred. Representative request histories and
production layout/threshold/work/atomicity decisions are still open.
