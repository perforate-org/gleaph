# verus-stable64 — verified absolute-target Stable64 growth leaf

The first production-adoption leaf for [verus-ic](https://github.com/perforate-org/verus-ic):
an absolute-target wrapper around Stable64 growth, verified with Verus.

## API

```rust
pub fn grow_to(target_pages: u64) -> u64
```

`target_pages` is a page count (one page = 65,536 bytes). The function grows stable memory to at
least `target_pages`, returns the resulting page count, and aborts on a subnet refusal (the plain
bridge returns `u64::MAX`; the verified token traps), matching verus-ic's documented behavior.

- Default build: `plain::grow_to` calls `verus_ic::bridge::{stable64_size, stable64_grow}` and
  computes the delta.
- `--features verus`: `verified::grow_to` threads the `verus_ic::token` linear token and proves
  `final_pages >= target_pages` (Verus contract).

## Ownership and scope

Stable memory must have a **single owner** (verus-ic consumer-guide rule): do not mix this leaf with
`ic-stable-structures` or another stable-memory library in the same canister. This crate is not yet
wired into a Gleaph canister; it is the adoption bootstrap and the future owner of absolute-target
growth. Moving a real canister onto it is a later slice.

Trust inherited from verus-ic v0.1.0: the token mirrors live stable memory, the `ic0` bridge
implements its contract, and a grow satisfying its preconditions is granted. See
`verus-ic`'s `docs/trust-ledger.md`. Unclaimed there: Candid, async execution, byte-level upgrade
persistence beyond its fixture, and multi-owner layouts.

## Dependency pin

```toml
verus-ic = { git = "https://github.com/perforate-org/verus-ic", tag = "v0.1.0" }
vstd = "=0.0.0-2026-10-18-0147"   # optional, enabled by `verus`
```

The verus-ic repository is private: CI needs a `VERUS_IC_TOKEN` secret with read access, and local
fetches need working git credentials (`CARGO_NET_GIT_FETCH_WITH_CLI=true` is set by the workflow).
The workflow authenticates with `https://<token>@github.com/` (token as the username), which works for
both fine-grained PATs and the GitHub CLI OAuth token; run it manually with
`gh workflow run verus-verify.yml --repo perforate-org/gleaph`. Until the secret is configured the
workflow skips the token-gated verification steps with a warning (so checkout/PRs stay green);
configure it to activate the proof job. The dependency should become a crates.io version once
verus-ic is published.

## Memory adapter

`memory/` (`verus-stable64-memory`) implements `ic_stable_structures::Memory` over the
verus-ic Stable64 bridge (`size`/`grow`/`read`/`write`), so a canister can construct its
`MemoryManager`/`StableBTreeMap` over `VerusMemory` and keep a single stable-memory owner.
The adapter is plain: the verified claim remains `verus-stable64`'s absolute growth and the
verus-ic Stable64 model; the `Memory` trait cannot thread the linear token. A host BTreeMap
round-trip test and a canister/PocketIC upgrade test cover it.

## Fixtures

- `fixtures/canister` — a minimal canister that owns its Stable64 memory through
  this leaf (`grow_to_pages`, `write_marker`, `read_marker`, `pages`).
- `fixtures/map-canister` — `StableBTreeMap` over `MemoryManager<VerusMemory>`
  (`put`/`get`/`len`), proving the adapter carries a real structure across upgrade.
- `fixtures/pocketic-tests` — PocketIC tests: the growth marker canister (install,
  grow/write/read, upgrade, no-shrink absolute target) and the map canister (put,
  upgrade, get/len survive). Requires `POCKET_IC_BIN` and prebuilt wasm:
  ```sh
  cargo build --manifest-path crates/verus-stable64/Cargo.toml \
    --target wasm32-unknown-unknown --release \
    -p verus-stable64-canister -p verus-stable64-map-canister
  POCKET_IC_BIN=/path/to/pocket-ic \
    cargo test --manifest-path crates/verus-stable64/Cargo.toml -p verus-stable64-pocketic-tests
  ```

## Verification

```sh
bash scripts/setup-verus-stack.sh
CARGO_NET_GIT_FETCH_WITH_CLI=true \
  bash scripts/verus-0.2026.10.18 cargo verus verify \
  --manifest-path crates/verus-stable64/Cargo.toml --features verus
cargo test --manifest-path crates/verus-stable64/Cargo.toml
```

The pinned Verus release is `0.2026.10.18.7d82994`; `scripts/setup-verus-stack.sh` mirrors the
verus-ic setup script and adds the Linux x86_64 asset for CI. Update both pins together when Verus
moves.

## Why a standalone workspace

The crate declares its own `[workspace]` (with the canister and PocketIC fixture members) as an
adoption-bootstrap exception: the Gleaph root `Cargo.toml`/`Cargo.lock` are currently being
reconciled by other work, and adding a member would mix unrelated changes. It should become a normal
workspace member once those files are stable.
