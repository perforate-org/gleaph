# Account canister → verus-ic owner migration readiness

Anchor timestamp: 2026-11-10 16:00:00 UTC +0900
Target revision: git `93a980d3`
Status: analysis record feeding plan 0385 and the Verus production-adoption track
([ADR 0104](../adr/0104-verus-owns-implementation-proofs.md)). It changes no code; it records the
migration surface, the exact intended diff, the test hazards and the current blocker.

## Question

Can `gleaph-account` become the first production canister whose Stable64 memory is owned by the
verus-ic bridge through `verus-stable64-memory`, and what exactly does that migration touch?

## Migration surface (measured)

`crates/account` is 693 lines over six files. Its **entire stable surface is one map**:

```text
crates/account/src/stable/memory.rs
  MemoryManager<DefaultMemoryImpl>::init(default_memory_impl())
  StableBTreeMap<Principal, Account, VirtualMemory<DefaultMemoryImpl>>  // MemoryId 0
  thread_local MEMORY_MANAGER + ACCOUNTS
crates/account/src/stable/store.rs
  every read/write goes through memory::ACCOUNTS
```

No other module imports `ic-stable-structures`, `ic-stable-memory-backend` or a `Memory`/
`MemoryManager`; `canister.rs`, `types.rs` and `lib.rs` are storage-agnostic. There is exactly one
`MemoryId` (`ACCOUNT_BY_ID = 0`). The canister's `post_upgrade` is empty: reinitializing the
thread-locals against existing stable memory is the whole upgrade path.

## Intended diff (fresh state; no data migration)

`crates/account/Cargo.toml`:

```diff
-ic-stable-memory-backend = { workspace = true }
 ic-stable-structures = { workspace = true }
+verus-stable64-memory = { path = "../verus-stable64/memory" }
```

`crates/account/src/stable/memory.rs`:

```diff
-use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
+use verus_stable64_memory::VerusMemory;
 ...
-pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;
+pub(crate) type Memory = VirtualMemory<VerusMemory>;
 ...
-    RefCell::new(MemoryManager::init(default_memory_impl()));
+    RefCell::new(MemoryManager::init(VerusMemory));
```

The on-disk *layout* (MemoryManager region table + BTreeMap) is unchanged; only the backend that
implements `Memory` changes. Because Gleaph is pre-production, the canister is deployed on a fresh
state and old testnet data is not migrated. `MemoryId 0` stays the account map.

## Test hazards (the non-obvious part)

1. **Backend scope changed.** The old `default_memory_impl()` host backend is suitable for the
   existing parallel tests; the verus-ic native bridge is a process-wide `Mutex<Vec<u8>>`, so account
   tests that run concurrently would share stable memory. Add a crate-level test mutex and acquire it
   in the three stateful tests (the pure RBAC test needs none), or run that test target with
   `--test-threads=1`; the in-code lock is preferable because it also documents the global resource.
2. **`reset_all` is no longer sufficient.** It currently only calls `ACCOUNTS.clear_new()`. After a
   bridge reset to zero pages, the cached `MEMORY_MANAGER`/`ACCOUNTS` thread-locals still refer to the
   old region table. The test reset must reset `verus_ic::bridge::reset_native_memory(0)` and then
   replace both thread-locals with fresh `MemoryManager::init(VerusMemory)` / map instances. The
   production path does not need this; it is test-only.
3. **No token is threaded.** `ic_stable_structures::Memory` cannot carry `Tracked<Mem>`; the verified
   claim remains `verus-stable64` absolute growth and the verus-ic Stable64 model, as recorded in
   plan 0384. BTreeMap-internal operations are not newly proved.

## Runtime acceptance (after unblocking)

- Build `gleaph-account` for wasm (it is a `cdylib`/`rlib`); its existing Candid surface is unchanged.
- PocketIC: install, `create_account`, `get_account`, upgrade the canister with the same wasm, then
  `get_account`/member resolution still return the pre-upgrade state; verify `pages()`/memory use is
  non-zero. This mirrors the map-canister upgrade test and adds the account handlers.
- Remove the `ic-stable-memory-backend` dependency from the account crate after the switch (the
  single-owner rule leaves no second backend path).

## Blocker

The root `Cargo.toml`/`Cargo.lock` are dirty with other sessions' in-progress workspace work
(`crates/laya-infer`, Rune/console members and their dependency changes; lockfile ±2.6k lines). The
account migration needs a path dependency and a lockfile update, which cannot be committed without
including unrelated work. The migration therefore waits until the root workspace files are
reconciled; the leaf/adapter work is deliberately kept in its standalone workspace meanwhile.

## Risks / notes

- `MemoryManager` metadata initialization on a fresh state is exactly what the map fixture exercises;
  no account-specific layout risk beyond the test-reset hazard above.
- The old backend removal is a contract change for the account crate only; other crates keep their
  existing backends until their own migration slice.
- The verified-leaf build (`--features verus`) is not used by the account canister: release/wasm builds
  use the plain mirror, matching the leaf's dual-interface design.
