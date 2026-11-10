# Verus adoption slice 2: canister and PocketIC fixture (2026-11-10)

Anchor timestamp: 2026-11-10 15:00:00 UTC +0900
Scope: plan 0383. Exercise `verus-stable64` as the sole stable-memory owner of a real wasm canister:
absolute growth, stable writes/reads, and canister-upgrade persistence under PocketIC. Parent commit:
`ab10c6c7`. No production canister or root Cargo file changed.

## Fixture canister

`crates/verus-stable64/fixtures/canister` (cdylib, `ic-cdk` 0.20, `candid` 0.10):

- `grow_to_pages(target) -> u64` — calls the leaf's plain `grow_to` (absolute target, delta computed
  once, refusal aborts) and returns the resulting page count;
- `write_marker(Vec<u8>)` — grows to one page when empty via the leaf, then
  `verus_ic::bridge::stable64_write` at offset 0;
- `read_marker() -> Vec<u8>` — reads the first eight bytes (empty before the first grow);
- `pages() -> u64` — `stable64_size`;
- `init`/`post_upgrade` are explicit, so the upgrade path is real rather than defaulted.

The canister has no heap state: its only durable state is Stable64 memory, which makes the owner rule
and upgrade behavior observable.

## PocketIC test

`crates/verus-stable64/fixtures/pocketic-tests/tests/upgrade_persistence.rs`:

1. install the wasm with empty init args;
2. `grow_to_pages(2)` → 2 pages;
3. `write_marker([1,2,3,4])`, `read_marker` → `[1,2,3,4,0,0,0,0]` (eight-byte read);
4. `upgrade_canister` with the same wasm and empty args, then `read_marker` still returns the marker;
5. `grow_to_pages(1)` after the upgrade → 2 (absolute target below the current size does not shrink);
6. `pages()` → 2.

Local transcript (cached PocketIC 15.0.0 server at
`crates/pocket-ic-tests/.pocket-ic/pocket-ic`):

```text
$ POCKET_IC_BIN=... cargo test -p verus-stable64-pocketic-tests
running 1 test
test upgrade_preserves_stable_memory ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.86s
```

The wasm builds with `cargo build --target wasm32-unknown-unknown --release -p
verus-stable64-canister`; the leaf host test and Verus verification still pass after the workspace
members were added (`verification results:: 1 verified, 0 errors` for the leaf; vstd/verus-ic
counts unchanged).

## CI

`verus-verify.yml` now also (token-gated on `VERUS_IC_TOKEN`):

- `rustup target add wasm32-unknown-unknown`, then builds the fixture canister wasm;
- downloads the pinned PocketIC **15.0.0** `x86_64-linux` server, sets `POCKET_IC_BIN`;
- runs `cargo test -p verus-stable64-pocketic-tests`.

The host test and verification now use explicit `-p verus-stable64` because the leaf workspace has
additional members.

## Scope and trust

This is a **runtime fixture**, not a new Verus claim: the verified statement remains the leaf's
`final_pages >= target_pages`; the canister test covers wasm build, single-owner Stable64 use, and
upgrade persistence. Candid/async semantics and byte-level upgrade proofs remain unclaimed, as in the
verus-ic consumer guide.

## Remaining work

1. Migrate one production canister's stable-memory owner onto the leaf atomically (fresh state), using
   this fixture as the runtime template.
2. Extend the verified surface (read/write/upgrade reopen) as the production layout requires.
3. Replace the private-git dependency (crates.io or read-only deploy key) and move the leaf into the
   root workspace once its Cargo files are reconciled.
