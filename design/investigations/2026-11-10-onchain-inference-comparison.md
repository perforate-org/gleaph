# On-chain inference comparison: our 14-layer decision model vs similar cases (2026-11-10)

Measured inputs: `crates/laya-infer` canbench (wasm32 +simd128), `model20-distil14b` INT8 pack,
end-to-end Rust evaluation. Estimated instructions are extrapolated from the measured per-layer
number (1.32B @ L=141) and have not yet been measured in a deployed canister.

## Reference cases

1. **IC-Imajev** ([forum](https://forum.dfinity.org/t/ic-imajev-running-a-4b-decision-model-in-a-canister/76058)):
   4B-parameter decision model, 32 layers, INT8 base + F32 LoRA, 4.70GB weight pack. Update path:
   84 tokens = 121.67B instructions / 27.3s across 4 internal worker update calls; query path is
   client-driven (32-63 query calls).
2. **llama.cpp on the IC** ([forum](https://forum.dfinity.org/t/llama-cpp-on-the-internet-computer/33471)):
   generative LLMs under the 40B update cap. Measured: Qwen2.5-0.5B q8_0 generates 12 tokens per
   update call; SmolLM2-135M q8_0 40 tokens; 1.78B q4 3 tokens; the 2.8x runtime upgrade later
   raised these.
3. **QLaya** ([GitHub](https://github.com/saipy10/QLaya)): the same Laya/ModernBERT family on a
   desktop x86 CPU (AVX-512 VNNI). 14-layer student + INT8: 244M params, 332MB, 78ms/decision.

## Comparison

| Metric | Ours (14L distil, INT8) | IC-Imajev (4B) | llama.cpp canister (0.5B q8) | QLaya 14L INT8 (CPU) |
| --- | --- | --- | --- | --- |
| Parameters | 283M (86M dense + embedding) | 4B | 0.5B | 244M |
| Weight pack | **285.7MB** (INT8 head included) | 4.70GB | 0.68GB | 332MB |
| Work unit | 1 decision (one 136-token forward) | 1 decision (84 tokens) | 12 generated tokens | 1 decision |
| Instructions | **16.02B measured** (tiled forward + 1-layer head, canbench) | **121.67B** | ~40B (12 tokens) | n/a (78ms) |
| Calls used | **1 update call** (est.; 40B cap) | 4 worker update calls | multiple update calls | local process |
| Latency | ~3.6s (est. at Imajev's 4.5B instr/s) | 27.3s | ~2 min per answer | 78ms |
| Kernel efficiency | ~1.1 FLOP/instr (measured gemv) | ~5.5 FLOP/instr | ~0.3 FLOP/instr | AVX-512 VNNI |
| Decision accuracy | blind 91.7%, regression 94.3% (routing) | JevBench #1 (different task) | n/a | 100% choice (their task) |

## Assessment

- **Model/architecture: strong.** A 14-layer student keeps gate quality (blind 91.7%, better than
  its 22-layer teacher) at 283M params and 328MB; that is ~14x smaller than the 4B IC-Imajev pack.
- **Per decision: 7x cheaper than IC-Imajev** (17.37B measured vs 121.67B) and it fits a
  **single update call** instead of 4 workers, with 60% of the 40B cap to spare. The 4-token tiled
  forward cut 22.01B -> 17.37B; a 1-layer decision head brings the canister pack to **16.02B**
  (blind 93.3%, hand 93.4%, all floors pass, pack 278.6MB).
- **Runtime efficiency: the weak spot.** Our kernels run at ~1.1 FLOP/instruction vs IC-Imajev's
  implied ~5.5 (5x gap) and only ~3-4x better than the older llama.cpp canister baseline (~0.3).
  At Imajev's efficiency the same work would be ~4B instructions — **inside the 5B query cap**.
  The head is now INT8 as well (the earlier F32 head was ~7.6B of the ~26B estimate), which is why
  the measured total is 22.01B.
- **Platform gap vs QLaya is expected, not a model flaw.** QLaya's 78ms uses AVX-512 VNNI (native
  8-bit dot units) on a desktop CPU; the canister is a single-threaded wasm runtime with 128-bit
  fixed SIMD. Normalizing by that hardware factor (~40-70x) puts our ~5-6s in the expected range.
- **Not yet measured in a deployed canister.** 22.01B is a canbench wasm32 measurement (same
  runtime class as the canister); latency is an instruction-rate extrapolation. The canister wasm
  builds (`756K`), but the PocketIC chunked-install/infer E2E has not run yet.

## Verdict

Against on-chain decision-model peers, ours is **competitive on cost-per-decision and memory and
better on call shape** (1 update call vs 4), while **behind the best runtime on kernel efficiency**.
Closing that efficiency gap (int8 head, blocking/unrolling, maybe 4-bit embeddings) is the path to a
single-query-call deployment; accepting the update path today is already viable.

## Why is IC-Imajev more efficient? (investigation, 2026-11-10)

Their published details (forum): INT8 base projections + F32 LoRA/readout; **integer dot products
in 256-element groups followed by scale application**; "Wasm SIMD allows weight reads to be shared
across multiple outputs"; fixed weights are **checked and rearranged before inference**; the runtime
splits/fuses work across workers including inside a layer, and passes intermediate state.

We tested the most promising explanation — sharing weight reads across multiple token rows:

| Kernel | instructions | per token |
| --- | ---: | ---: |
| one token, 4-row block | 2.43M | 2.43M |
| two tokens sharing weight loads | 4.27M | 2.14M (-12%) |

Token batching helps only ~12% in our Rust code because the extra accumulators spill and each token
still pays its own widen/dot work. Reaching their ~5.5 FLOP/instruction from our ~1.1 requires a
hand-tuned kernel, not loop reshaping: fixed Wasm SIMD (ICP supports deterministic SIMD128 only, no
relaxed SIMD per the official SIMD guide) makes ~0.5 MAC/instruction the practical ceiling for
straightforward i8 dot loops; their runtime evidently gets closer to 2.5 MAC/instruction via
pre-packed weight layouts, 256-element group scales, and tight hand-written Wasm SIMD loops.

Two caveats before treating 5.5 as ground truth: their FLOP estimate assumes 2*params*tokens for a
4B model whose text path and Gated DeltaNet layers may differ from that formula, and their per-worker
instruction totals were reported by them, not independently measured. A like-for-like check would
require their runtime or their benchmark harness.

Actionable gap for us: a hand-written/packed Wasm kernel (pre-transposed weights, fixed-size
unrolled chunks, grouped scales) is the only route to Imajev-level efficiency; Rust-level loop
tuning has plateaued (2.43M -> 2.34M in the best variant).

## Hand-written kernel experiments (2026-11-10)

Measured on `gemv(2304x768)` (1.77M MAC per token), wasm32 SIMD:

| Kernel | instructions/token | note |
| --- | ---: | --- |
| 4-row blocked | 2.43M | baseline |
| 4-row unrolled chunk loop | 2.34M | -3.7% |
| 2 tokens, 8-accumulator array | 2.14M | -12% |
| 2 tokens, 8 explicit accumulators | 2.07M | -15% |
| 8-row blocked | 4.08M | spills |
| FP32 dequantized | 6.52M | worse |

Conclusion: Rust/Wasm SIMD codegen plateaus at ~1.2 instructions/MAC (~1.1-1.4 FLOP/instr,
the same class as llama.cpp's own Wasm q8_0 kernel at ~1.4). Closing the remaining ~2.5x gap to
IC-Imajev (~2.7 FLOP/instr of *text* FLOPs) requires a hand-written WAT/Clang kernel with
pre-packed weight tiles and grouped scales, not more Rust loop tuning.

Also corrected the FLOP accounting: Qwen3.5-4B's text path is ~2B dense compute parameters (vision
encoder unused, embeddings compute-free), so Imajev's 121.67B instructions imply ~2.7, not ~5.5,
FLOP/instruction.
