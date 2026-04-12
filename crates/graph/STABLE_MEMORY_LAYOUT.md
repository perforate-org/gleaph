# Canister stable memory layout (`MemoryManager` / `Ic0StableMemory`)

The graph canister uses a single root [`ic_stable_structures::Ic0StableMemory`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/struct.Ic0StableMemory.html) (or [`VectorMemory`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/type.VectorMemory.html) in native tests) under one [`MemoryManager`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryManager.html). Logical partitions are fixed [`MemoryId`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/memory_manager/struct.MemoryId.html) values:

| `MemoryId` | Role |
|------------|------|
| `0` | **Graph PMA** — full [`GraphStore`](../../graph-store/src/facade.rs) backing: adjacency, property stores, PIDX btree bytes, and a **tail PMA stable root footer** ([`pma_stable_root`](../../graph-store/src/low_level/pma_stable_root.rs)) that stores the authoritative candid-encoded [`RegionManager`](../../graph-store/src/low_level/manager.rs). The footer is rewritten on graph flush (`flush_graph_metadata_stable`); it must not overlap `next_extent_addr` (see `write_region_manager_footer`). |
| `1` | **Gleaph service core** — one [`StableCell`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/struct.StableCell.html) holding a candid-encoded [`GleaphServiceCoreSnapshot`](src/service.rs) (ACL, prepared queries, extension types). Upgrades from older builds may still hold a monolithic [`GleaphServiceSnapshot`](src/service.rs) in this cell until the next service persist. |
| `2` | **Legacy region manager cell (read-only on upgrade)** — candid-encoded [`RegionManager`] used **only** when `post_upgrade` finds no valid PMA tail footer (pre-migration canisters). It is no longer written after migration. |
| `3` | **Graph catalog blob** — raw bytes for [`GraphCatalog`](src/catalog.rs) stable wire format (`StableBTreeMap`), split out of the former monolithic service snapshot. |

## Bucket size

The memory manager uses the library default bucket size unless we adopt `init_with_bucket_size` for tuning. Pre-production canisters may change bucket sizing without migrating legacy `stable_save` payloads (none are supported).

## Design note

Service metadata and graph PMA stay on **separate** `MemoryId`s because growth and access patterns differ, but each side uses **one** primary stable structure (no ad-hoc extra serde upgrade blobs).

## Persistence policy (graph PMA — no whole-state encoding)

**Authoritative graph state** (adjacency surfaces, property store regions, PIDX / equality-index btrees, and any other PMA-owned bytes) **lives only in stable memory** addressed by [`RegionManager`](../../graph-store/src/low_level/manager.rs) layouts. Within that PMA backing, bucket-backed property and PIDX regions use [`GleaphMemoryManager`](../../graph-store/src/low_level/virtual_region_memory.rs) and [`VirtualBucketMemory`](../../graph-store/src/low_level/virtual_region_memory.rs) as the analogue of ic’s `MemoryManager` + per-partition virtual `Memory` (logical byte offsets over each region’s bucket chain). Runtime code **reads and updates through**:

- the [`Memory`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/trait.Memory.html) interface on the graph’s virtual stable region (page-granular I/O, growth via the manager), and  
- **`StableBTreeMap`** (and related `ic_stable_structures` collections) where graph-store already maps keys/values with [`Storable`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/trait.Storable.html), and  
- **PMA / kernel operations** (vertex and edge records, scans, mutations) that ultimately read or write those stable-backed structures — **not** by serializing the whole graph or whole subsystems into a single blob.

**Forbidden for the graph path:** treating the PMA as an in-memory graph that is **fully re-serialized** (e.g. giant candid / serde snapshot of adjacency or property stores) on each request or upgrade boundary. **RegionManager metadata itself** must follow the same spirit: it is **allocation and layout metadata** for stable regions; the target design is to keep it **incrementally consistent on stable** (or equivalent structured stable cells/maps), **not** to re-encode the entire `RegionManager` value on every canister message as the primary persistence mechanism. The current tail footer still uses a full candid encode on flush; a structured PMA directory (v2+) can replace that without moving graph data off `MemoryId` 0.

### Virtual extent / `StableVec` rollout (graph-store)

Phased work (see merged stable PMA backlog): migrate region **contents** to [`VirtualExtentMemory`](../../graph-store/src/low_level/virtual_region_memory.rs) plus [`StableVec`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/vec/struct.StableVec.html) / [`StableBTreeMap`](https://docs.rs/ic-stable-structures/latest/ic_stable_structures/btreemap/struct.StableBTreeMap.html) where appropriate — **forward vs reverse** adjacency remain separate [`RegionKind`](../../graph-store/src/low_level/region.rs) values (slots `1` and `5` for edge entry heads); use [`GleaphMemoryManager::get_forward_edge_entries_extent`](../../graph-store/src/low_level/virtual_region_memory.rs) / [`get_reverse_edge_entries_extent`](../../graph-store/src/low_level/virtual_region_memory.rs) for explicit API. Target regions include vertex tables, label indices (×2), segment logs, maintenance queue, shard directory, and label catalog / GC state.

**Service state** (ACL, prepared queries, catalog blob, etc.) is a separate aggregate; it may use a bounded snapshot today, but it must **not** be conflated with graph PMA persistence and should evolve toward **minimal, incremental** stable updates where size and hot-path cost require it.

This policy is the contract for future canister wiring: request handlers **must not** add per-message full encodes of graph or region layout; they rely on graph-store’s stable-backed APIs only.
