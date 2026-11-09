# Verus production adoption slice 1: verified growth leaf (2026-11-09)

Anchor timestamp: 2026-11-09 22:05:00 UTC +0900
Scope: plan 0382. Adopt verus-ic v0.1.0 in a verified absolute-target Stable64 growth leaf, add the
pinned Verus toolchain scripts, and add a path-filtered CI job. Parent commit: `e9eefafe`. No
existing canister owner, root `Cargo.toml`/`Cargo.lock`, Rust workspace crate or Lean fixture changed.

## What was adopted

`crates/verus-stable64` is a standalone adoption-bootstrap workspace (the root Cargo files are dirty
with other sessions' work; it becomes a normal member later):

- API: `grow_to(target_pages: u64) -> u64` — grow stable memory to at least `target_pages` 64 KiB
  pages, return the final page count, abort on refusal.
- Plain build (`plain.rs`): `verus_ic::bridge::{stable64_size, stable64_grow}`, delta computed once,
  `u64::MAX` refusal aborts.
- Verified build (`verified.rs`, `--features verus`): acquires the `verus_ic::token` linear token,
  computes `target - pages` after the not-full branch, discharges `token::grow`'s precondition
  (`target ≤ max_pages`), and proves the contract `final_pages >= target_pages`.
- Dependency pin: `verus-ic = { git = "https://github.com/perforate-org/verus-ic", tag = "v0.1.0" }`,
  `vstd = "=0.0.0-2026-10-18-0147"` behind the `verus` feature
  (`verus = ["verus-ic/verus", "dep:vstd"]`).

The leaf intentionally does not touch a canister: verus-ic requires a single stable-memory owner, so
migrating an owner must happen atomically in a later slice.

## Verification transcript

```text
$ CARGO_NET_GIT_FETCH_WITH_CLI=true cargo test --manifest-path crates/verus-stable64/Cargo.toml
running 1 test
test tests::grow_to_absolute_target ... ok
test result: ok. 1 passed; 0 failed

$ CARGO_NET_GIT_FETCH_WITH_CLI=true bash scripts/verus-0.2026.10.18 \
    cargo verus verify --manifest-path crates/verus-stable64/Cargo.toml --features verus
verification results:: 2060 verified, 0 errors      # vstd
verification results:: 16 verified, 0 errors        # verus-ic v0.1.0
verification results:: 1 verified, 0 errors         # verus-stable64
Finished `dev` profile [unoptimized + debuginfo] target(s) in 38.81s
```

`bash scripts/setup-verus-stack.sh` found/provisions the pinned stack
(`.tools/verus-0.2026.10.18.7d82994`, release `0.2026.10.18.7d82994`, Rust 1.99.1). The scripts pass
`sh -n`.

## CI

`.github/workflows/verus-verify.yml` is path-filtered to the leaf, the two scripts and itself. It
installs the pinned Verus stack (Linux x86_64 asset, size 486844228, SHA-256 `e9af03…45dd`), runs the
host test and `cargo verus verify --features verus`. Because verus-ic is a **private** git
dependency, the token-dependent steps are gated on the `VERUS_IC_TOKEN` secret:

- with the secret: git credentials are rewritten, the stack is installed and verification runs;
- without it: the job emits a warning and skips the token-gated steps (the workflow stays green rather
  than failing checkout/PRs before the secret is provisioned).

The local equivalent (same commands, local credentials) is the transcript above. The CI job itself
was not executable here because the secret is not available in this environment.

## Trust boundary

Inherited from verus-ic v0.1.0 (`docs/trust-ledger.md`): the token mirrors live stable memory; the
`ic0` bridge implements its contract; a grow satisfying its preconditions is granted. Not claimed:
Candid, async execution, byte-level upgrade persistence beyond the verus-ic fixture, and multi-owner
layouts. The leaf adds one verified statement: a successful `grow_to` yields at least
`target_pages` pages.

## Remaining work

1. Make verus-ic consumable without a private token (crates.io publication or a read-only deploy key)
   and move the leaf into the root workspace.
2. Migrate one production stable-memory owner atomically, with a fresh-state plan and the single-owner
   rule, then extend the verified surface (read/write/upgrade reopen) as needed.
3. Add wasm-target verification/build for the consumer once a canister uses the leaf.
