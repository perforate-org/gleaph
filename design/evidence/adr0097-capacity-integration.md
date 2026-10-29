# ADR 0097 retained-prefix ownership and public capacity integration

Code/test commits: `1846cb7d3`, `4deef5a2c`, `6c5822a79`, on `spill-swap`. Main remains unmerged. This evidence does not approve all relocation, arena-exhaustion, bidirectional-write, or space contracts.

Follow-up: [normal relocation, late bucket-reservation failure and batched header reads](adr0097-normal-relocation-header-reads.md) resolves the normal-leaf folding and late-reservation witness gaps in bounded scopes. The counts and open items below describe the earlier commits named above.

## Retained-prefix ownership

The original diagnostic is preserved byte-for-byte as [sources/adr0097-cascade-fixture.rs](sources/adr0097-cascade-fixture.rs), SHA256 `548f7c49e84665381015b66d1e39498176644690649d35683dd2c813ecbde5e2`. It is a historical reproduction source, not a registered acceptance suite: its old live-only tree assertions are superseded by the native-tree assertions in the compiled regressions.

The first failing operation is **append 67**, immediately after seeding 67 rows and enabling bypass, for both edge widths:

| Edge bytes | Active prefix | Free span after append |
|---:|---|---|
| 4 | `[256,261)` | `[256,267)` |
| 10 | `[256,257)` | `[256,271)` |

`/tmp/spill-ownership/red.log` records both runtime failures. The leaf relocation planner and weighted-slide snapshot excluded bypass vertices. Relocation could publish another physical anchor and retire the old footprint without moving or republishing the bypass-owned prefix.

The existing leaf owner now includes bypass prefixes in its snapshot. `bypass_bucket_view` shares the source projection with reverse conversion; it does not transfer ownership. Both buffered/overlapping and streaming/disjoint relocation paths capture the bypass prefix without folding its run. `commit_vertex_edge_span_layout` copies that captured prefix and publishes the new vertex origin before the outer relocation retires the old footprint. The bypass's logical width, live degree and run remain unchanged by relocation. Zero-live retained extents are not excluded solely because their degree is zero.

`cascade_retained_prefix_ownership_{four,ten}_bytes` preserve the original cascade producer. They check every append's initialized prefix, used run length, exact ascending rows and free-span disjointness; the first relocation must retain the same run identity. They then allocate and overwrite unrelated storage, convert without changing prefix/run ownership, verify both directions, exercise 4-byte promotion or exact 10-byte rejection, and allocate/write again while checking active storage remains disjoint from free spans and row/count preservation.

The normal bucket-mode leaf materializer's existing folding policy was **not** changed in this bounded bypass correction. A broader experimental change to normal-bucket folding **and resident sizing** produced nine library failures, including normal-bucket insertion/relocation failures; it was removed rather than weakening those tests. That was not a one-factor attribution experiment. That experiment is preserved as `/tmp/spill-ownership/all-retention-candidate.rs` and `library.log`. This is not blanket approval of normal-bucket relocation retention. The bounded implementation's library run was **646/0** (`library-bounded.log`); strengthened ownership tests passed **2/0** (`ownership-green.log`).

## Sidecar fidelity

`distinct_sidecar` encodes the complete nonzero `target + 1` identity in the first eight bytes and distinguishes subsequent eight-byte groups for width 33. The 8- and 33-byte cases check source values in both directions, complete promoted native streams, leaf tombstone counts, and the neighboring label's exact target/value in both directions before and after promotion.

The tests explicitly assert that deletion folds away the values run. The no-hole cases retain edge and value runs with different prefix widths; the hole cases retain edge runs but **do not** claim simultaneous retained value-run-plus-holes coverage.

An injected wrong implementation redirected the second 8-byte property root entry to the first leaf, only in the no-hole 8-byte case:

- Old periodic fixture: **1/0**, despite the alias (`alias-old-pass.log`).
- Injective fixture with the identical alias: **0/1**, native row/value assertion fails (`alias-new-red.log`).
- Alias removed: **1/0** (`sidecar-green.log`).

All are in `/tmp/spill-ownership/`, alongside both exact injected sources and `property-leaf-alias.patch`. The alias was diagnostic only and is absent from production and committed tests. This demonstrates improved test sensitivity, not a newly discovered production property-reader defect.

## Public four-byte capacity boundary

For a tail, same-label, four-byte bypass, the insert dispatcher consumes only `Store(SpillNeedsPromotion)` through `promote_bypass_with_edge`. Other errors propagate. Wide edges remain on their separate rejection path.

The existing promoter now owns a shared transcription/publication boundary. It prepares native roots and blocks against the still-canonical source, including the pending append at the original high-water ordinal. Its publication callback prepares/reserves the existing bucket conversion plan, then publishes the final tree directly: no intermediate canonical slab-bucket state and no rollback after partial publication. The callback's reservation is the last recoverable failure. Counts and cache operations are preflighted; old runs/spans are retired afterward by the common owner. If preparation or the callback's reservation fails, that owner releases all its unpublished tree blocks and root span.

The internal callback seam has five meaningful arguments (graph, vertex identity, source bucket, optional pending edge, publication callback). It avoids introducing a second tree builder or another persisted representation. Ordinary bucket promotion uses the same owner without a pending edge. No layout or threshold changed. The new tree's bucket conversion allocation uses its physical combined root-region width.

Actual `insert_forward_out_edge` tests:

- Success with **0, 8, 16 holes**, preserving all old live identities, complete native tombstone-inclusive order in both directions, exact appended target/ordinal, bucket degree and global count.
- A real forward-LTB grow failure after one unpublished edge leaf was minted: exact `Forward(Store(RebalanceFailed(GrowFailed { current_size: 1, delta: 1 })))`. Original bypass descriptor, full source arena bytes, initialized prefix bytes, run length, rows in both directions, vertex/bucket/count/value source memory roles and leaf counts remain unchanged. The block returns to the LTB free list and allocator accounting proves the root span returned in full. Successful retry checks the complete native stream and counts, including zero slab-live count after promotion.

`/tmp/spill-public/runtime-red.log`: **0/4** against the previous public caller, each surfacing `Forward(Store(SpillNeedsPromotion))`. `green-attempt.log`: **4/0** after integration. Final strengthened cases are in the full **650/0** run.

The pre-existing retained-run boundary witnesses now invoke `EdgeStore::insert_edge` for their exact low-level `SpillNeedsPromotion` rejection. All unchanged-state assertions remain. Public four-byte success is checked separately; wide-edge rejection is not reclassified as successful promotion.

## Final validation and artifacts

Repository root:

- `timeout 120 cargo test -p ic-stable-lara --lib`: **650 passed / 0 failed**, exit 0, `library-final.log`.
- `timeout 120 cargo clippy -p ic-stable-lara --lib -- -D warnings`: passed, `clippy-final.log`.
- Scoped `rustfmt --edition 2024 --config skip_children=true --check` on the six changed Rust files: passed, `fmt-final.log`.
- `git diff --check`: passed.

These logs are under `/tmp/spill-public/`. The initial full run had one failure because the old higher-level rejection assertion became public success; the replacement retains the lower-level exact rejection contract, rather than accepting either outcome.

From `crates/ic-stable-lara`, each command was `timeout 210 canbench <name> --csv`, **without `--persist`**:

| Name | Exact instructions |
|---|---:|
| `bench_l_mix_hub_asc_33x50` | 896,221 |
| `bench_l_mix_hub_scn_33x50` | 713,437 |
| `bench_l_bp_promo` | 106,087 |

Logs and exact CSVs are in `/tmp/spill-public/`. First release compilation took 49.87 seconds; later compilations took 1.13 and 0.66 seconds. The ascending change from the preceding 904,471 measurement is not attributed to a new traversal optimization. Native read counters remain the same. Bypass conversion is +180 instructions versus the preserved full artifact; these measurements do not isolate public tree-preparation cost.

The complete workspace CSV was copied aside before focused runs and restored after each. Full YAML/CSV and benchmark source hashes remain unchanged. Generated `Cargo.lock` churn and temporary diagnostic registration were restored; foreign files were preserved.

## Clean-reference native comparison

After the correctness commits, a new disposable `git archive` snapshot of pinned `9a64049f4` was created. No existing dirty checkout was edited. The same instrumented 33×50 probe ran sequentially in that snapshot and at `6c5822a79`, sharing `/Users/yota/dev/gleaph-spill/target`. Both ran **1/0**; test-profile compilation took 11.95 and 13.12 seconds. `/tmp/spill-main-native/` contains the reference commit/path, common probe, complete logs, and target path. The branch probe was removed afterward.

The [common probe source](sources/adr0097-mixed-native.rs) and
[role-level CSV](adr0097-mixed-native-comparison.csv) are preserved with this evidence.
The workload, constructor capacities and traversal shapes are identical. Setup runs outside the counters. Both executions verify exact ascending and descending targets, 1,650 live decodes/callbacks and zero dead decodes. The resulting physical layouts differ: main has thirty 50-row prefixes, then 31/0/0; the branch has 1/0/…/0 with retained per-bucket runs.

| Direction / role | Main reads / bytes | Branch reads / bytes |
|---|---:|---:|
| Ascending vertex | 35 / 735 | 35 / 875 |
| Ascending bucket | 407 / 4,705 | 737 / 7,670 |
| Descending vertex | 33 / 693 | 33 / 825 |
| Descending bucket | 759 / 5,313 | 1,089 / 8,118 |
| Edge slab, either order | 31 / 15,310 | 1 / 10 |
| Edge log/run memory, either order | 6 / 5,010 | 198 / 17,018 |

All other memory-role read counts are zero during traversal. Main log reads are three 4-byte reads and three 1,666-byte reads. Branch run reads are 132 four-byte headers and 66 payload reads totaling 16,490 bytes. Full per-role/read-length histograms are preserved. Overall the branch makes **492 additional reads** in each direction despite slightly fewer total bytes and identical edge decode/callback counts.

This establishes descriptor traffic and physical read granularity as measurable differences, not their individual IC instruction costs.
The inspected spill/slab iterator setup calls `bucket_slab_window_end_exclusive_after_bucket` from
`visit_slab_bucket_span_edges` (at inspection time `single_bucket_span_iter`); following-bucket boundary reads are a concrete next investigation site.
No boundary check was removed or replaced by an inferred extent to optimize these measurements. It is a fresh native comparison, not a fresh main canbench result. No new mixed-hub optimization, complete benchmark refresh, arena-exhaustion acceptance, PocketIC, or broader performance/space approval is claimed.
