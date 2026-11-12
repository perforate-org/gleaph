# Correction: the 18.5B forward figure and the shipped pack (2026-11-12)

Trigger: a review challenge to "256 tokens ≈ 18.5B instructions" based on a 32-token measurement
of ~100M. The challenge is correct: 18.5B is not the shipped pack and not a 256-token measurement.

## Where 18.5B came from

- `2026-11-10-canister-inference-optimization-research.md:104` estimates **14 layers × 1.32B
  instructions/layer ≈ 18.5B**. The 1.32B figure is a measured one-layer kernel at **L=141**
  tokens, so the product describes a 14-layer *candidate* at a 141-token forward.
- `2026-11-10-model20-canister-inference-feasibility.md:162` repeats the same 14-layer estimate.
- The full 14-layer forward was later measured with canbench at **22.01B** (L=141, token-at-a-time)
  and **17.37B** after the tiled kernels (`2026-11-10-canister-inference-optimization-research.md`,
  "Head INT8 + full-model measurement" and "Tiled forward integrated").
- Later compact students replaced that candidate: the adoption notes record 3.81B, 3.10B, 2.6B,
  660M and 209M configurations. The canister doc and ADR 0113 carried the old 18.5B figure without
  re-checking it against the pack that was actually shipped.

## What is actually shipped

Read from `target/glev-artifacts/glev-0.1.0-h640-slot-int8.pack` (pack `__config__`):

| Field | Value |
| --- | --- |
| layers | **1** |
| hidden / ffn | 640 / 768 |
| heads / head_dim | 10 / 64 |
| head_layers | **0** |
| window_tokens / window_stride | 20 / 10 |
| short_window_tokens | 32 |
| route options | 5 intents + `none-of-these` |
| pack bytes | 9,049,654 |

## Measured / estimated cost

- **96-token question = 9 windows ≈ 953M instructions** (measured; adoption notes
  `2026-11-10-canister-inference-optimization-research.md:1115` and `:1439`). This is ~106M per
  window including the `[bos] + question window + 6 <mask>` prompt.
- **≤32 tokens = one window ≈ 0.1B instructions**, consistent with the 32-token ~100M figure.
- **256 tokens = 25 windows** (`ceil((256 - 20) / 10) + 1`), so the forward is **≈2.5–2.6B
  instructions**, not 18.5B. This is an extrapolation from the measured 96-token/9-window cost; a
  direct 256-token canbench run is not currently available (the glev canbench harness is stale and
  fails to compile against the current `ModelConfig`).
- A linear single-pass model at 100M/32 tokens would be ~800M at 256 tokens. The shipped runtime
  deliberately uses overlapping W20/10 windows, so it recomputes boundary tokens and lands ~3×
  above that naive linear figure, still far below 18.5B.

## Consequences

- All cost conclusions hold: the 256-token cap is comfortably under the 40B update budget; a
  windowed 256-token forward is ~6% of the budget, not ~46%.
- Tokenization (0.23–1.03M) is **~0.2–1% of the short-path forward**, not four orders of magnitude
  below 18.5B. It remains negligible in absolute terms, but the ratio to the ~18.5B figure was
  wrong by roughly two orders of magnitude.
- The old 328 MB heap note also described the 14-layer candidate; the shipped pack is ~9 MB on
  the wire, not 328 MB.

## Files corrected

- `crates/gleaph-routing-canister/src/lib.rs` module docs.
- `design/adr/0113-inference-canister-tokenizer.md` benchmark bullet.
- `design/investigations/2026-11-12-per-graph-routing-customization.md` IC-envelope note.
- glev `experiments/tokenizer-onchain-benchmark.md` comparison and consequence.
- The dated records that contain the original estimate remain unchanged (append-only); this record
  is the correction they are read against.

## Open item

Run a direct canbench/PocketIC measurement of `forward_question` for a 256-token question and
replace the 2.5–2.6B extrapolation with a measured value. The current glev canbench harness needs
repair first (it predates the current `ModelConfig` fields).
