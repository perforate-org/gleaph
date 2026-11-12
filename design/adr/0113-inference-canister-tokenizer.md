# 0113. Move tokenization into the inference canister

Date: 2026-11-12
Status: accepted (2026-11-12; implemented and verified). Amended by ADR 0114: the tokenizer remains for exact token-count quota, but the neural text route is removed in favor of `route_general`.
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 14:20:00 UTC +0900

## Context

ADR 0111 makes the Router own the deployment inference canister and proxy `route_and_extract`.
The SDK still tokenises locally with a 34.4 MB `tokenizer.json`; every client must ship or fetch
that asset, keep it in sync with the model, and apply truncation itself.

The on-chain probe (`glev: experiments/tokenizer-onchain-benchmark.md`, 2026-11-12) measured the
stock `tokenizers 0.22` crate on the exact pinned asset:

- install/update load (34 MB JSON parse + persist): **9.85B instructions** (fits one 40B update);
- `post_upgrade` reload: **9.84B** (300B install/upgrade budget);
- encode: **0.23–1.03M instructions** per question. That is ~0.2–1% of the shipped pack's
  ~0.1B-instruction <=32-token single-window forward and under ~0.05% of the estimated ~2.6B
  forward at the 256-token cap (W20/10 windows). The ~18.5B figure that earlier drafts cited was
  the 14-layer candidate at 141 tokens, not the shipped 1-layer h640 pack;
- ids exactly match the native reference, including after the upgrade reload;
- probe wasm 3.6 MB against a 12 MiB code-section limit.

The stock crate required only a deterministic getrandom custom backend; a purpose-built compact
BPE was not necessary.

## Problem

- Tokenization is model-side policy: truncation, the 256-token cap, CJK handling and the exact
  vocab must match the pack. Keeping it per client duplicates model knowledge and drifts.
- The Router cannot accept question text, cannot count tokens for quota, and cannot return slot
  spans that the client can bind without the tokenizer.
- A 34 MB client asset is a deployment liability for browser and mobile clients.

## Existing architecture assessment

The inference canister already owns the pack in chunks with controller-only admin methods and
reports `wire_version()`/`weights_installed()`. The tokenizer is the same kind of model-side
asset: install-time chunks, a fingerprint, and a runtime that the Router proxies. The SDK's
`RoutingTransport` abstraction already separates transport from policy. No new subsystem is
needed; the inference canister is the correct owner.

## Alternatives

- **A. Keep client tokenization.** Rejected by the stated direction: model knowledge stays on
  every client and the Router cannot accept text or price quota.
- **B. Stock `tokenizers` inside the inference canister (chosen).** Proven by the probe; keeps
  exact ids and the existing pack/route semantics.
- **C. Compact BPE runtime.** Deferred: ~9.9 MB versus 34 MB saves install transfer, but adds a
  second tokenizer implementation that must be kept bit-exact. Revisit if transfer cost matters.
- **D. Separate tokenizer canister.** Rejected: an extra hop on every question for a 1M-instruction
  operation, and another canister to provision.
- **E. Router tokenizes.** Rejected: Router is orchestration, not model runtime; it would need the
  same 34 MB asset and wasm code without owning the model.

## Decision

1. **The inference canister owns the tokenizer asset.** Controller-only
   `admin_install_tokenizer_chunk(offset, bytes)` / `admin_install_tokenizer_finish()` write the
   JSON to stable memory and parse it once (9.85B, one update). `post_upgrade` restores the stable
   bytes and re-parses (9.84B). The parse uses the stock `tokenizers` crate with a deterministic
   getrandom backend; the asset's embedded 48-token truncation is ignored, and the canister applies
   the model cap of 256 question tokens explicitly.
2. **The canister exposes text routing.** New update
   `route_and_extract_text(text) -> Result<TextRouteAndExtract, String>`:
   - tokenise, truncate to 256 tokens and run the existing forward;
   - return probabilities, the route order, `token_count`, `cjk_fraction`, and slot spans with
     **UTF-8 byte offsets** so a client without a tokenizer can bind values;
   - keep `route_and_extract(token_ids)` for tests, operators and existing direct users.
   New update `tokenizer_token_count(text) -> Result<u32, String>` returns the token count and CJK
   fraction for quota and validation without running the model; new query
   `tokenizer_fingerprint()` reports the installed asset SHA-256 and byte length.
3. **The Router accepts text.** New ingress
   `route_and_extract_text(graph_name, text)` resolves the graph, checks tenancy and the per-graph
   flag, rejects text above a bounded size (16 KiB), calls `tokenizer_token_count`, rejects
   > 256 tokens as `InvalidArgument`, charges the ADR 0112 quota with the exact unit count, then
   forwards to `route_and_extract_text` and returns the text result. `route_options` is unchanged.
   Tokenizer chunks are forwarded through the Router the same way as pack chunks, with the same
   1.9 MB chunk and controller-only rules.
4. **The SDK drops the local tokenizer on the Router path.** A new
   `TextRoutingTransport::route_and_extract_text(text)` returns the text result; `RoutingClient`
   gains `route_text(question)` that applies the abstain policy from the returned `token_count` /
   `cjk_fraction` and binds slots from byte spans. The existing tokenizer path remains for the
   direct `CanisterTransport` (tests and operators) but is no longer required by applications.
5. **The CLI uses text routing.** With `--graph`, `gleaph ask` no longer requires `--tokenizer`;
   the tokenizer asset is only needed for the legacy direct-canister path.
6. **Tokeniser distribution stays a pinned Glev release asset.** The existing
   `glev-0.1.0-tokenizer.json` SHA-256 is the source of truth; the Router forwards it to the
   canister in chunks. The release manifest remains wasm-only; no new artifact kind is introduced.

## Consequences

- Clients send text and no longer carry a 34 MB tokenizer; tokenisation policy lives beside the
  model pack and can never drift from it.
- Every question costs one extra small inter-canister call (`tokenizer_token_count`, ~1M
  instructions) before the model call. This is negligible against the forward and makes quota
  exact.
- The inference canister gains 34 MB of stable memory and a tokenizer heap structure beside the
  328 MB model; the probe must be extended to measure the residual heap and the stable-read path
  before release.
- Text results carry byte-offset spans, so SDK slot binding changes from token offsets to byte
  offsets.

## Trade-offs

- Slot spans in bytes must map exactly to the same substrings as token spans; the canister
  converts token offsets to character boundaries using the tokenizer offsets (no re-tokenisation
  in clients).
- `route_and_extract(token_ids)` and `route_and_extract_text` can diverge if one path changes
  without the other; a shared forward helper and a cross-check test keep them equivalent.
- The 16 KiB Router text cap and 256-token model cap are two limits; the first bounds ingress and
  quota work, the second is the model contract.
- Tokenizer chunks are one-time transfer; a fresh install requires 34 MB of chunk uploads after
  the wasm install.

## Migration

Pre-production fresh-install semantics: the inference canister gains stable regions for the
tokenizer bytes and fingerprint, and the install flow adds a tokenizer-upload step. No migration
from existing canisters; deployments are reinstalled with the new release and assets.

## Design Documentation Impact

- ADR 0111: the tokenizer follow-up is resolved by this ADR; ADR 0112 quota units use the exact
  token count returned by `tokenizer_token_count`.
- `design/storage/stable-memory-inventory.md`: inference canister stable regions for tokenizer
  bytes/fingerprint.
- SDK/CLI docs: text routing replaces the tokenizer requirement on the Router path.

## Implementation slices

1. Inference canister: tokenizer stable storage, chunk install/finish, fingerprint, token count,
   text route with byte-offset spans, upgrade reload, unit tests (glev). **Landed 2026-11-12**
   (the canister lives in Gleaph; `tokenizer_text` PocketIC test).
2. Router: text ingress, text cap, token-count call, quota, tokenizer chunk forwarding, PocketIC
   E2E with a provisioned canister and the pinned tokenizer (Gleaph). **Landed 2026-11-12** in
   `inference_router_proxy` (chunk forwarding, fingerprint check, text route, exact quota).
3. SDK/CLI: `TextRoutingTransport`, `RoutingClient::route_text`, byte-span slot binding, CLI
   `--graph` without `--tokenizer`, docs. **Landed 2026-11-12**: `RouterTransport` text method,
   `RoutingClient::new_text_only`/`route_text`/`bind_text_slots_with_fallback`, CLI plan/execute
   and routing log use the text path.
4. Install tooling: fetch/verify tokenizer, upload chunks, verify fingerprint and weights before
   enabling a graph. **Landed 2026-11-12**: `gleaph-inference-install` (operator binary) uploads
   the pack, verifies weights via the Router finish call, uploads the tokenizer, verifies the
   fingerprint, then optionally enables the graph.

## Review axes

- **Encapsulation**: model assets and tokenization policy stay behind the inference canister; the
  Router orchestrates and enforces access/quota; clients send text.
- **Separation of concerns**: provisioning, authorization, quota, tokenization and model forward
  remain owned by their existing domains.
- **Invariants**: the tokenizer fingerprint is pinned and checked; slot byte offsets are derived
  from the tokenizer offsets; text and token routes share one forward path.
- **Consistency**: one tokenizer per deployment canister, one fingerprint, one truncation rule.
- **Fitness for purpose**: removes a 34 MB client asset and keeps per-question cost negligible
  relative to the model.

## Acceptance evidence (2026-11-12)

- Benchmark: PocketIC probe measured install load **9,852,047,100** instructions, upgrade reload
  **9,840,391,974**, encode **0.23–1.03M** per question, wasm **3.6 MB**, and ids identical to the
  native reference (`glev: experiments/tokenizer-onchain-benchmark.md`).
- Inference canister: `tokenizer_text` PocketIC test installs the 34 MB tokenizer in chunks,
  checks `tokenizer_token_count` (16 for the Japanese sample), routes text with byte-offset spans,
  upgrades the canister, and reloads the tokenizer from stable memory.
- Router: `inference_router_proxy` PocketIC test forwards the pinned tokenizer through the Router,
  verifies the fingerprint, routes a Japanese question, and checks token count, CJK fraction and
  span bounds; Router unit tests 1156 pass.
- SDK/CLI: 29 routing tests and 201 CLI tests pass; `RouterTransport` text routing,
  `RoutingClient::route_text`, byte-span binding and the CLI `--graph` path are implemented.
- Install tooling: `gleaph-inference-install` verifies local SHA-256 values, uploads pack and
  tokenizer chunks, confirms weights and fingerprint, then optionally enables the graph; chunk
  planning and the digest helper are unit-tested.

## Open questions

- Should the Router cache the last `tokenizer_token_count` per text hash to avoid the extra call
  for retries? Deferred: the count call is cheap and cache invalidation is not worth it yet.
- The exact 16 KiB text cap should be confirmed against the largest real long-context questions;
  it is an ingress bound, not a model bound.
