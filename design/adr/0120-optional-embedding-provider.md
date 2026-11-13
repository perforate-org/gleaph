# 0120. Embeddings are an optional SDK provider

Date: 2026-11-13
Status: accepted (2026-11-13), implemented as the embedding contract and provider boundary.
Last revised: 2026-11-13
Anchor timestamp: 2026-11-13 15:00:00 UTC +0900

## Context

Semantic search needs a text-embedding model. The movie demo currently runs
`intfloat/multilingual-e5-small` in a client-side Python sidecar; the Router and Graph only store
and search the resulting bytes. A browser experience would naturally move the model closer to the
user, but shipping every model with the SDK would make the core client heavy:

- model assets are large (tens to hundreds of MiB);
- model downloads are a product decision (size, license, privacy, offline use);
- the model lifecycle differs from the Router/canister lifecycle;
- applications may use a hosted embedding API, a local sidecar, ONNX/WebGPU, or no embeddings at
  all;
- an implicit download at client construction is surprising and untestable.

The core SDK must stay model-free and must not perform network fetches just by being imported.

## Decision

1. **The core SDK never bundles, downloads, or initialises an embedding model.**
   `import @gleaph/sdk` / constructing `GleaphClient` performs no model I/O. Rust default features
   pull in no ONNX/Candle/Transformers dependency.
2. **Embedding is an explicit provider boundary.** The SDK defines an `EmbeddingContract` and an
   `EmbeddingProvider` trait/interface. Implementations live in separate packages or application
   code:
   - HTTP sidecar / hosted embedding API;
   - browser Transformers.js / ONNX Runtime Web;
   - native Rust ONNX/fastembed;
   - application-specific provider.
3. **Loading is explicit.** A provider exposes a separate `load()`/initialisation step. A browser
   provider must report download progress and size and respect an explicit user action; a cached
   model skips the download.
4. **The contract is mandatory.** Every provider returns the model identity and preprocessing:
   model, revision, dimensions, query/passage prefixes, pooling, normalization, similarity and
   maximum input length. The SDK validates vectors against it before passing bytes to the Router.
   Dimension equality alone is not sufficient: E5 query/passage spaces and different model
   revisions are incompatible.
5. **No implicit semantic search in `ask`.** `ask`/`interpret` remain model-free. Semantic search
   is an explicit composition:
   `provider.embed(text)` → `prepared_query("films-by-embedding", { embedding })`.
6. **Model assets are separate from the SDK package.** Browser model files are fetched from a
   configured URL, hash-checked, and cached (IndexedDB/Cache Storage). A model asset is not an npm
   dependency and is not copied into the SDK bundle.
7. **Missing provider is a normal state.** The SDK still supports Router/GQL/prepared operations.
   Only embedding-dependent calls fail with a typed "provider not configured/unavailable" error.

## Consequences

- Core SDK install size and cold start stay independent of the selected model.
- Applications can switch between E5-small, Bekko, a hosted API, or a sidecar without changing the
  Router contract.
- The embedding contract becomes a compatibility gate; model/index changes require a new index or
  explicit re-embedding.
- A model download is visible, cancellable and cacheable rather than hidden inside client
  construction.
- `films-by-embedding` remains a bytes-level operation; the SDK does not own the model.

## Evidence

- `sdk/client/rust/src/embedding.rs`: contract, provider trait and byte encoding, no model deps.
- `sdk/client/js/src/embedding.ts`: the same boundary for the JS SDK.
- `demo/movies/scripts/embedding_contract.py` and `seeds/embeddings.provenance.json`: one
  executable contract for the movie demo; all embedding scripts fail on a mismatch.

## Amendment 2026-11-14

ADR 0122 selects Bekko a8m for the movie demo. The E5-small sidecar reference in the Context is the
historical starting point, not the current committed model. The provider boundary in this ADR is
unchanged.
