# 0108. Glev model repository boundary and naming

Date: 2026-11-11
Status: proposed (naming decided 2026-11-11; repository split not yet implemented)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 23:30:00 UTC +0900

Related: [0107](0107-select-from-graph-match-list.md),
`design/investigations/2026-11-10-canister-inference-optimization-research.md`,
`sdk/client/rust/src/routing/`, `crates/cli/src/ask.rs`, `crates/cli/src/routing_log.rs`,
`demo/knowledge/eval/routing/`.

## Context

The natural-language routing program produced a distilled, INT8-quantized, question-only
routing/slot model. The working names are `Laya` (upstream project), `laya-infer` (Rust runtime),
`laya-infer-canister` (inference canister), and the separate `gleaph-kernels/` workspace.

- Upstream **Laya** is an existing Apache-2.0 project (PyPI `laya`, HF
  `convaiinnovations/laya`): a multilingual, non-autoregressive **System 1 decision engine** and the
  origin of the Jev-compatible `/v1/systemone` protocol used by `crates/cli/src/decision.rs`.
  Gleaph's model is a derivative (distilled, slot-extended, quantized), not the upstream project.
- `crates/laya-infer` has mandatory path dependencies on `gleaph-kernels/`, a nested git repository
  (no remote) that the parent `.gitignore` excludes. The runtime therefore cannot build from a
  clean main-repository checkout, and the main workspace would carry the heavy native/wasm kernel
  build and the training toolchain.
- The inference canister is a **Gleaph ICP service boundary**: it owns chunked install, stable-memory
  decision caching, the Candid API, upgrade persistence and instruction budgets. Its client is the
  Gleaph SDK (`sdk/client/rust/src/routing`), which is consumed by `gleaph ask`.
- The wire types are duplicated: `RouteAndExtract`/`SlotSpan` exist both in the canister and in the
  SDK wire module. The pack config is duplicated between the Python exporter and the Rust
  `ModelConfig`.

Artifacts are large and have a different release cadence from the database: checkpoints (672 MB for
the current slot model), INT8 packs (8.6 MB), tokenizer (33 MB), and 41 GB of training/experiment
outputs under `target/laya-routing`.

## Problem

1. **Naming**: `Laya` names the upstream project, so it must not be the Gleaph model name.
2. **Build/release boundary**: the model stack (torch Python tooling, kernels, canbench, training
   outputs) is heavy and experimental and has a different cadence from the database. Keeping it in
   the main repository taxes every database build and makes a clean checkout impossible.
3. **Contract drift**: the duplicated wire and pack schemas will diverge unless one repository owns
   each.

## Existing Architecture Assessment

The main repository's existing concepts (Router, Graph, Index, Text, Vector, Prepared) own query
planning/execution and product APIs; none owns ML inference, model artifacts, or training. The
inference canister, however, *is* a product service (like Router or Graph): it is deployed and
upgraded by Gleaph, speaks Candid to the Gleaph SDK, and must stay next to its client. The model
runtime behind it is a library, not a service.

`crates/cli/src/decision.rs` is a thin HTTP client for an external decision model; it is a
product-facing client, not model implementation. `crates/cli/src/embedding.rs` is a deterministic
demo embedding recipe, not a model. The Japanese analyzer crates and vector index are product
features and stay.

## Alternatives

1. **Move the canister and client to `glev` as well.** Benefits: one model-side repository.
   Drawbacks: the canister is a Gleaph service (install/upgrade/stable memory/Candid/instruction
   budgets); moving it separates the service from its SDK consumer and its acceptance gates, and
   forces Gleaph builds to fetch a wasm artifact for every integration test.
2. **Keep everything in the main repository** (vendor kernels, commit heavy training code).
   Benefits: one CI. Drawbacks: database CI pays the model toolchain cost; the experimental kernels
   and training outputs stay entangled with the database; the naming collision remains.
3. **Chosen: split only the model library and training assets; keep the service and its client in
   Gleaph.** `glev` owns kernels, the model runtime library, the pack format/exporter, training and
   experiments, and model release artifacts. Gleaph owns the canister, the wire contract, the SDK
   routing client, the CLI and the acceptance gates, and consumes `glev-infer` by a pinned git
   revision.
4. **Submodule only `gleaph-kernels`.** Rejected: the nested repository has no remote and the heavy
   build would still run in the main workspace.

## Decision

### Naming

| Item | Name | Home |
| --- | --- | --- |
| Model family | **Glev** (GLEV, グレヴ) | docs/model cards |
| Repository | `glev`, hosted at `github.com/perforate-org/glev` | — |
| Runtime library | `glev-infer` | `glev` |
| Kernel workspace | `glev-kernels` | `glev` |
| Pack exporter / training / experiments | tooling under `glev` | `glev` |
| Inference canister | `gleaph-routing-canister` | Gleaph |
| Wire contract crate | `gleaph-routing-wire` | Gleaph |
| Client | `sdk/client/rust/src/routing` (routing feature) | Gleaph |
| Artifacts | `glev-<version>-<variant>-<quant>.pack`, `glev-tokenizer.json` | `glev` releases |

Glev is documented as a Gleaph-specific derivative of Laya (Apache-2.0). Jev is an upstream
protocol; Glev is Jev-compatible but is not the upstream implementation, and documentation must not
imply endorsement.

### Boundary

**Moves to `glev`:** `gleaph-kernels` source, the model runtime (renamed `glev-infer`), pack format
and export tooling, Python training/distillation/experiment scripts, canbench measurements, and
model artifacts (packs, tokenizers, checkpoints) as release assets.

**Stays in Gleaph:** `gleaph-routing-canister` (formerly `laya-infer-canister`), the
`gleaph-routing-wire` contract crate, the SDK routing client, the CLI `ask` command and opt-in
routing log, the routing acceptance corpora/gates
(`demo/knowledge/eval/routing`, `demo/knowledge/scripts/routing-bench`), the HTTP decision-model
client and demo embedding recipe, the Japanese analyzer and vector-index crates, and design records.

### Dependency direction

- Gleaph → glev is **one-way**: `gleaph-routing-canister` depends on
  `glev-infer = { git = "https://github.com/perforate-org/glev", rev = "<pinned>" }`.
  A local `[patch]` may point the same dependency at a sibling checkout for development; the
  committed manifest uses the git revision.
- `glev` has **no dependency on Gleaph**. The wire types and Candid API are Gleaph's, so they cannot
  live in `glev`.

### Contract ownership

- `gleaph-routing-wire` (Gleaph) owns the Candid request/response types
  (`RouteAndExtract`, `SlotSpan { tag, start, end, score }`), the wire version constant, and the
  canister's Candid surface. Both the canister and the SDK depend on it; the duplicate definitions
  are deleted.
- `glev-infer` owns the pack format and its config schema (`route_options`, `slot_tag_names`,
  window/short-window settings, `slot_top2_threshold`, special token ids) because it is the only
  reader. The exporter lives in `glev`, validates its output against the schema, and writes a
  format/schema version.
- The canister exposes `pack_info()` with the pack format/schema version from `glev-infer` and the
  wire version from `gleaph-routing-wire`; the SDK validates the versions before routing.

### Artifact pinning and release

- A `glev` release is an atomic compatible set: runtime git revision, INT8 pack, tokenizer and
  checksums. The runtime is source-pinned by git revision; the data artifacts are pinned by SHA-256
  in a checked-in manifest.
- Gleaph unit tests use fixtures. PocketIC integration and the routing acceptance gates download the
  pinned pack/tokenizer and **fail** if they are unavailable or their checksum does not match; they
  do not silently skip.
- The model artifacts stay out of git (size); they are release assets.

### Licensing and attribution

Laya is Apache-2.0. The `glev` repository must carry the upstream NOTICE/attribution and a model card
documenting the derivation, training data provenance, and quantization. The ModernBERT base
checkpoint license and redistribution terms must be verified before any public model release.

## Consequences

Positive:

- The database repository keeps a clean checkout; the model toolchain and training outputs live
  where they belong.
- The inference canister stays beside its SDK client and acceptance gates, so the Candid/wire
  contract has one owner in the repository that deploys it.
- The wire duplication is removed by `gleaph-routing-wire`.
- Model releases can move independently (git revision + artifact checksums).
- The name no longer conflates the Gleaph model with the upstream Laya project.

## Trade-offs

- Gleaph builds compile `glev-infer` from a pinned git revision; a broken revision pin is a build
  failure, which is desirable but couples the two repositories at the source level.
- A second repository and CI system, plus artifact hosting (GitHub Releases or equivalent), are
  required.
- A local `[patch]` is needed for side-by-side development; the committed manifests must be clean of
  it.
- Migration work: history-preserving move, crate renames, artifact upload, CI wiring, and license
  review. Model artifacts remain outside git.

## Migration

1. Finish the scoped Gleaph commits (SDK, CLI, evaluation assets, design records).
2. Create `github.com/perforate-org/glev` and move `gleaph-kernels` + the runtime there with
   history; rename `laya-infer` to `glev-infer` and `gleaph-kernels-*` to `glev-kernels-*`.
3. In Gleaph, add `gleaph-routing-wire`; make the canister and SDK use it; rename
   `laya-infer-canister` to `gleaph-routing-canister`; point the canister at the pinned `glev-infer`
   git revision.
4. Publish the runtime revision and the atomic artifact set (pack, tokenizer, checksums); add the
   Gleaph artifact manifest and CI download/verification.
5. Move training and experimental scripts to `glev`; keep the acceptance harness and corpora in
   Gleaph.
6. Update the ADR index, the investigations record (append a naming/boundary note; records stay
   append-only), SDK/CLI documentation, and CI documentation.

## Design Documentation Impact

- This ADR and the ADR index.
- `design/investigations/2026-11-10-canister-inference-optimization-research.md`: append the naming
  and boundary decision (do not rewrite prior records).
- SDK/CLI documentation for the pinned revision and artifact versions.
- A `glev` README/model card documenting the Laya derivation and licenses.

## Review axes

- **Encapsulation**: the model runtime is a library behind a pinned revision; the service and its
  Candid contract stay with the service owner.
- **Separation of concerns**: ML lifecycle (training, quantization, kernels, canbench) is separated
  from database query planning/execution and from ICP deployment concerns.
- **Invariants**: `gleaph-routing-wire` owns the wire; `glev-infer` owns the pack schema; the
  canister exposes both versions; artifacts are pinned by checksum.
- **Consistency**: one wire owner, one pack-schema owner, one pinned revision, one atomic artifact
  set.
- **Fitness for purpose**: the experimental model stack stops taxing the database repository while
  the service, its client and the product gates remain where regressions must be caught.

Recommendation: **approve with changes** — the split is justified, provided `gleaph-routing-wire`,
the canister rename, the pinned git revision, the artifact manifest, and the license review land in
the first implementation slice.
