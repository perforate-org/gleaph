# model20 canister-inference feasibility (2026-11-10)

Question: can the Gleaph routing checkpoint (`target/laya-routing/model20`, Laya/ModernBERT
decision model) run inside an IC canister like IC-Imajev's 4B decision model
(<https://forum.dfinity.org/t/ic-imajev-running-a-4b-decision-model-in-a-canister/76058>)?

## Checkpoint shape

| Property | model20 |
| --- | --- |
| Architecture | ModernBERT encoder: 22 layers, hidden 768, FFN 1152, 12 heads, rotary, sliding/full attention |
| Parameters | 321.9M (embedding 196.6M; dense compute ~91M MAC/token) |
| Decision path | one forward pass over a single row (state + instructions + 6 inline options), markers at option positions, classifier readout (`act_head`: 772->256->2, F32) |
| Input rows / length | 1 row; measured sequence length over the eval sets: dev 128-156, hand-written 129-141, blind 128-141 (mean ~136) |
| Output | per-option scores (choice), no autoregressive generation |

The model is already a decision model (single forward + readout), so the IC-Imajev execution
strategy (INT8 base projections, F32 LoRA/readout, split the forward across calls) applies directly
and with ~12x fewer parameters.

## Measurement 1: INT8 weight-only quantization impact

Per-output-channel symmetric INT8 for every Linear and the token embedding, dequantized in place;
the classifier head stays F32. Evaluated through Laya's own encoder/predict path with the serving
policy (T=2.5, `none-of-these >= 0.23`) on the three existing routing sets.

| Set | FP32 acc / coverage / precision / OOS | INT8-simulated acc / coverage / precision / OOS |
| --- | --- | --- |
| dev (200) | 91.5% / 98.8% / 90.5% / 40/40 | 91.5% / 98.8% / 90.5% / 40/40 |
| hand-written (106) | 92.5% / 97.5% / 93.6% / 25/26 | 93.4% / 97.5% / 94.9% / 25/26 |
| blind (60) | 88.3% / 95.6% / 90.7% / 14/15 | **86.7% / 95.6% / 90.7% / 13/15** |

INT8 keeps every blind floor (accuracy/coverage/precision/OOS all >= 85%); the cost is one answer
on the frozen blind set. Weight pack: **318.1 MB INT8 + 1.6 MB scales** (98 quantized tensors; only
the small classifier head remains F32). This fits stable memory trivially and even fits the 4 GiB
wasm heap if cached there.

## Measurement 2: compute estimate

Dense compute per token: 22 x (4*768^2 + 2*768*1152) MAC/token, plus the O(L^2) attention term.
FLOPs = 2 x MACs; instruction conversion uses two points: IC-Imajev's measured 4B run implies
~5.5 FLOP/instruction, and a conservative 2 FLOP/instruction for a canister SIMD kernel.

| Length | FLOPs | instructions @5.5 FLOP/instr | instructions @2 FLOP/instr |
| ---: | ---: | ---: | ---: |
| 128 | 25.5 GF | 4.63B | 12.7B |
| 141 (hand/blind max) | 28.3 GF | **5.15B** | 14.2B |
| 156 (dev max) | 31.6 GF | 5.75B | 15.8B |

Budget in this repo: **5B instructions per query call, 40B per update call**.

- **Update path: GO.** Even the conservative 2 FLOP/instruction estimate (15.8B) fits one update
  call, and a multi-worker split (IC-Imajev style) has large headroom.
- **Query path: borderline.** 5.15-5.75B at Imajev's efficiency slightly exceeds 5B; it needs a
  faster kernel, a slightly shortened input (fixed option descriptions dominate the 136-token
  average), or a two-call client-driven split.

## Other observations

- Only the question varies at serving time: instructions and the six option descriptions are fixed,
  so their token IDs (and possibly cached prefix activations) can be precomputed; the variable
  tokenization cost is one short state.
- The fixed prefix/option block is ~120 of the ~136 tokens; compressing the option descriptions or
  caching per-option encoder work is the main query-path lever.
- Weights must be installed/prepared by a controller update (INT8 pack + scales + config +
  tokenizer), read-only during inference; weight-pack checksums are needed for the public query
  path (state/weight mixing is the same hazard IC-Imajev documents).
- Tokenizer: the Laya tokenizer assets are ~33 MB. Either keep them in stable memory with a small
  byte-level BPE implementation, or take token IDs from the client on the query path (as IC-Imajev
  does) and keep a canister-side tokenizer for trusted update-path calls.

## Verdict

**Feasible.** Precision (INT8) and memory are not blockers; the only real constraint is the
per-call instruction budget. Recommended first implementation is the **update-path inference
canister** (single caller canister call, internally split if needed), with the query path as a
follow-up once the kernel's measured FLOP/instruction ratio is known. A smaller distilled decision
model remains the fallback if the measured kernel is far below 5.5 FLOP/instruction or if
update-path latency is unacceptable.

## Measurement 3: one encoder layer on wasm32 SIMD (canbench)

A canister-facing layer kernel now lives in `crates/laya-infer` (INT8 projections, LayerNorm, RoPE,
attention, GLU, residuals) and is measured with canbench on the production wasm32 `+simd128` target
at the exact model20 shape (hidden 768, FFN 1152, 12 heads, L=141):

| Benchmark | instructions | share |
| --- | ---: | ---: |
| full layer | **2.90B** | 100% |
| attention (scores + softmax + AV) | 1.33B | 46% |
| MLP (GLU up + down) | 843M | 29% |
| QKV projection (+norm, RoPE) | 539M | 19% |
| attention output projection | 191M | 7% |

The layer executes ~1.47 GFLOP of dense work, so this naive kernel achieves only ~0.5
FLOP/instruction (IC-Imajev's INT8 runtime implies ~5.5). The breakdown shows why: attention runs as
scalar F32 loops, and each INT8 projection does one function call per output row with bounds-checked
slices.

### Kernel optimization pass

A second pass rewrote attention to a head-major contiguous layout with F32 SIMD dot products /
AXPY, and the INT8 projections to a four-row blocked kernel that widens the activation vector once
per 16-element block and shares it across rows (`i32x4_dot_i16x8`). Measured again at L=141:

| Benchmark | naive | optimized | change |
| --- | ---: | ---: | ---: |
| full layer | 2.90B | **1.32B** | -54% |
| attention | 1.33B | **226M** | -83% |
| MLP | 843M | **591M** | -30% |
| QKV projection | 539M | **373M** | -31% |
| attention out projection | 191M | **134M** | -29% |

Extrapolating 22 layers: **~29B instructions** for the full encoder (embedding lookup and the
768+4 -> 256 -> 2 readout are marginal). Against the per-call budgets:

- **update path: GO with a single worker.** 29B fits one 40B update call; scaling IC-Imajev's
  measured 121B / 27.3s puts one decision in the ~7-15s range before further tuning.
- **query path: split, not one call.** 29B is ~6x the 5B query cap. A layer-boundary split (e.g.
  ~4 layers per query, hidden state 141x768 F32 = 433KB transferred per hop) needs ~6 client-driven
  query calls, which is the practical route; a single-call query would need another ~6x kernel
  improvement plus a much shorter input (the fixed option descriptions dominate the 136-token
  average).
- Remaining kernel headroom is real: the optimized projections are still ~1.5 instructions per MAC
  and attention ~7 per MAC, so further tuning is possible but not required for the update path.

### Optimization plateau and two further levers

Microbenchmarks show the INT8 projection kernel is at its practical limit for plain Wasm SIMD:
`gemv(2304x768)` costs 2.42-2.43M instructions (1.37/MAC). A hand-unrolled four-accumulator kernel
(2.42M), an eight-row kernel (4.08M), and an FP32 dequantized kernel (6.52M) are the same or worse;
`#[inline(always)]` and LTO change nothing. Further projection gains would need another algorithm
(e.g. 4-bit packing or relaxed-SIMD dot products), not loop tuning.

Two non-kernel levers were measured instead:

- **Length budget is not viable.** Shortening criteria or lowering `max_len`/`head_max_len`
  reduces the sequence but loses accuracy fast: blind 88.3% at L≈136, 76.7% at L≈106,
  65% at L≈88. Rejected.
- **Layer pruning works.** Evaluating the checkpoint with the last layers removed: 22 layers
  (baseline) blind 88.3%, 20 layers 88.3%, 18 layers 85.0% (coverage 91.1%), 16 layers 75.0%
  (OOS collapses 8/15). A 20-layer encoder is accuracy-neutral on all three sets and cuts the
  encoder instruction count by ~9% (22 x 1.32B -> 20 x 1.32B ≈ 26.4B, ~59% below the naive
  64B). 18 layers is the aggressive option that still clears the blind floor but loses ~3pt.

Combined verdict: **~26B instructions** with 20 layers, one 40B update worker, or ~5-6
layer-split query calls of ≤5B each. Since the model is already a decision model, this is the
same execution shape as IC-Imajev at ~1/15 of its instruction count.

Caveats: weights here are synthetic — INT8 projection cost is value-independent, and the INT8
accuracy question is covered by Measurement 1 — but real-weight validation and the pack exporter
are still pending. Heap use is small (workspace ~2MB); the 318MB INT8 weight pack can be cached in
the 4GiB heap or streamed from stable memory.

Next step: real INT8 weight pack exporter + optimized kernels (blocked INT8 matmul, SIMD F32
attention) benchmarked with canbench; then the update-path inference canister prototype.

## Update (2026-11-10): 14-layer distilled student

`model20-distil14b` (14 of 22 layers, distilled from teacher soft labels) passes every gate
(regression 93.4/95.0/98.7/92.3; blind 91.7/91.1/100/93.3) and halves the encoder work:
**14 x 1.32B ≈ 18.5B instructions**, still a single update call with a large margin. INT8
per-channel simulation keeps blind at 95.0% with a **278MB** weight pack. The canister prototype
can target 14 layers instead of 22.

## Prototype validated end-to-end (2026-11-10)

The INT8 pack + Rust forward reproduce the gate decisions without activation calibration:
dev 100% (OOS 40/40), hand-written 94.3/98.8/96.2 (OOS 24/26), blind 91.7/93.3/97.6 (OOS 14/15).
PyTorch parity keeps the same argmax on all reference questions; near-tie probabilities can drift
up to 0.27 from per-row activation INT8, which does not affect the routing policy.
`crates/laya-infer-canister` installs the pack in controller-only update chunks and serves
update-call inference over caller token ids; PocketIC deployment/instruction measurement is the
remaining step.

## Measured full model (2026-11-10)

`bench_full_model_l141` (14 encoder layers + 2-layer head + scorer, INT8, wasm32 SIMD) measures
**22.01B instructions** — down from the ~26B estimate because the head is now INT8 too. It fits one
40B update call; the wasm32 canister module is 756K plus the 285.7MB uploaded pack.

## Tiled-forward update (2026-11-10)

`bench_full_model_l141` now measures **17.37B instructions** with the 4-token tiled forward
(QKV/Wo/MLP/head tiled, attention per token), down from 22.01B, with identical end-to-end gate
decisions (blind 91.7%, regression 94.3%). The canister artifact builds with the C kernels linked
via the consumer build script.

## Final canister target (2026-11-10)

Canister inference pack: `model20-distil14b-h1.int8.pack` (278.6MB, 14 encoder layers, 1-layer
decision head) at **16.02B instructions** (canbench). End-to-end Rust INT8 evaluation: blind
93.3 / 95.6 / 97.7 (OOS 14/15), hand 93.4 / 100.0 / 95.0 (OOS 23/26) - all floors pass.

## PocketIC install/infer E2E (2026-11-10)

The pruned canister pack (81.6MB, INT4 embedding + INT8 dense) installs over **43 chunks of
1.9MB** and answers a blind routing question correctly in a PocketIC application subnet
(install + upload + infer = 9.4s locally). Canister wasm: 804KB (C kernels linked).

## Production pack (2026-11-10)

Canister inference target for general prompts: `model20-distil14b-h1-prod32k.int8.pack`
(93.8MB, 32,873-token INT4 embedding + INT8 dense, 14 encoder layers, 1-layer head).
Installs in **50 chunks of 1.9MB**; PocketIC E2E upload + finish + correct `infer` takes 11.6s
locally. The used-vocabulary pack (81.6MB, 43 chunks) remains the minimal-install variant.
