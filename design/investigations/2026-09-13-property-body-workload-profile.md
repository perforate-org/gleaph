# Property body sizes and workload evidence

Status: Experimental — repository-input profile complete; production workload frequencies unknown.
Anchor: 2026-09-13 11:18:25 UTC +0000.
Profile verified: 2026-09-13 11:35:15 UTC +0000.
Repository anchor: `95d7910878a5ee8af95af878f2b0df952edd70dc`.

## Conclusion

The inspected demo inputs contain **no property Value larger than 143 encoded bytes**, and none
needs more than one 4 KiB chunk. They therefore do not exercise the large external-body trade-off
in the [32-case canbench comparison](2026-09-13-property-body-btree-comparison.md#ic-stable-memory-follow-up).
The scaled Social demo repeats 71 authored post texts 100 times; it does not add a long-text tail.

A hypothetical 128-byte directory-cell threshold leaves 300 of 35932 Social vertex-property
occurrences external (0.83%), and zero of 94460 edge-sidecar property occurrences. **This is not
99.17% of accesses being inline**: every Social prepared query projects `Post.body`, whereas other
stored fields have different query usage. Static property counts cannot supply traffic weights.

No runtime request history or old/new value pairs were supplied or collected. Read/rewrite/shrink
frequencies remain unknown, not measured as zero. These inputs support keeping the small-value path
cheap and continuing existing-library evaluation; they justify neither a production threshold/cap
nor a custom allocator. A representative long-body/old-new sample is the remaining input for an
adoption decision, not another favorable synthetic benchmark presented as real traffic.

## 1. Corpora and storage ownership

The profile consumes the actual **typed load artifacts**, not YAML text size, a JSON row's size or
the entire graph manifest. Diagnostic source (measurement dump, not retained)
uses the owning [`Value::to_binary_bytes`](../../crates/gql-value/src/impls.rs) after the same
[canonical Value serde](../../crates/gql-value/src/serde_support.rs) used by the CLI. It does not copy
a codec or depend on Graph internals.

| Corpus | Origin | Vertex rows | Edge rows | Vertex property occurrences | Edge property occurrences |
| --- | --- | ---: | ---: | ---: | ---: |
| Knowledge | Checked-in JSONL | 21 | 37 | 48 | 0 |
| Social 1x1 | Authored-scale generator | 103 | 554 | 451 | 1108 |
| Social 5x20 | Explicit configured-scale generator | 7244 | 47230 | 35932 | 94460 |

Sources:

- Knowledge [vertices](../../demo/knowledge/seeds/vertices.jsonl) and
  [edges](../../demo/knowledge/seeds/edges.jsonl), with its
  [graph type](../../demo/knowledge/migrations/000001_knowledge_graph_type/up/00_knowledge_graph_type.gql).
- Social's existing [generator](../../demo/social/scripts/build-config.mjs) and
  [load artifact owner](../../demo/social/scripts/social-load-artifact.mjs), run into separate
  temporary output roots. [Scaling](../../demo/social/scripts/social-scale.mjs) defines the default
  5 user generations × 20 post generations. Both scales were passed explicitly here; no files in
  the demo's ordinary output directories were regenerated.
- Social 1x1 has 28 users/71 posts; 5x20 has 140 users/7100 posts. There are exactly 71 distinct post
  texts in each. `scalePostsForUsers` copies the body and changes identifiers/references, not text
  length. The two corpora are separate sensitivity views, not independent samples to pool.
- Social's [graph type](../../demo/social/migrations/000001_social/up/00_social_graph_type.gql)
  defines the loaded node properties. YAML `bio`, topics-as-lists and UI presentation metadata are
  **not** extra PropertyStore values in the generated load rows. Relations become edges.
- All inspected edge `inline_value`s are JSON null, and these demo graph types declare no inline
  property profiles. Social's two per-edge load properties are `demo_edge_id` and `demo_kind`;
  Knowledge's edges have no property entries. These are sidecar **input occurrences**, not a
  measurement of deployed physical copies, stable keys, indexes or allocation. `demo_edge_id` is
  an existing demo property, not newly introduced lifetime edge identity.
- Knowledge's [embedding path](../../demo/knowledge/README.md#embedding-ingestion-path) ingests vectors
  separately; Social's load-artifact owner excludes manifest embeddings. Embedding arrays, query
  vectors and returned element IDs are not counted as canonical property bodies merely because
  they appear elsewhere in a JSON document or result.

The retained histograms (measurement dump, not retained) contain all 266
scope/label/property/encoded-length bins and input file hashes. No deployed database or browser
traffic was sampled. A valid input artifact is not proof of a successful installation or current
on-chain state.

### Length convention

`b = Value::to_binary_bytes().len()` includes Value type/length tags and UTF-8 bytes. It excludes
property names, graph/entity/slot keys, labels and the outer stable-value envelope. The current
[StoredPropertyValue](../../crates/graph/src/facade/stable/vertex_properties.rs) adds one version byte
per stored value; neither that byte nor the B-tree/node/MemoryManager overhead is a body payload.
For these occurrences only, adding the outer byte would add the property count to the total below.
This is arithmetic, not a total-storage estimate.

Thresholds below are hypothetical cutoffs on **b**, inclusive for small cells (`b <= T`). A future
codec choosing another threshold domain would require recomputation. A directory small cell is not
LARA edge INLINE; no adjacency/profile placement is changed by this analysis.

## 2. Encoded size distributions

Occurrence-weighted nearest-rank quantiles: choose the smallest size whose cumulative count is at
least `ceil(q * count)`. Empty populations have no quantile, not a zero-byte body.

| Corpus / scope | Values | Total Value bytes | p50 | p95 | p99 | Maximum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Knowledge vertex | 48 | 1432 | 20 | 69 | 79 | 79 |
| Knowledge edge | 0 | 0 | — | — | — | — |
| Social 1x1 vertex | 451 | 9543 | 11 | 98 | 119 | 143 |
| Social 1x1 edge | 1108 | 28264 | 19 | 47 | 47 | 59 |
| Social 5x20 vertex | 35932 | 859068 | 11 | 101 | 121 | 143 |
| Social 5x20 edge | 94460 | 2718215 | 19 | 47 | 59 | 59 |

No measured Value exceeds 4096 bytes in any scope. Social's longest `Post.body` is 138 UTF-8 bytes
plus five Value encoding bytes. Knowledge's maximum is a `Concept.definition` at 79 encoded bytes;
its eight `Document.body` values top out at 69 bytes. These are short example sentences, not a
representative document/RAG corpus.

## 3. Hypothetical directory thresholds

Each cell is **external value count / external Value bytes**; the test is strictly `b > T`.
Bytes are body exposure, not allocated body-tree space or I/O volume.

| Corpus / scope | T=32 | T=64 | T=128 |
| --- | ---: | ---: | ---: |
| Knowledge vertex | 18 / 1045 | 6 / 423 | 0 / 0 |
| Knowledge edge | 0 / 0 | 0 / 0 | 0 / 0 |
| Social 1x1 vertex | 71 / 6054 | 53 / 5057 | 3 / 412 |
| Social 1x1 edge | 397 / 16508 | 0 / 0 | 0 / 0 |
| Social 5x20 vertex | 7100 / 605400 | 5300 / 505700 | 300 / 41200 |
| Social 5x20 edge | 39928 / 1764860 | 0 / 0 | 0 / 0 |

For the scaled Social input, the count and byte fractions illustrate the different denominators:

| Scope / threshold | External values | Fraction of values | Fraction of Value bytes |
| --- | ---: | ---: | ---: |
| Vertex / 32 | 7100 | 19.76% | 70.47% |
| Vertex / 64 | 5300 | 14.75% | 58.87% |
| Vertex / 128 | 300 | 0.83% | 4.80% |
| Edge / 32 | 39928 | 42.27% | 64.93% |
| Edge / 64 | 0 | 0.00% | 0.00% |
| Edge / 128 | 0 | 0.00% | 0.00% |

But the frequently projected property is not a uniformly sampled stored value:

| Body population | Values | External at 32 | External at 64 | External at 128 |
| --- | ---: | ---: | ---: | ---: |
| Knowledge Document.body | 8 | 8 | 1 | 0 |
| Social 1x1 Post.body | 71 | 71 | 53 | 3 |
| Social 5x20 Post.body | 7100 | 7100 | 5300 | 300 |

For Social `Post.body`, 53/71 = **74.65%** remain external at 64 and 3/71 = **4.23%** at 128,
versus 14.75%/0.83% across all scaled vertex properties. Even those body fractions are not hit rates:
which posts are selected, parent references, repeated paths, filters and caching still matter.
Knowledge's four prepared queries do not explicitly project/filter `body` or `definition`, so its
six values over 64 bytes do not establish six corresponding external reads per query run.

No cutoff is selected. Increasing the directory cell bound affects node geometry/density; the
[earlier separation experiment](2026-09-13-property-value-separation-design.md#8-probe-results-and-disposition)
and [B-tree constructor results](2026-09-13-property-body-btree-comparison.md#short-body-density)
already show why a larger maximum is not free. Empty/new body regions, keys, descriptors, node
occupancy, decoded Value workspace and the MemoryManager's region floors remain unmeasured here.

## 4. What can and cannot be said about operations

| Evidence | Verified fact | Not established |
| --- | --- | --- |
| Knowledge's four prepared definitions and browser scenario dispatch | Named read queries; property filters/projections use names, title, is_public, plus topology/vector/element IDs | Executions per second, property gets per query, read/write ratio |
| Social's six prepared definitions and `loadScenario` | All six project `Post.body`; feed variants also project parent body; no SET/REMOVE in these definitions | Which scenarios/posts are hot, update/shrink frequency or old/new lengths |
| Demo load flow | CLI insert artifacts followed by prepared-query interaction | A live append-only production workload or zero future updates |
| Graph property writer | Existing previous-value acquisition, persistability/index preflight, whole-property set/remove boundaries | Arrival frequency, observed changed-byte fraction or rewrite/shrink distribution |
| CLI large-row fixture | Five synthetic 600000-character SET payloads exercise splitting/resume through a fake transport | A real 600 KB workload sample or Graph runtime cost |

Read definitions: [Knowledge prepared directory](../../demo/knowledge/prepared/) and
[Social prepared directory](../../demo/social/prepared/). Dispatch:
[Knowledge SCENARIOS](../../demo/knowledge/src/routes/index.tsx) and
[Social loadScenario](../../demo/social/src/components/SocialDemo.tsx).
Mutation owners: [CLI load](../../crates/cli/src/load.rs), especially
`update_phase_fits_large_rows_to_the_payload_bound_and_resumes_without_gaps`, and
[Graph property writes](../../crates/graph/src/facade/store/properties.rs).
The existing PocketIC [bulk-load lifecycle fixture](../../crates/pocket-ic-tests/tests/adr0057_durable_bulk_load.rs)
covers correctness/interleavings, not production frequency; it was inspected, not rerun or pooled
into the size histogram.

The static sources give **no timestamped old/new history** from which to count same-size rewrites,
growth, shrink or no-ops. The strings `SET`, the number of tests, seed cardinality and generated
copies are not request-rate estimates. Further, [`ensure_persistable`](../../crates/graph/src/property/persisted.rs)
checks encodability, not a 143-byte or 4 KiB product limit. The demo maxima cannot justify rejecting
larger valid properties or removing the large-body safety requirements.

## 5. Applying the measured costs without inventing traffic

For every sampled external body, both Whole and C4096/default would store one raw body entry under
the **comparison's common 18-byte key/default geometry**. There is no multi-chunk mutation here and
no 1 MiB neighbor. That is a shape observation, not a benchmark of these complete maps: node counts,
key ordering, the directory and Graph's Value encoding still differ from the two-body IC fixtures.
Do not multiply the canbench 65-byte cost by these property counts to claim total query cost.

For explicit *hypothetical* operation counts matching the IC fixtures, let:

- `r_s`, `w_s`: 65-byte external-body read/rewrite beside a 1 MiB body;
- `r_L`, `w_L`: 1 MiB body read/full same-size rewrite beside a 65-byte body;
- `s_L`: 1 MiB→65-byte shrink in that large-target fixture.

Using the shared range/assembly read contract and matched previous-value acquisition, the additive
instruction difference is:

```text
Chunk - Whole = -63914*r_s -2433437*w_s
                +907886*r_L +21376230*w_L +33212749*s_L
```

These coefficients are differences of exact
persisted canbench counters (measurement dump, not retained), not fitted
latency/Graph costs or an instruction-limit proof. They combine independent measured fixture shapes;
real tree population, locality, message boundaries, decoding, policy and index work are not modeled.

With only `w_s` and one `w_L`, at least **9** small-neighbor rewrites offset the added cost of one
large full rewrite (`ceil(21376230/2433437)`). One large shrink needs at least **14** such rewrites
(`ceil(33212749/2433437)`). No evidence here supplies either ratio. If `T >= 65`, the 65-byte selected
body is directory-inline and this specific small-external-body benefit term no longer applies;
directory access cost is unmeasured, not zero. Keeping values inline also changes the body tree's
population, so the old fixture costs are not automatically transferable.

The ordinary Whole point-get control is faster than the shared range/assembly read. Comparing a
1 MiB chunk read against it gives a different read difference (3007803 instructions), not 907886.
The formula above deliberately retains one explicit read contract rather than mixing baselines.
Neither includes Graph Value decoding. Large replacements/shrinks remain relevant supported-shape
risks even though the inspected seeds do not contain them.

## 6. Decision and missing evidence

1. Keep production storage unchanged. The data does not justify a custom allocator, an unbounded
   Whole body tree, a chunk size choice or T=64/128 activation.
2. Retain directory-small-value separation as a design direction. These inputs make its common path
   worth measuring, but do not select its codec or threshold. No adjacency bytes/IDs are added.
3. Before weighting the large-body cases, obtain a bounded, sanitized sample from the intended
   application: property/label/type, encoded old/new sizes, operation (read/set/remove), and actual
   counts over a stated interval. Distinguish create/grow/same-size/shrink and unchanged assignments.
   Do not collect property contents, principals or credentials merely to compute lengths/counts.
4. If no such application sample is available, choose workload assumptions explicitly with the user
   and report scenario sensitivity rather than labeling a synthetic mix representative. A benchmark
   at sampled small sizes can refine common-path cost, but cannot answer unknown long-body rates.

No persistence layout, MemoryId, public API, production cap, whole-row atomicity, durable edge-SET
outcome or activation contract changes in this slice. Bulk edge SET remains unavailable. The custom
extent proposal remains deferred, unimplemented and unmeasured.

## 7. Reproduction and validation

From repository root, with cached Rust/Node dependencies (no deployment or network required):

```sh
repo="$PWD"
probe="$(mktemp -d $TMPDIR/gleaph-body-workload-XXXXXX)"
GLEAPH_DEMO_OUTPUT_ROOT="$probe/social-1x1" SOCIAL_DEMO_USER_SCALE=1 SOCIAL_DEMO_POST_SCALE=1 node demo/social/scripts/build-config.mjs
GLEAPH_DEMO_OUTPUT_ROOT="$probe/social-5x20" SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20 node demo/social/scripts/build-config.mjs
printf '%s\n' \
  '[package]' 'name = "gleaph-property-body-workload-profile"' 'version = "0.0.0"' \
  'edition = "2024"' 'publish = false' '' '[[bin]]' \
  'name = "gleaph-property-body-workload-profile"' \
  "path = \"$repo/design/investigations/artifacts/2026-09-13-property-body-workload-profile.rs\"" '' \
  '[dependencies]' \
  "gleaph-gql-value = { path = \"$repo/crates/gql-value\", features = [\"serde\"] }" \
  'serde = { version = "1", features = ["derive"] }' 'serde_json = "1"' '' \
  '[profile.dev]' 'debug = "line-tables-only"' '' \
  '[profile.test]' 'debug = "line-tables-only"' > "$probe/Cargo.toml"
rustfmt --edition 2024 --check design/investigations/artifacts/2026-09-13-property-body-workload-profile.rs
cargo test --offline --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --bin gleaph-property-body-workload-profile
cargo clippy --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --bin gleaph-property-body-workload-profile --tests -- -D warnings
cargo run --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --bin gleaph-property-body-workload-profile -- demo/knowledge/seeds/vertices.jsonl demo/knowledge/seeds/edges.jsonl > "$probe/knowledge.json"
for corpus in social-1x1 social-5x20; do
  cargo run --offline --locked --manifest-path "$probe/Cargo.toml" --target-dir "$repo/target" --bin gleaph-property-body-workload-profile -- "$probe/$corpus/seeds/vertices.jsonl" "$probe/$corpus/seeds/edges.jsonl" > "$probe/$corpus.json"
done
```

These JSON outputs are the `rows`/`value_histogram` portions of the retained corpus artifact. It also
records scale metadata and SHA-256/file-byte counts for each input. Summaries use `sum(count)` and
`sum(value_bytes*count)`; external totals use the same sums over `value_bytes > T`. Quantiles use the
nearest-rank definition above. No timing is attached to this native profiler.

Two native tests passed (6.59s build, <0.01s runtime):
`counts_property_occurrences_and_uses_value_codec_bytes` and
`rejects_unsupported_ownership_and_malformed_values`. They distinguish rows from values, UTF-8 from
character/JSON length, outer envelopes from Value bytes, explicit NULL from absence, and unsupported
inline/multi-label/ambiguous rows from measured sidecars. This narrow diagnostic is not the CLI's
general validator: duplicate properties/endpoints/auth/schema are not reimplemented. Independent
input checks verify no duplicate JSON keys in these actual corpora.

Scoped Clippy passed (3.97s), both generators (Node v22.22.3) and all three native profiling runs
completed, and rustfmt passed. The initial diagnostic build had test-map indexing errors (missing borrowed keys),
fixed before the successful test run; this was not a semantic production red-to-green. The diagnostic
uses the Value crate's normal default features plus serde; production codecs were not changed.
Lizard 1.23.0: five functions, max CCN 9, aggregate CCN 20, function NLOC 93. No five-argument helper,
new production module or dependency, stable collection, PocketIC constructor or canister install.

Evidence: `$TMPDIR/gleaph-body-workload-t1lwzwde/` contains isolated manifest/lock, generated artifacts,
logs, input snapshots/hashes, analysis checks and same-assistant review. No independent/security
review or live workload evidence is implied. No workspace tests, Graph builds, PocketIC or new
canbench were needed or run: existing complete instruction evidence was consumed unchanged. No
staging/commit, production layout or demo source change.
