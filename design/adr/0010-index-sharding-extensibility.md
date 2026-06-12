# 0010. Index sharding extensibility (defer split strategy; stable wire)

Date: 2026-06-12  
Status: accepted  
Last revised: 2026-06-12

## Context

Gleaph today runs **standalone graph**: one graph shard (`ShardId(0)`), one graph-index
canister, postings tagged with `shard_id`. Federation types and registry fields already
anticipate **multiple graph shards** ([ADR 0006](0006-pre-federation-foundation.md),
[standalone-mode.md](../sharding/standalone-mode.md)).

[ADR 0006 §5](0006-pre-federation-foundation.md) introduced **index canister grouping** via
`group_index = shard_id / GROUP_SIZE`. [capacity-planning.md](../index/capacity-planning.md)
lists additional split axes (subject, property range, logical graph boundary). None of these
numeric policies or partition formulas are implemented; **`GROUP_SIZE` is not chosen**.

We want the same posture as graph sharding:

- **Keep multi-index extension points** in types and router resolution.
- **Do not commit** to a fixed split strategy at standalone time.
- **Implement index routing together with graph multi-shard**, without breaking posting keys,
  index Candid APIs, or `PostingHit` semantics.

### Problems if we commit too early

| Premature choice | Risk |
|------------------|------|
| Fix `GROUP_SIZE` now | Wrong capacity/regional split; registry migration |
| Embed `logical_graph_id` or `index_group_id` in **posting keys** | Stable layout break; per-entry overhead |
| Single global index Principal hard-coded in router | Multi-index requires wire/API churn |
| Assume one index per logical graph forever | Conflicts with shard-group or property-range splits |

### Non-goals (this ADR)

- Picking `GROUP_SIZE`, property bands, or subject-split layout.
- Implementing router multi-index fan-out or merge (graph multi-shard milestone).
- Index stable-memory layout changes (inverted posting lists — see
  [capacity-planning.md](../index/capacity-planning.md)).
- Subnet / cycle policy for many index canisters.

---

## Decision

### 1. Degenerate case mirrors graph standalone

| Layer | Standalone (today) | General federation case |
|-------|--------------------|-------------------------|
| Graph | 1 shard, `ShardId(0)` | `n` shards, contiguous local ids per registration policy |
| Index | 1 canister (`Principal`) | **`m` canisters**, `m` may be 1 or greater |
| Posting tag | `shard_id = 0` | **`shard_id` on every posting** (unchanged) |
| Router dispatch | single graph target | fan-out per participating shard |

**Standalone index is the degenerate case** (`m = 1`, one shard group, all postings on one
canister). No separate “standalone index mode” in wire types.

### 2. Stable invariants (must not break for multi-index)

These remain fixed across index sharding rollout:

| Invariant | Rationale |
|-----------|-----------|
| Index instance identity = **`Principal` only** (no numeric index id in posting keys) | ADR 0006; canister migration |
| Posting keys include **`shard_id` + local vertex id** (and edge tail for edge postings) | Router slices hits; graph owner checks |
| **`PostingHit { shard_id, vertex_id }`** on index read APIs | Router seed construction |
| Graph-index **read API args** stay value-oriented: `lookup_equal(property_id, encoded_value)`, `lookup_intersection(...)`, etc. | Planner/router unchanged at boundary |
| Graph DML → **`posting_insert/remove(..., shard_id, ...)`** on **that shard’s configured index** | `FederationRouting.index_canister` |
| Index **`admin_set_shard_owner(shard_id, graph_canister)`** per shard on each canister | Write authorization |
| Postings are **derived**; rebuild via backfill | Split = new canister + cursor replay, not key rewrite |

**Do not add** `logical_graph_id`, `index_group_id`, or split policy ids to posting key
encodings for sharding. Partition is expressed by **which canister holds which shard’s
postings**, not by key prefix.

### 3. Defer split strategy — router holds policy, not the formula

[ADR 0006 §5](0006-pre-federation-foundation.md) `shard_id / GROUP_SIZE` is an **example**
grouping function, not a committed constant. Acceptable future policies (choose later, possibly
per deployment):

- shard group (`GROUP_SIZE` or explicit ranges)
- one index per logical graph
- property-id band per index
- subject split (label vs vertex property vs edge property canisters)

**Router** is the only place that maps **`(logical_graph_name, query context)` → ordered list
of index `Principal`s** for reads. Graph shards map **`shard_id → index Principal`** for writes
(via `ShardRegistryEntry` / `FederationRouting`).

No formula is stored in graph-index stable memory.

### 4. Registry shape (already sufficient — do not narrow)

`ShardRegistryEntry` **already** carries per-shard:

```text
shard_id, graph_canister, index_canister, logical_graph_name
```

**Decision:** keep per-shard `index_canister` as the write-time SSOT. Do **not** replace with a
single graph-wide index field. Multiple shards may:

- share one index Principal (**`m = 1`**, today’s PocketIC / standalone), or
- point at different Principals (**`m > 1`**, future split).

This avoids a registry schema break when `m` increases.

### 5. Router read path — introduce resolution, not `shards[0]`

**Today (implementation debt):** `gql.rs` uses `shards[0].index_canister` for all lookups.

**Target (implement with graph multi-shard):**

```text
resolve_index_lookup_targets(logical_graph_name, shards) -> Vec<Principal>
  // dedupe + stable sort; today |Vec| == 1
```

- **Standalone:** all shards share one Principal → one target (same behavior).
- **Future:** multiple Principals → router fans out read APIs and **merges `PostingHit` lists**
  (union for equality; intersection merge policy deferred to split-strategy ADR).

**Constraint until merge is implemented:** registration policy SHOULD keep all shards of one
logical graph on **one** index canister when plans use **`lookup_intersection`**, OR router
rejects multi-target intersection with a clear error. Equality-only plans may use multi-target
merge earlier.

### 6. Graph write path — unchanged boundary

Each graph shard writes postings only to **`FederationRouting.index_canister`** (from router
registration). On index split:

1. Install new index canister(s).
2. `admin_set_shard_owner` on each index for moved shard ids.
3. Update **`index_canister`** on affected `ShardRegistryEntry` rows.
4. Backfill postings for moved shards.
5. Graph metadata **`FederationRouting.index_canister`** updated per shard.

No change to `posting_insert` argument shape.

### 7. What we implement with graph multi-shard (same milestone)

| Item | Breaking? |
|------|-----------|
| `resolve_index_lookup_targets` + dedupe | No |
| Router dispatch uses shard list’s index set (still one call if `m=1`) | No |
| Federation graph dispatch (`list_shards_for_graph`) | Already planned |
| Document registration invariant: intersection queries → single index Principal | No (policy) |
| Metrics hook: index Principal + shard list (for future capacity) | No |

**Not in the same milestone unless split strategy ADR exists:**

- Automatic split at 350 GiB
- `GROUP_SIZE` assignment
- Property-range or subject canisters

### 8. Capacity and layout optimizations stay orthogonal

[capacity-planning.md](../index/capacity-planning.md) thresholds and inverted posting-list
layout are **intra-canister** optimizations. They do not determine **`m`**. Splitting **`m`**
uses registry + backfill; inverted keys use a new **MemoryId / layout version** inside one
canister ([ADR 0007](0007-stable-memory-layout.md)), not a posting-key sharding prefix.

---

## Consequences

### Positive

- Standalone deployments remain valid without choosing `GROUP_SIZE`.
- Graph and index multi-shard roll out together with **one registry abstraction**.
- Posting keys and index Candid surface stay stable when **`m`** increases.
- Split strategy can follow measured capacity ([capacity-planning.md](../index/capacity-planning.md))
  without revisiting kernel wire types.

### Negative / cost

- Router must eventually implement **multi-index read merge** (equality union; intersection TBD).
- **`lookup_intersection` across canisters** is not free; policy may restrict to single index
  per graph until merge semantics are defined.
- Per-shard `index_canister` in registry is slightly redundant when **`m = 1`** (acceptable).

### Clarifies ADR 0006

- §5 grouping formula is **illustrative**; **`GROUP_SIZE` remains deferred** until a
  capacity-driven follow-up ADR or operator policy selects it.
- Rejected alternative “assign index numeric ids” unchanged — Principals + router resolution suffice.

---

## Alternatives considered

| Alternative | Why rejected |
|-------------|--------------|
| Fix `GROUP_SIZE = 1` permanently | Blocks meaningful federation without registry migration |
| `logical_graph_id` prefix in every posting key | Stable key break; redundant if canister is graph-scoped |
| Single graph-wide index field in registry | Schema break when shards map to different index Principals |
| Graph shards call index on query path | Conflicts with federation target; router owns reads |
| Defer all multi-index types until split | `ShardRegistryEntry.index_canister` and `shard_id` postings already commit to extensibility |

---

## Implementation checklist (graph multi-shard milestone)

1. **`resolve_index_lookup_targets`** in router (dedupe shard index Principals).
2. Replace **`shards[0].index_canister`** call sites in `gql.rs` / index client wiring.
3. Registration validation: document **single-index intersection** invariant for operators.
4. Tests: two graph shards, **same** index Principal (PocketIC) — parity with today.
5. Tests (optional): two shards, two index Principals, **equality** lookup merge only.
6. Update [federation-target.md](../sharding/federation-target.md) and [property-index.md](../index/property-index.md) — status **Partially Implemented** for routing.

Follow-up ADR (when needed): **index split strategy** — selects among capacity-planning axes,
defines intersection merge across Principals, may fix `GROUP_SIZE` or explicit ranges.

---

## References

- [0006 — Pre-federation foundation](0006-pre-federation-foundation.md) §5 (illustrative grouping)
- [0007 — Stable memory layout](0007-stable-memory-layout.md)
- [capacity-planning.md](../index/capacity-planning.md)
- [standalone-mode.md](../sharding/standalone-mode.md)
- [federation-target.md](../sharding/federation-target.md)
- [property-index.md](../index/property-index.md)
- `crates/router/src/types.rs` — `ShardRegistryEntry`
- `crates/graph/src/facade/stable/metadata.rs` — `FederationRouting`
- `crates/graph-index/src/key.rs` — `PostingKey`
