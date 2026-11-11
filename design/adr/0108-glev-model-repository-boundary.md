# 0108. Glev model repository boundary and naming

Date: 2026-11-11
Status: proposed (naming decided 2026-11-11; repository split not yet implemented)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 23:00:00 UTC +0900

Related: [0107](0107-select-from-graph-match-list.md),
`design/investigations/2026-11-10-canister-inference-optimization-research.md`,
`sdk/client/rust/src/routing/`, `crates/cli/src/ask.rs`, `crates/cli/src/routing_log.rs`,
`demo/knowledge/eval/routing/`.

## Context

The natural-language routing program produced a distilled, INT8-quantized, question-only
routing/slot model plus a canister runtime. The working names are `Laya` (upstream project),
`laya-infer`, `laya-infer-canister`, and the separate `gleaph-kernels/` workspace.

- Upstream **Laya** is an existing Apache-2.0 project (PyPI `laya`, HF
  `convaiinnovations/laya`): a multilingual, non-autoregressive **System 1 decision engine** and the
  origin of the Jev-compatible `/v1/systemone` protocol used by `crates/cli/src/decision.rs`.
  Gleaph's model is a derivative (distilled/slot-extended/quantized), not the upstream project.
- `crates/laya-infer` has mandatory path dependencies on `gleaph-kernels/`, a nested git repository
  (no remote) that the parent `.gitignore` excludes. Those crates therefore cannot be committed to
  the main repository and still build from a clean checkout.
- The main repository holds the product consumers: `sdk/client/rust/src/routing` (tokenizer, wire
  client, slot binding, abstain policy), `crates/cli/src/ask.rs`, the opt-in routing log, and the
  routing acceptance corpora/gates.
- The wire and pack schemas are currently duplicated: `RouteAndExtract`/`SlotSpan` exist both in the
  canister and in the SDK wire module, and the pack config exists both in the Python pack exporter
  and in the Rust `ModelConfig`.

Artifacts are large and have a different release cadence from the database: checkpoints (672 MB for
the current slot model), INT8 packs (8.6 MB), tokenizer (33 MB), and 41 GB of training/experiment
outputs under `target/laya-routing`.

## Problem

1. **Naming**: `Laya` names the upstream project, so it must not be the Gleaph model name.
2. **Build boundary**: the model stack (torch Python tooling, tokenizers, canbench, PocketIC, Clang
   Wasm kernels) is heavy and experimental; a clean main-repository checkout cannot build it because
   the kernels are an ignored nested repository, and main CI would pay the cost for a feature most
   database work does not exercise.
3. **Contract drift**: once the model lives in another repository, the duplicated wire and pack
   schemas will diverge unless one repository owns them.

## Existing Architecture Assessment

The main repository's existing concepts (Router, Graph, Index, Text, Vector, Prepared) own query
planning/execution and product APIs; none owns ML inference, model artifacts, or training. There is
no existing abstraction that can absorb this stack without mixing database concerns with model
lifecycle. The current path dependency on `gleaph-kernels/` is incidental — it happened because the
kernel experiments started before the boundary existed — not a designed encapsulation.

`crates/cli/src/decision.rs` is a thin HTTP client for an external decision model; it is a
product-facing client, not model implementation. `crates/cli/src/embedding.rs` is a deterministic
demo embedding recipe, not a model. The Japanese analyzer crates and vector index are product
features and stay.

## Alternatives

1. **Keep everything in the main repository** (vendor kernels, commit artifacts elsewhere).
   Benefits: one CI and one version. Drawbacks: main CI gains heavy/native/experimental
   dependencies; model release cadence couples to database releases; 41 GB of training outputs and
   large model artifacts need a home; the naming collision remains.
2. **Move everything, including the SDK routing client and CLI `ask`, to the new repository.**
   Benefits: a clean product/model split. Drawbacks: `gleaph ask` and the reception gates are
   product surfaces; moving them makes every product integration depend on the model repository and
   weakens the main repository's ability to test its CLI.
3. **Chosen: split the model implementation; keep the contract, client, CLI and acceptance gates in
   the main repository.** The new repository depends on the main repository's contract crate; the
   main repository depends only on pinned binary artifacts.
4. **Submodule only `gleaph-kernels`.** Rejected: the nested repository has no remote, so a
   submodule cannot be fetched; the heavy dependencies would still be built by the main workspace.

## Decision

### Naming

| Item | Name |
| --- | --- |
| Model family | **Glev** (GLEV, グレヴ) |
| Repository | `glev`, hosted under the Gleaph GitHub organisation (`github.com/<gleaph-org>/glev`; `github.com/glev` is an unrelated GitHub user) |
| Runtime crate | `glev-infer` |
| Inference canister | `glev-infer-canister` |
| Kernel workspace | `glev-kernels` |
| Contract crate (main repository) | `glev-wire` |
| Artifacts | `glev-<version>-<variant>-<quant>.pack`, `glev-tokenizer.json`, `glev-infer-canister-<version>.wasm` |

Glev is documented as a Gleaph-specific derivative of Laya (Apache-2.0). Jev is an upstream
protocol; Glev is Jev-compatible but is not the upstream implementation, and documentation must not
imply endorsement.

### Boundary

**Moves to `glev`:** `gleaph-kernels` source, the inference runtime (renamed `glev-infer`), the
inference canister (renamed `glev-infer-canister`), pack export tooling, Python
training/distillation/experiment scripts, model packs/checkpoints/tokenizer as release assets, and
canbench measurements.

**Stays in the main repository:** the `glev-wire` contract crate; the SDK routing client
(`sdk/client/rust/src/routing`); the CLI `ask` command and opt-in routing log; the routing
acceptance corpora and gates (`demo/knowledge/eval/routing`, `demo/knowledge/scripts/routing-bench`);
the HTTP decision-model client and demo embedding recipe; the Japanese analyzer and vector-index
crates; and design records.

### Contract ownership

`glev-wire` is the single source of truth for:

- Candid request/response types (`RouteAndExtract`, `SlotSpan { tag, start, end, score }`) and their
  versioned wire compatibility;
- the pack config schema (JSON Schema plus the Rust deserialization type), including
  `route_options`, `slot_tag_names`, window/short-window settings, `slot_top2_threshold`, and the
  special token ids;
- format/schema/policy version constants and the compatibility handshake exposed by the canister.

The `glev` repository depends on `glev-wire` at a pinned main-repository commit (one-way). The main
repository must not depend on `glev` source. The Python pack exporter validates its output against
the JSON Schema and emits the schema/format version.

### Artifact pinning and release

- A `glev` release is an **atomic compatible set**: canister wasm, INT8 pack and tokenizer, with
  SHA-256 checksums and a release tag.
- The main repository pins one release in a checked-in artifact manifest (release tag, version,
  SHA-256 for wasm/pack/tokenizer).
- Unit tests use fixtures. PocketIC integration and the routing acceptance gates download the pinned
  artifacts and **fail** if they are unavailable or their checksum does not match; they do not
  silently skip.
- The canister exposes `pack_info()` with pack format, config schema and policy versions; the SDK
  validates the version before routing.

### Licensing and attribution

Laya is Apache-2.0. The `glev` repository must carry the upstream NOTICE/attribution and a model card
documenting the derivation, training data provenance, and quantization. The ModernBERT base
checkpoint license and redistribution terms must be verified before any public model release.

## Consequences

Positive:

- The main repository keeps a small, product-relevant dependency set and a clean checkout.
- Model releases can move independently of database releases.
- The name no longer conflates the Gleaph model with the upstream Laya project.
- Wire/config schema duplication is removed by `glev-wire`.
- The kernels are no longer an ignored, uncommittable dependency.

## Trade-offs

- Cross-repository version pinning: a pinned artifact set must be updated deliberately.
- A second CI system and artifact hosting (GitHub Releases or equivalent) are required.
- `glev-wire` is a new crate and a new contract surface that must be tested from both sides.
- Migration work: history-preserving move, crate renames, artifact upload, CI wiring, and license
  review. Model artifacts remain outside git (non-negotiable given their size).

## Migration

1. Finish the scoped main-repository commits (SDK, CLI, evaluation assets, design records).
2. Create the Gleaph organisation and the `glev` repository.
3. Move `gleaph-kernels` and the inference crates with history (git filter-repo; the nested kernel
   repository needs its history imported or documented as a separate fork), renaming
   `laya-*` to `glev-*`.
4. Add `glev-wire` in the main repository; make the canister and SDK use it; delete the duplicated
   types and schema definitions.
5. Remove the path dependencies and workspace members from the main repository; add the pinned
   artifact manifest and CI download/checksum step.
6. Move training and experimental scripts to `glev`; keep only the acceptance harness and corpora in
   the main repository.
7. Update the ADR index, the investigations record (append a naming note; records stay append-only),
   SDK/CLI documentation, and CI documentation.

## Design Documentation Impact

- This ADR and the ADR index.
- `design/investigations/2026-11-10-canister-inference-optimization-research.md`: append the naming
  decision and the boundary pointer (do not rewrite prior records).
- SDK and CLI documentation for the pinned artifact and version handshake.
- A `glev` repository README/model card documenting the Laya derivation and licenses.

## Review axes

- **Encapsulation**: model implementation moves behind a release artifact; the main repository keeps
  the client and contract.
- **Separation of concerns**: ML lifecycle (training, quantization, kernels, canbench) is separated
  from database query planning/execution.
- **Invariants**: `glev-wire` owns the wire and pack schema; an atomic release set plus checksums
  owns compatibility; the main repository never builds model source.
- **Consistency**: one schema owner, version handshake, and pinned artifact; no duplicated structs.
- **Fitness for purpose**: the heavy experimental stack stops taxing the database repository while
  product acceptance gates remain where product regressions must be caught.

Recommendation: **approve with changes** — the split is justified, provided `glev-wire`, the
artifact manifest, the license review, and the history-preserving kernel migration are part of the
first implementation slice.
