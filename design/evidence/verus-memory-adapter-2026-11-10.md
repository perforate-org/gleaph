# Verus adoption slice 3: ic-stable-structures Memory adapter (2026-11-10)

Anchor timestamp: 2026-11-10 15:45:00 UTC +0900
Scope: plan 0384. Add the production-owner adapter that backs `ic_stable_structures::Memory` with the
verus-ic Stable64 bridge, prove it with a host BTreeMap test and a canister/PocketIC upgrade test, and
wire it into `verus-verify`. Parent commit: `8f3b3038`. No production canister or root Cargo file
changed.

## Adapter

`crates/verus-stable64/memory` (`verus-stable64-memory`):

- `VerusMemory` implements `ic_stable_structures::Memory` 0.7.2:
  - `size()` → `verus_ic::bridge::stable64_size`;
  - `grow(pages)` → `stable64_grow`, mapping a refusal (`u64::MAX`) to `-1` as the trait expects;
  - `read`/`write` → `stable64_read`/`stable64_write`.
- It is deliberately **plain**: the `Memory` trait cannot thread the Verus linear token. The adopted
  verified statement remains the leaf's absolute-growth contract and the verus-ic Stable64 model; no
  BTreeMap-internal proof is claimed.
- Host test: `MemoryManager<VerusMemory>` + `StableBTreeMap<u64,u64,_>` insert/get/len round-trip on
  the native backend (`reset_native_memory`).

## Map canister fixture and PocketIC

`fixtures/map-canister` is a canister whose stable state is one
`StableBTreeMap` over `MemoryManager<VerusMemory>` (`put`/`get`/`len`, explicit `init`/`post_upgrade`).
`fixtures/pocketic-tests/tests/map_upgrade_persistence.rs` installs it, puts `(7, 42)`, checks
`get`/`len`, upgrades the canister with the same wasm, and checks that `get` and `len` survive.

Local transcript:

```text
$ cargo test -p verus-stable64 -p verus-stable64-memory
tests::grow_to_absolute_target ... ok
tests::btreemap_round_trip_over_verus_memory ... ok

$ cargo build --target wasm32-unknown-unknown --release \
    -p verus-stable64-canister -p verus-stable64-map-canister
Finished `release` profile

$ POCKET_IC_BIN=... cargo test -p verus-stable64-pocketic-tests
test map_survives_upgrade_over_verus_memory ... ok
test upgrade_preserves_stable_memory ... ok
test result: ok. 2 passed; 0 failed
```

`cargo fmt --all --check`, the leaf's `cargo verus verify -p verus-stable64 --features verus`
(1 verified, 0 errors), YAML parsing and the freshness checker (all anchored) remain green.

## CI

`verus-verify.yml` now runs the adapter host test (`-p verus-stable64 -p verus-stable64-memory`),
builds both fixture canisters, and runs both PocketIC tests; all steps stay gated on
`VERUS_IC_TOKEN`.

## Next step: gleaph-account

`gleaph-account` is the first production migration candidate: it has exactly one stable structure
(`StableBTreeMap<Principal, Account, VirtualMemory<DefaultMemoryImpl>>` over
`MemoryManager::init(default_memory_impl())`). The migration will replace the default backend with
`MemoryManager::init(VerusMemory)` on a fresh state and add PocketIC create/get/upgrade coverage. It
is deferred until the root `Cargo.toml`/`Cargo.lock` (dirty with other sessions' work) are
reconciled, because the account crate needs the path dependency and lockfile change committed.
