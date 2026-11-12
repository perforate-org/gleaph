# On-chain tokenizer benchmark — initial measurements (2026-11-12)

Question: can the inference canister own tokenization (ADR 0111 follow-up), so the Router can
accept question text and keep tokenization off the client? This is the first measurement step;
the probe canister itself belongs to the glev repository.

## Asset facts (pinned `glev-0.1.0` tokenizer)

- 34,363,285 bytes of HuggingFace `tokenizer.json`.
- Model: BPE, vocabulary 256,000, merges 580,604, `added_tokens` 249.
- Normalizer: Replace `" "` → `"▁"`; pre-tokenizer: Metaspace (`prepend_scheme=always`, `split=true`).
- The asset carries `truncation.max_length = 48`; the SDK deliberately calls
  `with_truncation(None)`, so an on-chain runtime must not inherit that limit.

Estimated compact binary (no JSON):

| Part | Bytes |
| --- | --- |
| vocabulary strings | 1,931,902 |
| token offsets (u32 × 256,000) | 1,024,000 |
| merge pair table (8 B × 580,604) | 4,644,832 |
| merged-token ids (4 B × 580,604) | 2,322,416 |
| added tokens | ~4,000 |
| **total** | **~9.9 MB** |

## Initial measurements

- **Stock `tokenizers` crate is not IC-wasm compatible as-is.** A wasm32 build pulls
  `getrandom 0.3` through `ahash`'s `runtime-rng` and fails with the wasm_js requirement. The
  crate's `unstable_wasm` feature binds JS `crypto`, which an IC canister does not have.
- Native (x86) reference with `tokenizers 0.22`:
  - load + parse the 34 MB JSON: **571 ms**
  - encode representative questions (13–21 tokens): **50–150 µs**
  - "GraphRAGの論文を引用している文書を深さ3まで教えてください。" → 16 tokens.
- Model forward is ~18.5B instructions (inference-canister notes, 40B update budget). A
  tokenization target of < 2B instructions (< ~10% of one forward) would be acceptable; the JSON
  load must fit one update (≤40B) or be an install-time/upgrade step.

These are native numbers, not IC instruction counts. The probe replaces them.

## Probe plan (in glev)

1. **Exporter** (Python): `tokenizer.json` → compact blob (vocab + offsets + merge table + added
   tokens), checksummed like the pack/tokenizer release assets.
2. **Probe canister**: chunk upload into stable memory, `finish` builds heap tables,
   `tokenize(text) -> ids` with `ic_cdk::api::performance_counter(0)` deltas around finish and
   encode, plus heap/stable sizes. Implement BPE (Metaspace + merges + byte fallback + added
   tokens) with fixed hashers (no getrandom).
3. **Correctness gate**: compare ids against the reference `tokenizers` output on short/long
   English, Japanese, URLs, code and whitespace edge cases. No benchmark number is meaningful
   until ids match.
4. **Measurements**: install/load instructions, per-question encode instructions (≤32, 33–96,
   >96 tokens, CJK), wasm size, heap and stable bytes.
5. **Go/no-go**: encode < ~2B instructions; load within one update (or an explicit chunked load
   path); wasm size within the canister code limit and heap headroom beside the 328 MB model;
   exact ids on the golden set. If the custom runtime passes, a follow-up ADR moves
   `route_and_extract_text` into the inference canister and the Router forwards text.

## Status

- [x] Asset analysis and native reference measurement
- [x] Stock-crate wasm feasibility check (rejected)
- [ ] Compact exporter
- [ ] Probe canister + correctness gate
- [ ] Instruction benchmark and go/no-go

Note: the probe must be committed in glev (model runtime ownership). Writing to `~/dev/glev`
was blocked by the session sandbox in this round, so only this investigation record landed.
