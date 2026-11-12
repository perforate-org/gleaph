# getrandom seed choice for the on-chain tokenizer (2026-11-12)

Follow-up to the custom-backend note. Question: should `__getrandom_v03_custom` derive bytes from
`ic0.time()` (or another on-chain value), or is a fixed seed enough?

## Decision

Use a **fixed, deterministic seed** (zeros or a compiled constant) for the tokenizer's internal
hash maps. Do not derive pseudo-randomness from `ic0.time()`.

## Rationale

- The tokenizer's hash maps are lookup tables for a fixed vocabulary/merge set; randomness there is
  not a cryptographic requirement. Determinism makes ids, loads and benchmarks reproducible.
- `ic0.time()` is replicated consensus time: all replicas see the same value, and it is public and
  low-entropy. Hashing it (splitmix/xorshift) produces a deterministic public seed, which offers no
  meaningful collision resistance over a constant while adding non-obvious behavior.
- A real unpredictable seed would use `ic0.raw_rand`, which is async. It can be fetched in an async
  `init`/`post_upgrade` and persisted in stable memory, then fed to the hash builder. That is the
  correct IC primitive if adversarial hash-flooding becomes a demonstrated concern; it is not
  needed for the first probe.
- Avoid time-based pseudo-randomness entirely: it is not secret, not better than a constant, and
  invites a false sense of security.

## Consequence

The probe can build `ahash`/`HashMap` with a fixed seed (or use `ahash`'s deterministic
`RandomState::with_seeds`) and still reproduce the reference ids exactly. If a future threat model
requires unpredictable hashing, the seed comes from `raw_rand` at install/upgrade, not from time.

## Sources

- [IC resource limits](https://docs.internetcomputer.org/references/resource-limits/)
- [IC verifiable randomness](https://docs.internetcomputer.org/guides/backends/randomness/)
- [getrandom custom backend](https://github.com/rust-random/getrandom)
