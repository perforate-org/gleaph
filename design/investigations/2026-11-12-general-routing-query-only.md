# General routing: query-only route and quota/tokenizer removal (2026-11-12)

Status: implemented and verified for ADR 0116.

## Change

- Router `route_general` is now an `#[ic_cdk_macros::query(composite = true)]` method and the sole
  route. The update route and `preview_route_general` were removed.
- The route no longer counts tokens or consumes quota; the 16 KiB text cap and 64-operation context
  cap remain.
- `inference_quota` (heap windows, config field, admin setter/stats, tests) and the tokenizer
  install/count/fingerprint forwarding were deleted.
- The inference canister no longer owns stable memory: tokenizer chunk/finish, fingerprint, count,
  MemoryIds 1/2 and `post_upgrade` reload were removed. Its runtime is just the frozen general
  route plus the bench probe.
- Wire version is 4; tokenizer types and `MAX_QUESTION_TOKENS` were removed.
- The operator tokenizer installer binary, artifacts tokenizer asset, fetch script and demo
  tokenizer instructions were removed. The Glev runtime pin remains.

## Raw calibration batch (wire 5)

`admin_general_route_raw_batch(graph, texts)` is a composite query that builds one threshold-zero
context and scores up to 8 questions in one Router→canister hop. Calibration now chunks its dev set
by 8 instead of issuing one query per item. Each result carries the shared context fingerprint; the
canister validates the context once, preserves input order and the Router checks the reply length,
fingerprints and operation membership.

## Validation

```sh
cargo test -p gleaph-routing-canister --lib
# 4 passed: general route select/extract, OOS + fingerprint mismatch, wire 5, raw batch

cargo test -p gleaph-routing-canister --test artifacts_manifest
# 1 passed: runtime revision + wire 5, no asset

cargo build -p gleaph-routing-canister --release --target wasm32-unknown-unknown --features bench
POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-routing-canister --test general_route_bench -- --nocapture
# wasm 2,174,445 bytes (was 3,770,939 before tokenizer removal)
# worst 50 ops / 256 tokens: 262,331,969 instructions (was 361,203,908)
# memory before/after: 6,840,489 bytes (was 9,747,703)

POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-pocket-ic-tests --test general_router_route
# 3 passed: movie, knowledge and OOS through the Router composite query, with NO tokenizer upload;
# the movie test also exercises the raw batch (2 texts, one context fingerprint)

POCKET_IC_BIN=<pocket-ic 15> cargo test -p gleaph-pocket-ic-tests --test general_route_authority
# 1 passed: graph-tenant profile read/write, non-tenant profile/raw rejection

cargo test -p gleaph-router --lib
# 1,156 passed
cargo test -p gleaph-sdk --features routing
cargo test -p gleaph-cli
```

The general route bench ratio rose from 2.43x to 5.25x because the fixed tokenizer overhead was
removed from the smaller cells; the absolute worst cell fell by ~99M instructions.

## Notes

- Query routing is free but single-replica/uncertified. Routing is advisory; execution re-authorizes
  and prepared results keep their own consistency contract.
- No durable quota or rate limit exists for query routing. Abuse protection is infrastructure-level;
  a paid/consensus-verified routing tier would need a new ADR.
- Fresh-install semantics: Router runtime-config shape, canister stable memory (now none) and wire
  version changed with no migration or compatibility decoder.
