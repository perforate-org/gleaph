# Registry canister stable memory layout

The registry canister uses [`ic_stable_structures::MemoryManager`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryManager.html) over the default stable memory backend ([`DefaultMemoryImpl`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/struct.DefaultMemoryImpl.html) on wasm32; [`VectorMemory`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/type.VectorMemory.html) in the unit test under `registry_store`).

| `MemoryId` | Role |
|------------|------|
| `0` | **Registry map** — [`StableBTreeMap`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/btreemap/struct.StableBTreeMap.html) from UTF-8 graph name bytes (key) to candid-encoded [`GraphEntry`](src/lib.rs) (value). See [`registry_store.rs`](src/registry_store.rs). |

## Persistence policy

After each mutation of the in-memory `REGISTRY` map, the canister **refreshes** this map from a full snapshot (`persist_full`): existing keys are removed and current entries are re-inserted. **`#[pre_upgrade]` does not write stable memory**; durability relies on ongoing persistence and reload from stable on `#[init]` / `#[post_upgrade]`.

## Upgrade note (breaking)

Older builds persisted the registry with `ic_cdk::storage::stable_save` / `stable_restore` as a **single blob**. That layout is **not** migrated automatically. Deployments upgrading from that scheme must treat stable memory as empty or run a one-off migration out of band; otherwise the canister will start with an empty registry from stable.
