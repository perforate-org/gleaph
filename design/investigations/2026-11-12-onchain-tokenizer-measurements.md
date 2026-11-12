# On-chain tokenizer probe results (2026-11-12)

This is the measured follow-up to
[`2026-11-12-onchain-tokenizer-benchmark.md`](2026-11-12-onchain-tokenizer-benchmark.md), whose
native estimates and "stock crate rejected" item are superseded by the probe below. The
executable probe and its full result table live in the glev repository
(`experiments/tokenizer-onchain-benchmark.md`, commit `bed68b8`); this record keeps the dated
Gleaph-side summary and cites ADR 0113.

## Probe

- Stock `tokenizers 0.22` inside a wasm32 canister with a deterministic getrandom custom backend
  (`getrandom_backend="custom"` plus `__getrandom_v03_custom`); the DFINITY dummy backend returns
  an error and panics in `ahash`, so it is not usable.
- 34,363,285-byte pinned tokenizer, uploaded in 19 chunks, parsed once in an update, persisted
  with `stable_save`, and re-parsed in `post_upgrade`.
- Ids were compared against the native `tokenizers` reference for every sample, including after
  the upgrade reload.

## Measurements (PocketIC, wasm32)

| Measurement | Result |
| --- | --- |
| install load (parse 34 MB JSON + persist) | **9,852,047,100 instructions** (one 40B update) |
| upgrade reload (`post_upgrade`) | **9,840,391,974 instructions** (300B budget) |
| short English (13 tokens) | 386,298 instructions |
| medium English (16 tokens) | 441,652 instructions |
| long English (40 tokens) | 1,032,379 instructions |
| short Japanese (16 tokens) | 231,026 instructions |
| long Japanese (48 tokens) | 517,977 instructions |
| URL (24 tokens) | 370,371 instructions |
| probe wasm | 3.6 MB (code-section limit 12 MiB) |

## Consequence

Tokenization is negligible beside the ~18.5B-instruction model forward (roughly 1M versus 18.5B,
about 0.006%). The stock crate is viable; a compact BPE exporter is only an optional install-size
optimisation. The text ingress adds one `tokenizer_token_count` call (~1M instructions) before the
model call so quota units stay exact. ADR 0113 records and implements the resulting ownership
move.
