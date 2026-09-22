# ADR 0097: pinned survey, descriptor writes and hybrid value batches

## Scope and provenance

The complete survey is pinned to **`5ad7f5a61`**, before the two changes below. It is not a final-source persistence refresh or landing approval. `main` remains untouched and unmerged.

From `crates/ic-stable-lara`, `timeout 240 canbench --csv` completed with **exit 0 in 189.5 seconds**: **189 totals and 554 scopes**, 743 unique rows. Every observed instruction total is positive. Historical `3ea7dc739` has the same 189 benchmark names and 572 scopes; one scope appears and nineteen disappear in the new execution. Absent scopes are **not zero-cost observations**. Nested scopes are inclusive, not additive.

- [Complete exact survey CSV](adr0097-survey-5ad7f5a61.csv)
- [All-name/union-of-scopes comparison](adr0097-survey-5ad7f5a61-comparison.csv)
- [Name, scope and threshold summary](adr0097-survey-5ad7f5a61-summary.json)
- [Run metadata](sources/adr0097-survey-5ad7f5a61-run.json)

The comparison distinguishes exact historical/current integers from **rounded main console** numbers. Its main nominal integers are decimal expansions of displayed values, not recovered exact measurements; all 189 nominal totals agree with the previously recovered main table. Raw CSV delta columns use preserved YAML, not the immediately preceding experiment. CSVs use LF without changing numeric fields.

The pinned survey has **25 totals above +5% versus rounded main**, superseding the old count of 22 only for this source revision. Nine totals exceed exact `3ea7dc739` by more than 5%:

| Name | `3ea7dc739` exact | `5ad7f5a61` exact |
| --- | ---: | ---: |
| `bench_l_def_bp_iter_128` | 24,993 | 34,607 |
| `bench_l_fe_24x51` | 271,574 | 482,204 |
| `bench_l_fe_48x51` | 387,038 | 806,564 |
| `bench_l_iter_128` | 19,371 | 40,105 |
| `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384` | 43,376,650 | 15,498,702,117 |
| `bench_t_v_dense_a` | 18,393 | 75,841 |
| `bench_t_v_dense_d` | 18,137 | 71,821 |
| `bench_t_v_ebreak` | 9,422 | 30,417 |
| `bench_t_v_window` | 9,893 | 48,008 |

The **16K increase was unexplained at this checkpoint**. The [pinned follow-up](adr0097-descriptor-rejection-crossover.md) records matching canonical targets/order and exact totals, but **not movement-contract parity**: historical completion remapped 8,192 survivors without notifications. Its reset oracle was invalid for that contract. Physical state selects silent bulk packing versus repeated single-edge compaction. The benchmark itself supplies zero on every step; whole-span/deferred callers already advance the cursor. [Current-head owner/observer repair and a byte-identical cursor diagnostic](adr0097-compaction-movement-publication.md) distinguish those cases. Performance is not recovered or accepted. Proximity to rounded main **15.54 B** remains neither acceptance nor proof of equivalent physical work; no fixture or policy change conceals the difference.

Likewise, `bench_t_v_dense_a`, `bench_t_v_dense_d`, `bench_t_v_window` and `bench_t_v_ebreak` are names, not proofs of slab-only state. Their ordinary insertion fixtures lack a no-run assertion. Physical-state comparison after retention corrections is still needed; no fixture change or silent fold was introduced here.

### Main aliases and operation mismatches

| Current name | Main name |
| --- | --- |
| `bench_l_du_spill_del_scn` | `bench_l_du_log_del_scn` |
| `bench_l_ip_spill_8b_of` | `bench_l_ip_log_8b_of` |
| `bench_l_du_span_compact_mnt` | `bench_l_du_log_fold_mnt` |
| `bench_remove_churn_bypass_spill_regime_probe` | `bench_remove_churn_bypass_log_regime_probe` |

Main has two additional, retired shared-log expansion fixtures, `bench_l_b_eo_exp` and `bench_l_b_ipe_exp`; these are not missing current results. They remain visible in the union comparison.

`bench_l_s2_h_del_1024` and the `bench_labeled_stage2_hub_delete_half_by_slot_then_compact_*` family use different main/branch compaction policies. Main calls the original explicit compactor; the branch selects `SlackMayBeDropped`. Their whole totals are not operation-equivalent. Direct unlink/fold names also conceal the changed linked-log versus retained-run tombstone work. The bypass-regime alias still asserts **no active log/run** on both sides.

`bounded_inline_*` setup on main explicitly folds the shared log, while branch setup retains runs and verifies combined extent. Same API does not mean same physical fixture. `bounded_inline_promote_full` measures traversal, not promotion preparation, and is not a substitute for the hybrid-values API. The shared-leaf-pressure fixture's old post-measurement log watermark is replaced by a vertex-count observation; no watermark comparison is implied. Source diffs against `9a64049f4` and the empty historical-branch diff are retained with the survey.

## Descriptor insertion: `e596eadae`

`LabelBucketStore::insert_label_bucket_at` owns in-place descriptor insertion. It already reads and validates the complete old row, but serialized the unchanged prefix along with the shifted suffix. The change keeps that validation and the existing span/count checks, computes the insertion address with checked addition, and writes only the changed suffix. No cache, layout, reservation, slack, threshold or compaction rule changes.

The setup-heavy `bench_l_ip_pstats_*` totals include construction and seeding. The actual pressure scope stays **36,157 instructions** in all three controls; this is not a faster pressure algorithm.

| Fixture | Survey before | After descriptor change |
| --- | ---: | ---: |
| `bench_l_ip_pstats_16` | 45,934,879 | 45,848,389 |
| `bench_l_ip_pstats_64` | 64,619,574 | 62,985,048 |
| `bench_l_ip_pstats_256` | 303,322,427 | 276,081,039 |

[Exact after CSV](sources/adr0097-descriptor-after.csv), [native before](sources/adr0097-descriptor-native-before.txt), [native after](sources/adr0097-descriptor-native-after.txt), [probe source](sources/adr0097-descriptor-native.rs).

The same native fixture separates constructor, seeding and pressure, then checks exact live slots/targets and nonzero-width values. At 256 labels, bucket seeding write bytes fall **4,580,248 → 199,008**. Read calls/bytes, write-call count, growth and other memory roles are unchanged. In the native tables, each role records `[read calls, read bytes, write calls, write bytes, grow calls, requested pages]`; pages are 64 KiB. These are `Memory` observations, not IC syscall counts or a per-call instruction price.

Front/middle/tail insertion and malformed-unchanged-prefix witnesses pass before and after. The malformed wire row must retain the existing panic `invalid LabelBucket wire bytes: InlinePropertyBytesStateWithoutSchema`, with memory and vertex unchanged. The initial typed-error expectation was a test-oracle error, not production red evidence. Descriptor validation completed **655/0**, strict library clippy and scoped formatting. The [subsequent owner-boundary correction](adr0097-descriptor-rejection-crossover.md) prepares the updated vertex before writing at the real maximum count; the suffix optimization alone did not prove failure atomicity there.

## Actual hybrid-value contract: `8a6bafd1b`

The unsupported `0b6673a0d` removal was not harmless. `prefetch_spill_replay_tags` includes tombstones in its length; it does not certify a dense live extent. The affected owner is `visit_hybrid_out_inline_property_batches_for_bucket_next`, reached by outgoing value batches, deferred incoming value batches and the facade's bytes-first candidate/selected-topology flow. Width-zero mixed-hub scans and bounded inline edge-range visitors are different paths.

Three witnesses use widths **8 and 33**, injective values, a nonempty edge prefix, independent edge/value prefix widths and retained runs, neighboring labels, optional holes at **0/3/31/63**, and a later append. They assert both orders, exact slot/value association, bounded first-batch contents, callback termination, scratch reuse and selected target identity.

Original implementation with the initial three witnesses: **0/3 pass**. Hole traversal raises `MissingAscLogIndex { asc_log_index: 1 }`; callback `Break(17)` becomes `Continue(())`; descending values are attached to the wrong prefix slots. The initial phase-two witness fails its phase-one values assertion, so that result alone is **not an isolated phase-two failure**.

Restoring only checked `prefix + used run length == live degree` admission gives **1/3 pass** on those initial witnesses, before the scratch-reuse and exact first-batch strengthening. On the actual incoming hybrid benchmark it changes **126,123 → 126,142** instructions ([guard-only CSV](sources/adr0097-hybrid-guard-only.csv)). Removing that guard did not justify losing the contract for nineteen instructions.

The final owner:

- clears batch buffers and old replay at phase-one entry;
- retains checked run mint/prefetch and checked extent arithmetic;
- delegates hole-bearing rows to the existing sparse rank-mapping path;
- replaces duplicate prefix/ascending/descending emitters with one ordered ordinal/value reader, preserving physical-plane batch boundaries;
- propagates `Break` at each callback and publishes no replay for interrupted scans;
- preserves canonical run-index order in the existing replay, regardless of traversal direction.

No new cache or persistent metadata is introduced. An additional scratch-reuse witness reproduced stale neighboring-label bytes before entry clearing. A deliberate final-code mutation reversing the descending replay fails phase two with `[(96,8),(63,41),(31,73),(8,96)]` instead of `[(96,96),(63,63),(31,31),(8,8)]`. The mutation is removed. Final library result: **658/0**; strict library clippy and touched-file formatting pass.

### Matching nonzero-property measurements

| Actual consumer | Survey before | Corrected owner |
| --- | ---: | ---: |
| `bench_t_v_in_hyb_ip` | 126,123 | 111,370 |
| `bench_t_v_pf_then_sel` | 280,180 | 266,211 |
| `bench_l_ip_spill_8b_of` | 1,275,736 | 1,118,925 |

[Traversal controls](sources/adr0097-hybrid-final-traverse.csv), [repeated value-batch control](sources/adr0097-hybrid-final-batches.csv). The final correction combines necessary correctness repairs with ordered-emitter consolidation; these totals do not isolate a cost for each individual repair. Other totals selected by `bench_t_v` are unchanged, including direct replay, scalar inline-property visitors and sparse/dense topology controls. This does not claim all nonzero-value or bounded regressions recovered.

The matching property-first native probe uses the benchmark's four-byte edge, 256 rows, eight-byte values, targets `slot + 10`, and ascending phase one plus all selected slots in phase two. Its actual shape is **edge prefix 0/run 256; value prefix 256/no run**. Both revisions emit one batch with exact values and return all 256 exact selected targets. An initial diagnostic assumed a values run and failed during shape reporting on the no-run sentinel; it is not a production failure. The corrected paired probes each pass **2/0**, including the mixed-hub control.

[Native before](sources/adr0097-hybrid-native-before.txt), [native after](sources/adr0097-hybrid-native-after.txt), [matching property probe](sources/adr0097-hybrid-native.rs). Phase one has identical vertex **1/25**, bucket **2/82**, edge-run **17/1,060**, and value-slab **1/2,048** reads/bytes. Phase two has identical vertex **1/25**, bucket **2/82**, and edge-run **512/2,048**, with 256 visits. Other roles and writes/growth are zero. Instruction improvement therefore must not be described as reduced stable-memory traffic. The phase-two scalar reads are an observed investigation site, not a claimed per-read cost.

## Mixed-hub control and remaining work

Both production changes leave mixed-hub traversal at **790,432 ascending / 557,118 descending**, versus rounded main **452.96 K / 350.32 K**. Exact order, 1,650 live decodes/callbacks, zero dead rows, prefix shape and all role/read-length histograms are unchanged in the paired native control. The insertion total falls **59,092,295 → 58,998,829** under the descriptor change. [Descriptor control](sources/adr0097-descriptor-mixed-control.csv), [hybrid control](sources/adr0097-hybrid-final-mixed-control.csv).

Remaining descriptor/boundary lookahead, run read granularity and iterator execution costs still need one-factor attribution. Do not infer instruction costs from read counts or transfer the nonzero-property result to this width-zero path. The nine historical increases, including the now-attributed but unrecovered 16K cost, other rounded-main regressions, wide/arena/other-caller atomicity, space/PocketIC and final-source unfiltered persistence/all-name review remain open.

## Execution evidence and budget exception

All canbench runs were **without `--persist`**, on the configured normal target. Focused commands used `canbench bench_l_ip_pstats_ --csv`, `canbench bench_l_mix_hub_ --csv`, `canbench bench_t_v_in_hyb_ip --csv`, `canbench bench_t_v --csv`, and `canbench bench_l_ip_spill_8b_of --csv`, all bounded and with exit 0. The final three hybrid/control invocations each used `timeout 150`; the survey used `timeout 240`. The workspace CSV was restored after every invocation. The complete historical CSV/YAML and benchmark source hashes remain unchanged:

- CSV: `5226fd785d56ba528ba5d5a50a08206cf62034e48a9b522cbe254c5c401ea567`
- YAML: `a3e2c76e91a05c7c1a7ccd6fea56d670939b50411d697645257eade4611d162b`
- `labeled/bench.rs`: `bb12331194546dbd91ae1ef5e0b5ba0d3fc141ffd93ba8b71ed956b4b76acbc7`

Final native command: `timeout 90 cargo test -p ic-stable-lara --lib`, **658 passed**, 6.20-second test runtime. Strict clippy used `timeout 120 cargo clippy -p ic-stable-lara --lib -- -D warnings`; scoped rustfmt checks and commit-range whitespace checks passed. Temporary probe registration, diagnostic mutations and generated lock changes were removed. The locked dependency churn is preserved as patches; the full survey's as-used lock is reconstructed from its pinned lock plus `/tmp/spill-survey-5ad/lock.patch`, with git blob `96b3191d2920d046ec6296ace2ae7dd89bf3c323` matching the patch. Its SHA256 is `f5d323d0c66d9afe939a6c7e13db316e201c39fce4c4bde44e515a8c855496bf`. The `Cargo.lock` line in the original `build-hashes.txt` instead describes the restored/pinned lock (`15d08391d78c37bdefaecf488881f7d37f5e70e9596e4978ebeb3cc18ceea9a2`); it must not be cited as the as-used lock hash.

**Budget exception:** individual commands were bounded, but cumulative timing was not maintained correctly. The 189.5-second survey plus command-log creation-to-last-write intervals reconstruct approximately **726 seconds**, exceeding the shared **600-second** budget. This is a filesystem-based approximation, not a complete monotonic command ledger. No cumulative-budget compliance is claimed; further Rust/performance runs were stopped. Future slices need one shared monotonic deadline, including compilation and canbench execution, rather than summing build/test lines (which omit canbench execution and can contain minute-formatted durations).

The final structural validators for local plans `0329-lara-current-performance-survey.md` and `0330-lara-hybrid-value-batch-contract.md` both exit 0; that checks plan structure, not budget compliance or landing approval. No independent review, Lizard comparison, new space audit or PocketIC run is claimed.

Raw sources, commands, complete logs, CSVs, compiler/build provenance, negative diagnostics and SHA256 manifests are retained in `/tmp/spill-survey-5ad/`, `/tmp/spill-descriptor/`, and `/tmp/spill-hybrid/`. Diagnostic cumulative patches are not patches to stack onto the branch. The preserved full benchmark artifact still describes `3ea7dc739`; the new complete survey describes `5ad7f5a61`; only the explicitly listed focused rows describe the subsequent production changes.
