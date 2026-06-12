# Index and catalog capacity planning

Last updated: 2026-06-12  
Anchor timestamp: 2026-06-12 23:38:44 UTC +0000

## Status

**Planned** — capacity model, split thresholds, and inverted posting-list optimization for operators and future index work. No automated enforcement in canisters yet. Current flat `PostingKey` layouts and region inventory match **Implemented** code (`graph-index`, ADR 0007).

## Purpose

Provide **falsifiable formulas and threshold tables** so graph-index (and related catalog) stable memory stays **practically below** the Internet Computer **500 GiB per-canister stable memory limit**, with headroom for B-tree overhead, metadata regions, and growth.

## Non-goals

- Exact byte-accurate stable page accounting (use canbench / future metrics for that).
- Graph canister (LARA) sizing in full detail — only enough context to show index is usually not the first limit.
- Choosing final **`GROUP_SIZE` or split axis** — deferred to operator policy / follow-up ADR ([ADR 0010](../adr/0010-index-sharding-extensibility.md)); capacity-planning lists options only.
- Subnet-level storage quotas or cycle billing.

## Platform limits (planning assumptions)

Source: [IC resource limits](https://docs.internetcomputer.org/references/resource-limits/) (verified against docs **as of 2026-06-12 UTC**).

| Limit | Value | Planning note |
|-------|-------|----------------|
| Stable memory per canister | **500 GiB** | Hard upper bound for `INDEX_*` regions + future catalog stable |
| Stable read/write per replicated update | **2 GiB** | Backfill and large range scans must batch (`admin_*_backfill_step`) |
| Wasm heap (wasm64 / EOP) | 6 GiB | Query paths must not materialize full buckets in heap |

**Operational headroom (recommended):**

| Threshold | Stable bytes | Action |
|-----------|--------------|--------|
| **Soft** | **350 GiB** (`3.76 × 10^11` B) | Open split/migration plan; freeze new `CREATE INDEX` on hot canister |
| **Hard** | **400 GiB** (`4.29 × 10^11` B) | Stop index writes; split or add index canister before accepting DML that maintains postings |
| **Critical** | **450 GiB** | Emergency backfill to new canister only |

Leave **≥ 100 GiB** below 500 GiB for B-tree internal nodes, `MemoryManager` bookkeeping, catalog stable (if moved to index), and upgrade margin.

---

## What consumes stable memory

### graph-index canister (today)

| MemoryId | Symbol | Scales with | Class |
|--------|--------|-------------|-------|
| 2 | `INDEX_POSTINGS` | Indexed **vertex** property assignments | derived |
| 4 | `INDEX_LABEL_POSTINGS` | **Label memberships** (all labeled vertices) | derived |
| 5 | `INDEX_EDGE_POSTINGS` | Indexed **edge** property assignments | derived |
| 0–1, 3 | admins, shard owners, router | O(shards) | canonical |

Catalog (`BidirectionalCatalog` on router today; optional on index later) scales with **unique metadata names** — typically **≪ 1 GiB** even for aggressive schemas. It is **not** a driver of the 500 GiB limit.

### Dominance order (typical production)

1. **Label postings** — one entry per `(label, shard, vertex)`; not opt-in.
2. **Vertex property postings** — one entry per indexed, indexable, non-null assignment (`CREATE INDEX`).
3. **Edge property postings** — one entry per indexed edge property value.
4. **Catalog** — negligible vs 1–3.

---

## Key size model (from code)

Encoded sizes below are **key bytes only** (values in `StableBTreeSet`; no separate value column).

### Vertex property posting — `PostingKey`

Layout: `1 + 4 + 4 + len(value) + 4 + 4` → **`17 + V` bytes**  
(`crates/graph-index/src/key.rs`, `V` = sortable index key from `value_to_index_key_bytes`).

### Label posting — `LabelPostingKey`

Layout: **`13` bytes** fixed (`crates/graph-index/src/label_key.rs`).

### Edge property posting — `EdgePostingKey`

Layout: **`23 + V` bytes** (property, value, label_id, shard, owner, slot).

### Sortable value length `V` (planning profiles)

| Profile | Typical `V` | Example |
|---------|-------------|---------|
| `bool` | 2 | true/false tag + payload |
| `int64` / `decimal` | 12–24 | numeric normalization |
| `text` short | 16–40 | country code, enum |
| `text` uuid | 38–52 | high-cardinality id |
| `text` long | 80–256+ | free text indexed (discouraged at scale) |

Use measured `V` from a sample of `value_to_index_key_bytes` when available.

### B-tree overhead factor **η**

Stable structures store keys in 4 KiB pages with internal nodes. For planning:

```text
stored_bytes ≈ η × (entry_count × key_bytes)
```

| η | Use |
|---|-----|
| **1.5** | Optimistic (dense buckets, small keys) |
| **2.0** | **Default for tables below** |
| **2.5** | Conservative (sparse trees, many distinct prefixes) |

---

## Growth formulas

Symbols:

| Symbol | Meaning |
|--------|---------|
| **N** | Live vertices (summed over graph shards in one index group) |
| **λ** | Average label memberships per vertex (≥ 1 for labeled nodes) |
| **κ_v** | Indexed vertex properties with a value on a vertex (worst case: all N vertices) |
| **E** | Live edges |
| **κ_e** | Indexed edge properties per edge (worst case: all E edges) |
| **V** | Average sortable value key length (bytes) |
| **η** | B-tree overhead factor (default **2.0**) |

```text
L_label   = N × λ
P_vertex  = N × κ_v        // upper bound if every vertex carries every indexed prop
P_edge    = E × κ_e

S_label   = η × L_label   × 13
S_vprop   = η × P_vertex  × (17 + V)
S_eprop   = η × P_edge    × (23 + V)

S_index   = S_label + S_vprop + S_eprop + S_meta
S_meta    ≈ 1 MiB × (#shards)     // planning fudge for owners/admins
```

**Cardinality note:** For low-cardinality property `country`, many vertices share the same `(property_id, value)` **prefix**, but Gleaph still stores **one posting key per vertex** — bucket walks are prefix-local, storage is **O(vertices with that property)**, not O(distinct values). A **Planned** inverted layout (§ below) stores each distinct sortable value once and reduces this to **O(distinct values + vertices)** for the posting tail.

---

## Threshold table A — label index only

Assume **η = 2.0**, **λ = 1**, no vertex/edge property indexes (`κ_v = κ_e = 0`).

| Vertices **N** | Label postings | Est. `S_label` | vs soft 350 GiB |
|----------------|----------------|----------------|-----------------|
| 1 × 10^6 | 1 M | **~26 MiB** | safe |
| 1 × 10^7 | 10 M | **~260 MiB** | safe |
| 1 × 10^8 | 100 M | **~2.6 GiB** | safe |
| 5 × 10^8 | 500 M | **~13 GiB** | safe |
| 1 × 10^9 | 1 B | **~26 GiB** | monitor |
| 5 × 10^9 | 5 B | **~130 GiB** | plan split before growth |
| 1 × 10^10 | 10 B | **~260 GiB** | **approach soft limit** |

With **λ = 2** (multi-label average), multiply **`S_label`** by 2.

---

## Threshold table B — vertex property index (opt-in)

Assume **η = 2.0**, **V = 16** (int/short text), **κ_v = 1** (one indexed property per vertex, all populated).

| Vertices **N** | Vertex postings | Est. `S_vprop` | Cumulative with table A row |
|----------------|-----------------|----------------|------------------------------|
| 1 × 10^7 | 10 M | **~660 MiB** | ~920 MiB @ N=10M |
| 1 × 10^8 | 100 M | **~6.6 GiB** | ~9.2 GiB @ N=100M |
| 1 × 10^9 | 1 B | **~66 GiB** | ~92 GiB @ N=1B |
| 5 × 10^9 | 5 B | **~330 GiB** | **exceeds soft limit** with labels |

**κ_v = 3** (three indexed vertex properties, all populated): multiply **`S_vprop`** by 3.

| Scenario | N | κ_v | V profile | Est. `S_vprop` (η=2) |
|----------|---|-----|-----------|----------------------|
| Profile + age + country | 10^8 | 3 | V≈16 | **~20 GiB** |
| UUID primary lookup | 10^8 | 1 | V≈48 | **~13 GiB** |
| UUID × 2 indexed fields | 10^8 | 2 | V≈48 | **~26 GiB** |

High-cardinality indexed strings (UUID, hash) are **as expensive as label postings per field**.

---

## Threshold table C — edge property index

Assume **η = 2.0**, **V = 16**, **κ_e = 1**, average **E = 10 × N** (avg degree 10).

| Vertices **N** | Edges **E** | Edge postings | Est. `S_eprop` |
|----------------|-------------|---------------|----------------|
| 1 × 10^7 | 10^8 | 10^8 | **~7.8 GiB** |
| 1 × 10^8 | 10^9 | 10^9 | **~78 GiB** |
| 5 × 10^8 | 5 × 10^9 | 5 × 10^9 | **~390 GiB** |

Edge indexes dominate when **E × κ_e** is large. Prefer **vertex-side indexing** or **narrow edge label + property** registration when possible.

---

## Threshold table D — combined scenarios (worked examples)

All use **η = 2.0**. “Soft OK” means **`S_index` ≲ 350 GiB**.

| ID | Workload sketch | N | E | λ | κ_v | κ_e | V | Est. `S_index` | Soft OK? |
|----|-----------------|---|---|---|-----|-----|---|----------------|----------|
| **D1** | Standalone social, label + 2 indexed ints | 10^7 | 10^8 | 1 | 2 | 0 | 16 | **~2.6 GiB** | yes |
| **D2** | Federation catalog, 1 label, 3 indexed props | 10^8 | 10^9 | 1 | 3 | 0 | 16 | **~46 GiB** | yes |
| **D3** | D2 + one indexed edge weight | 10^8 | 10^9 | 1 | 3 | 1 | 16 | **~124 GiB** | yes |
| **D4** | Multi-label (λ=2), 2 vertex indexes | 10^9 | 10^10 | 2 | 2 | 0 | 16 | **~218 GiB** | yes (monitor) |
| **D5** | Billion-node, λ=1, κ_v=3 | 10^9 | 10^10 | 1 | 3 | 0 | 16 | **~158 GiB** | yes (monitor) |
| **D6** | D5 + κ_e=1 on all edges | 10^9 | 10^10 | 1 | 3 | 1 | 16 | **~236 GiB** | borderline |
| **D7** | UUID vertex id indexed | 5 × 10^8 | 5 × 10^9 | 1 | 1 | 0 | 48 | **~179 GiB** | monitor |
| **D8** | Label-only at 10B vertices | 10^10 | — | 1 | 0 | 0 | — | **~260 GiB** | monitor |
| **D9** | Property-heavy, no labels counted | 5 × 10^9 | — | 0 | 5 | 0 | 16 | **~330 GiB** | **split** |

---

## When to add or split an index canister

### Decision rules (operator)

1. **Estimate** `S_index` from formulas or future metrics (`entry_count × avg_key_bytes × η`).
2. If **`S_index > 350 GiB`** → schedule split (see strategies below).
3. If **`S_index > 400 GiB`** → block new index-maintaining DML on that canister until split completes.
4. **Graph shard** stable **> 350 GiB** → split **graph** first; index follows shard group ([ADR 0006](../adr/0006-pre-federation-foundation.md) §5).
5. **New `CREATE INDEX`** on a hot canister: reject if projected **`S_index + Δ > 350 GiB`**.

### Split strategies (planned implementation)

| Strategy | Partition key | Best when | Router change |
|----------|---------------|-----------|---------------|
| **Shard group** (`GROUP_SIZE`) | `shard_id / GROUP_SIZE` | Graph already sharded; postings tagged with `shard_id` | Resolve group → index principal at registration (**example policy** — [ADR 0010](../adr/0010-index-sharding-extensibility.md)) |
| **Subject split** | label vs vertex-prop vs edge-prop regions | One posting type dominates | Fan-out lookup by plan anchor kind |
| **Property range** | `property_id` bands | Few huge indexed properties | Merge `lookup_equal` / intersection results |
| **Logical graph boundary** | one index canister per graph | Multi-tenant router; catalog on index | `list_shards_for_graph` → single index principal |

Postings are **derived** ([stable-memory-inventory.md](../storage/stable-memory-inventory.md)): migration = **new canister + backfill + router registry cutover**.

### Catalog placement vs capacity

Moving **`ROUTER_PROPERTY_CATALOG`** (and label catalogs) to the **graph-index canister** does **not** materially change **`S_index`** (names are O(thousands–millions), not O(vertices)). Choose catalog location for **SSOT and tenant isolation**, not for 500 GiB headroom.

---

## Graph canister reminder (same 500 GiB cap)

Canonical LARA + properties usually grow **faster** than index:

```text
S_graph ≈ O(N) vertex rows + O(E) edge/payload bytes + O(all properties stored)
```

Rule of thumb: if **`E × avg_payload`** approaches hundreds of GiB, **shard the graph** before tuning index splits. Index size is bounded by **what you index**, not everything stored.

---

## Planned optimization — sortable value dictionary and posting lists

### Problem (current layout)

Vertex and edge property postings embed the **full sortable index key** in every `PostingKey`
(`value_to_index_key_bytes` from `gleaph-gql`; see [property-index.md](property-index.md)). For
`text`, `bytes`, and large extension keys, **`V` is repeated once per vertex (or edge)** even when
many rows share the same encoded value (e.g. `country = "JP"` on 10^8 vertices).

General-purpose byte compressors (gzip, zstd) are **unsuitable**: they destroy lexicographic order,
breaking `lookup_range` and ordered bucket walks.

### Approach — order-preserving indirection (not gzip-in-key)

Split property/edge indexes into two derived layers inside graph-index:

```text
Value bucket (distinct sortable keys, one copy each)
  Key:   (property_id, sortable_value_bytes)
  Order: memcmp on sortable_value_bytes (same as today’s value suffix)

Posting tail (many rows per value)
  Key:   (property_id, value_ordinal, shard_id, vertex_id)
         — or edge tail: (+ label_id, owner_vertex_id, slot_index)
  Order: value_ordinal preserves value order; tail fixed width (~12–16 B)
```

`value_ordinal` is a dense **`u32`** assigned when a distinct `(property_id, sortable_bytes)` is
first inserted into the value bucket. Ordinal order must match sortable byte order (rank in the
value `BTreeMap`, not insertion order).

**Wire/API:** Router and graph continue to pass **`encoded_value` bytes** on
`lookup_equal` / `posting_insert`; graph-index maps to `value_ordinal` internally.

### What this saves

| Cardinality | Example | Current `S_vprop` driver | With dictionary + tail |
|-------------|---------|--------------------------|-------------------------|
| **Low** | enum / country on 10^8 vertices, **D** distinct values | **η × N × (17 + V)** | **η × (D × (17 + V) + N × (17 + 4))** |
| **Medium** | 10^6 distinct strings, 10^8 vertices | same as low if D ≪ N | large win when **N ≫ D** |
| **High** | UUID per vertex, D ≈ N | **η × N × (17 + V)** | **~η × N × (17 + V + 4)** — small win (fixed-width tail only) |

Symbols for planning (vertex property, one indexed field):

```text
D = distinct indexable values for that property (≤ N)

S_vprop_flat     = η × N × (17 + V)                    // today
S_vprop_inverted = η × (D × (17 + V) + N × (17 + 4))  // planned tail uses u32 ordinal
```

**Example:** N = 10^8, V = 20, D = 200 countries, η = 2:

- Flat: **~7.4 GiB**
- Inverted: **~2.0 GiB** (value bucket ~0.01 GiB + tail ~2 GiB)

Same pattern applies to **`EdgePostingKey`** (`23 + V` → value bucket + fixed tail).

Label postings (`13` B fixed) do not carry value bytes; this optimization does **not** apply to
`INDEX_LABEL_POSTINGS`.

### Read paths (must remain equivalent)

| Operation | Flat layout | Inverted layout |
|-----------|-------------|-----------------|
| `lookup_equal(p, v)` | range on `(p, v, …)` | find value bucket `(p, v)` → scan tail `(p, ord, …)` |
| `lookup_range(p, req)` | half-open on value prefix | walk value buckets in range → merge tails |
| `lookup_intersection` | per-arm ranges | per-arm value ranges + tail intersection (same asymptotics as multi-range today) |
| `count_postings_by_value` | walk property bucket | walk value buckets; count tail length per value |

### Costs and non-goals

- **DML:** property update moves `(shard_id, vertex_id)` between value buckets (remove old tail,
  insert new tail); value bucket GC when last tail removed.
- **Storage format for tails:** sorted `(shard_id, vertex_id)` arrays, delta batches, or bitmaps —
  choice is implementation detail; must support batched backfill under **2 GiB/message** limits.
- **Not a substitute for split:** high-cardinality indexed text/UUID still scales **O(N)**; use
  **CREATE INDEX** discipline, property-range split, or canister split from tables above.
- **Product guardrails (optional):** cap indexable `text`/`bytes` key length (e.g. reject
  `property_indexability` when `V > 64`) for equality indexes; full-text search remains out of
  scope.

### Status

**Planned** — no change to current `INDEX_POSTINGS` / `INDEX_EDGE_POSTINGS` layout yet. Tables
A–D in this document assume **flat keys** until an ADR lands and layout version bumps ([ADR
0007](../adr/0007-stable-memory-layout.md)). Rebuild path: derived backfill from graph canonical
properties (same as today).

---

## Metrics and verification (planned)

| Metric | Source | Use |
|--------|--------|-----|
| `INDEX_*` entry counts | canister query / admin | `S_index` estimate |
| Avg encoded key length | sample keys | refine **V** |
| Stable pages touched | canbench (`bench_layout*`) | regression on layout changes |
| Per-graph projected `S_index` | router registry + stats | pre-approve `CREATE INDEX` |

Until metrics ship, use **tables A–D** at provisioning time and re-estimate after major schema or `CREATE INDEX` changes.

---

## Related documents

- [property-index.md](property-index.md) — posting model, opt-in property index
- [label-index.md](label-index.md) — label postings scale with all labeled vertices
- [../adr/0006-pre-federation-foundation.md](../adr/0006-pre-federation-foundation.md) — shard registry, illustrative grouping
- [../adr/0010-index-sharding-extensibility.md](../adr/0010-index-sharding-extensibility.md) — defer split strategy; stable wire
- [../adr/0007-stable-memory-layout.md](../adr/0007-stable-memory-layout.md) — index region inventory
- [../adr/0009-edge-property-index-and-index-ddl.md](../adr/0009-edge-property-index-and-index-ddl.md) — `CREATE INDEX` registration
- [../storage/stable-memory-inventory.md](../storage/stable-memory-inventory.md) — derived rebuild paths
- [../sharding/federation-target.md](../sharding/federation-target.md) — router-owned index reads
