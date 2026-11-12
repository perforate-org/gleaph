# IC getrandom workaround for the tokenizer (2026-11-12)

Follow-up to `2026-11-12-onchain-tokenizer-benchmark.md`. The stock `tokenizers` crate pulls
`getrandom 0.3` through `ahash`'s default `runtime-rng` feature. The workaround space:

## Options

1. **`ic-dummy-getrandom-for-wasm`** (DFINITY): a custom backend that always returns
   `Error::UNSUPPORTED` at runtime. This only helps when randomness is never actually requested.
   `ahash`'s `RandomState::new()` does `getrandom::fill(...).expect(...)`, so the first hash map
   created by `tokenizers` would panic. Not usable here as-is.
2. **Custom getrandom backend with a successful fill (verified).** getrandom 0.3 supports opt-in
   backends through `--cfg getrandom_backend="custom"` plus one root-crate function:

   ```rust
   #[unsafe(no_mangle)]
   unsafe extern "Rust" fn __getrandom_v03_custom(
       dest: *mut u8,
       len: usize,
   ) -> Result<(), getrandom::Error> {
       unsafe { core::ptr::write_bytes(dest, 0, len) };
       Ok(())
   }
   ```

   Filling deterministic bytes is sufficient for the tokenizer's internal hash maps (they are
   lookup tables keyed by fixed vocabulary strings, not a cryptographic boundary). A production
   version can mix in `ic0.time` or a seed fixed at install instead of zeros.
3. **Patch/vendor `ahash` with `compile-time-rng`** (and without `runtime-rng`) so getrandom is
   not linked at all. Cleanest determinism, but requires a patched dependency and a build-time
   seed; not needed once option 2 works.

## Verified build

- `tokenizers 0.22` compiled for `wasm32-unknown-unknown` with `getrandom_backend="custom"` and
  the backend function above; an actual `Tokenizer::from_bytes` call path linked into the module.
- Resulting wasm: **2.0 MB** (release, opt-level `s`, LTO).
- IC limits from the resource-limits reference: **wasm code section 12 MiB**, total wasm 100 MiB,
  **install/upgrade budget 300B instructions**, update budget 40B. A combined inference-canister
  wasm (current ~0.9 MB) plus the tokenizer code fits the code-section limit with room to spare.

## Consequence for the benchmark

The purpose-built compact BPE runtime is no longer forced by wasm compatibility alone. The probe
can now compare:

- **A. Stock `tokenizers`** with the custom backend: JSON load at install/upgrade (300B budget) or
  from stable chunks; tokenize per update.
- **B. Compact binary + minimal BPE**: ~9.9 MB install blob, lower load work, but must reproduce
  exact ids.

Correctness is still the gate: both variants must match the reference tokenizer ids on the golden
set before any instruction number is meaningful.

## Sources

- [IC resource limits](https://docs.internetcomputer.org/references/resource-limits/)
- [getrandom custom backend recipe](https://github.com/rust-random/getrandom)
- [DFINITY `ic-dummy-getrandom-for-wasm`](https://github.com/dfinity/ic/tree/master/packages/ic-dummy-getrandom-for-wasm)
- [DFINITY forum: random numbers without async for getrandom](https://forum.dfinity.org/t/get-random-numbers-without-async-for-rust-getrandom/61671)
