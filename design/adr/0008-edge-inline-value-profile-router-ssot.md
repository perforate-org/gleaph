# 0008. Edge payload profile schema: router SSOT and graph stable retirement

Date: 2026-06-12  
Status: accepted  
Last revised: 2026-07-03
Anchor timestamp: 2026-07-03 09:35:13 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-12 | Proposed; router-owned logical schema, graph `EDGE_PAYLOAD_PROFILES` retirement plan. |
| 2026-06-12 | Accepted; policy frozen pending implementation phases A–E in §6. |
| 2026-06-12 | Implemented phases A–E; router region 21 live; graph `EDGE_PAYLOAD_PROFILES` retired (41 regions). |
| 2026-07-01 | ADR 0034 Slice 20 + Slice 21 + Slice 22: store value is a versioned `EdgeInlineValueSchemaRecord` supporting admin `UnnamedProfile` entries and named scalar inline schemas; `ResolvedEdgeLabel` carries `inline_property_id` as a router-derived projection consumed by both reads and mutations. Graph encodes mutation values into the payload using the same shared scalar codec and never writes the inline property to sidecar state. Development stable data must be wiped when this format changes because backward compatibility is not maintained. |
| 2026-07-03 | ADR 0034 Slice 24: `EdgeInlineValueSchemaRecord` adds a named fixed-size inline STRUCT variant (`property_id`, declaration-ordered logical field specs `[name, scalar_type]`). Byte offsets, widths, and total width are deterministically derived; the physical profile is `EdgeInlineValueProfile::opaque_bytes(total_byte_width)`. Graph receives only the top-level inline property identity and the opaque physical profile in this slice; struct reads, mutation packing, and `COST BY` over struct fields remain planned. Development stable data must be wiped because the record format version is bumped and no legacy decode fallback is provided. |
| 2026-07-03 | ADR 0034 Slice 25: `ResolvedEdgeLabel` replaces `inline_property_id` with `inline_schema: Option<ResolvedInlineSchema>` (`None`, `Scalar { property_id }`, or `Struct { property_id, fields }`). Router derives per-field byte offsets and scalar profiles from the canonical `InlineStructLayout` and projects them as a read-only wire shape. Graph consumes this single Router-resolved projection in separate read and mutation paths: the query executor validates and decodes struct payloads into a declaration-ordered `Value::Record`, while the mutation executor rejects any write or removal on a Struct-labeled edge until Slice 26. Development stable data must be wiped when this format changes because backward compatibility is not maintained. |

## Context

ADR [0006](0006-pre-federation-foundation.md) §2 makes the **router** the single source of truth for
**name → numeric id** of vertex properties, vertex labels, and edge labels. Graph shards store
**values** keyed by resolved ids; they do not maintain label or property **names** in stable memory.

`EdgeInlineValueProfile` ([labeled-edge-inline-values.md](../storage/labeled-edge-inline-values.md)) is different
from a name catalog. It defines the **logical physical schema** for a catalog edge label:

- `byte_width` — bytes per edge slot in labeled LARA payload storage
- `EdgeInlineValueEncoding` — how to interpret those bytes (e.g. `RawU16`, `F32`, weight encodings)

Today this schema lives in graph stable memory as `EDGE_PAYLOAD_PROFILES` (MemoryId **37**,
`StableBTreeMap<EdgeLabelId, EdgeInlineValueProfile>`). Installation is **graph init-time only**
(`install_edge_label_payload_profile_at_init`). The router edge-label catalog (`ROUTER_EDGE_LABEL_*`)
stores **names and ids only** — no payload schema.

Phase 8 P1 retired the legacy `EDGE_WEIGHT_PROFILES` compatibility region; payload profiles are the
sole edge-profile stable region on the graph shard. That removed duplicate *weight* storage but not
duplicate *ownership* of schema between router (ids) and graph (profiles).

### Problems today

| Area | Issue |
|------|--------|
| **Split SSOT** | Router owns `EdgeLabelId` allocation; graph owns `EdgeLabelId → EdgeInlineValueProfile`. Federation requires every shard to agree on schema per id without a central registry. |
| **Wire gap** | `ResolvedEdgeLabel` carries `name`, `id`, `payload_profile`, and `inline_schema` ([`plan_exec.rs`](../../crates/graph-kernel/src/plan_exec.rs)). Graph execution consumes the router-derived scalar-or-struct projection and must not re-read graph stable for schema. |
| **Wildcard / fusion fallback** | When `label_expr` cannot decompose to explicit names, expand uses `GraphStore::edge_catalog_label_ids_with_payload_profiles()` — a **shard-local** enumeration of labels with installed profiles ([`label_expr.rs`](../../crates/graph/src/plan/query/executor/expand/label_expr.rs)). That list can diverge from router’s logical graph schema. |
| **Admin surface** | Tests and benches call graph-local `install_edge_label_*_at_init`; production path for schema registration is undefined at the router boundary. |
| **Stable overhead** | One extra graph facade region (ADR [0007](0007-stable-memory-layout.md) baseline: 42 regions) for data that is graph-wide logical metadata, not per-shard adjacency or property values. |

### What is *not* a problem (clarifications)

| Topic | Current behavior | Implication |
|-------|------------------|-------------|
| **Unlabeled edges** | `UNLABELED_*` wire labels map to `catalog_label = None`; payload width defaults to 0 ([`helpers.rs`](../../crates/graph/src/facade/store/helpers.rs)). | Unlabeled traversal does **not** require a payload-profile catalog on graph or router. |
| **LARA physical width** | `LabelBucket::payload_byte_width` is stored per orientation in forward/reverse CSR ([labeled-edge-inline-values.md](../storage/labeled-edge-inline-values.md)). | Physical width is **materialized** on the shard when edges are written; it is not a substitute for logical encoding semantics at query time. |
| **Property analogy** | Property **names** on router; property **values** on graph. | Payload profile is **schema**, not edge-local payload bytes. Schema belongs with router catalog policy; bytes stay in LARA payload stores. |

### Prerequisites (met)

- ADR 0006 — router catalog SSOT for label ids
- ADR 0007 — layout registry and benchmark gate for graph `MemoryId` changes
- Phase 3 `BidirectionalCatalog` on router for edge label names
- `EdgeInlineValueProfile` type in `graph-kernel` (shared, canister-agnostic)

### Non-goals (this ADR)

- Changing LARA payload slab layout (MemoryIds 0–31) or `LabelBucket` wire format
- Runtime mutation of payload schema after first edge insert (init-time registration remains)
- Reintroducing graph-local edge label **name** catalogs
- Production migration from pre-0008 stable snapshots (dev data discard per roadmap)

---

## Decision

### 1. Router owns logical edge inline value schema (SSOT)

The router is the authoritative store for:

```text
(GraphId, EdgeLabelId) → EdgeInlineValueSchemaRecord
```

for every catalog edge label in a logical graph. `EdgeInlineValueSchemaRecord` is a versioned envelope
that represents either an admin `UnnamedProfile`, a named scalar inline schema
(`property_id`, `scalar_type`, derived `EdgeInlineValueProfile`) (ADR 0034 Slice 20), or a named
fixed-size inline STRUCT schema (`property_id`, declaration-ordered logical field specs
`[name, scalar_type]`) (ADR 0034 Slice 24). Byte offsets, widths, total width, and the physical
`EdgeInlineValueProfile::opaque_bytes(...)` profile are deterministically derived from the canonical
record. Development stable data must be wiped when this format changes because backward compatibility is not maintained.

Registration is coupled to edge-label identity:

- **Preferred:** extend edge-label intern / admin APIs so registering or interning a catalog edge
  label also records its `EdgeInlineValueSchemaRecord` (default `UnnamedProfile` with `no_payload` / 0 bytes
  when omitted).
- **Alternative (rejected):** a parallel stable map `ROUTER_EDGE_PAYLOAD_PROFILES` keyed by `EdgeLabelId`,
  updated only through router `commit_*` catalog/schema APIs. Slice 20 keeps a single map and
  versioned value instead.

Router does **not** store per-edge inline value bytes — only the schema template per label id.

### 2. Wire schema on every graph-facing dispatch

Graph shards must not consult a local stable profile catalog in production paths.

Extend plan and DML wire tables (in `graph-kernel::plan_exec`) so graph execution receives schema
with resolved labels:

```rust
// Illustrative — exact shape chosen at implementation time
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
    pub payload_profile: EdgeInlineValueProfile, // router-filled physical profile
    pub inline_schema: Option<ResolvedInlineSchema>, // router-derived scalar or struct projection (Slice 25)
}

// `ResolvedInlineSchema` is an explicit enum with no parallel optional state:
// - `None`: no named inline slot.
// - `Scalar { property_id }`: one fixed-width scalar inline property.
// - `Struct { property_id, fields }`: one fixed-size inline struct with declaration-ordered physical fields.
pub enum ResolvedInlineSchema {
    Scalar { property_id: PropertyId },
    Struct { property_id: PropertyId, fields: Vec<ResolvedInlineStructField> },
}

pub struct ResolvedInlineStructField {
    pub name: String,
    pub byte_offset: u16,
    pub profile: EdgeInlineValueProfile,
}
```

Rules:

| Path | Requirement |
|------|-------------|
| **Query (`gql_query` / physical plan)** | Router `resolve_plan_labels` (or successor) fills `payload_profile` for every edge label named in the plan **plus** any labels required for wildcard/predicate-fusion enumeration (see §3). |
| **DML / mutation batch** | Same resolved table attached to the wire. Graph classifies edge assignments, requires and encodes the inline value (scalar or struct) before any canonical write, and never writes the inline property id to sidecar state. |
| **Standalone graph tests** | May inject a synthetic `ResolvedLabelTable` in memory; graph stable `EDGE_PAYLOAD_PROFILES` is not the production contract. |

Graph query execution reads profiles from `GqlExecutionContext` (heap, plan-scoped) — not from
stable memory.

### 3. Wildcard and predicate fusion without graph stable enumeration

Replace `edge_catalog_label_ids_with_payload_profiles()` stable fallback with router-supplied
candidates:

| `label_expr` shape | Label id source |
|--------------------|-----------------|
| Decomposes to explicit names | `resolved_labels.edge` from plan wire |
| Wildcard, negation, or other non-fusion expr | Router supplies **logical-graph** list: all `EdgeLabelId` with `payload_profile.required_byte_width() > 0` (or full catalog list when fusion needs topology-only labels) |
| Unlabeled physical edges | No profile lookup; 0-byte path |

The graph executor must not invent schema by scanning its own stable map.

### 4. Retire graph `EDGE_PAYLOAD_PROFILES` stable region

**Remove** from the graph canister:

- `EDGE_PAYLOAD_PROFILES` thread-local and `init_edge_inline_value_profiles`
- `edge_inline_value_profiles.rs` stable store module (or reduce to test-only helpers outside wasm)
- `install_edge_label_payload_profile_at_init` / `install_edge_label_weight_profile_at_init` as
  graph stable writers

**Retain on graph:**

- LARA payload bytes and `LabelBucket::payload_byte_width` (physical materialization)
- Validation that DML payload bytes match **wire-resolved** profile for the label
- Decode/prepare using plan-scoped profile from execution context (`GLEAPH.WEIGHT`, payload batch
  scans, etc.)

**Heap-only optional cache:** graph may cache `EdgeLabelId → EdgeInlineValueProfile` for the duration
of a single query or mutation batch (derived from wire). No persistence across upgrades.

### 5. Stable-memory layout changes (implementation patch)

Follow ADR 0007 gates (inventory + registry + reopen tests + canbench delta).

| Canister | Change |
|----------|--------|
| **Graph** | Remove MemoryId **37** (`EDGE_PAYLOAD_PROFILES`). Repack facade ids **38–41 → 37–40**. Region count **42 → 41** (32 LARA + 9 facade). |
| **Router** | Add stable region for edge inline value schemas (proposed MemoryId **21**, `ROUTER_EDGE_PAYLOAD_PROFILES`). The value type is a versioned `EdgeInlineValueSchemaRecord`. Region count **21 → 22**. |

Update `gleaph_graph_kernel::stable_layout`, `stable-memory-inventory.md`, and per-canister
`layout.rs` in the same patch as code.

### 6. Implementation phases

| Phase | Deliverable | Verification |
|-------|-------------|--------------|
| **A — Wire + router store** | Router schema map; extend `ResolvedEdgeLabel`; admin/intern sets profile | Router unit tests; catalog + schema consistency |
| **B — Graph read path** | Executor, `gleaph_weight`, expand fusion use execution context only | Existing expand/weight tests with wire injection |
| **C — DML path** | Mutations validate against wire profile; remove graph install APIs | Facade store tests; pocket-ic DML |
| **D — Stable retirement** | Delete graph region; repack ids; router region live | Reopen tests; `bench_layout_graph_stable_reopen_touch`; cold_touch **41** |
| **E — Doc sync** | `labeled-edge-inline-values.md`, ADR 0007 baseline table, roadmap | design-sync checklist |

Phases A–C may land before D (dual-read or feature flag) only in short-lived branches; **main** should not keep two SSOTs beyond one merge window.

---

## Consequences

### Positive

- Single federation-wide schema per `EdgeLabelId`; shards cannot silently diverge on encoding/width.
- Graph facade loses one canonical stable region; aligns with ADR 0006 “values on shard, ids/schema on router.”
- Wildcard fusion uses the same label set the router used to plan the query.
- Clear admin model: register edge label + payload schema once on router.

### Negative / costs

- Router dispatch must always attach schema (slightly larger plan/mutation wire).
- Graph canister isolated tests need explicit resolved tables instead of `install_*_at_init`.
- One-time dev stable wipe on graph MemoryId repack (acceptable per roadmap).

### Risks

| Risk | Mitigation |
|------|------------|
| Schema on wire stale vs router | Router is writer; graph rejects DML if label id unknown or profile mismatch |
| LARA bucket width ≠ router schema | Validate on first edge insert for label; refuse DML if bucket already materialized with different width |
| Missing profile for label in plan | Fail plan resolution at router (same as unknown label name today) |

---

## Alternatives considered

### A. Keep graph stable as materialized cache of router schema

Router SSOT + graph periodically syncs to `EDGE_PAYLOAD_PROFILES`.

**Rejected:** duplicates data, reintroduces drift, keeps an extra graph region without hot-path win
(profile lookup is cheap; wire + heap is sufficient).

### B. Derive schema only from LARA `LabelBucket::payload_byte_width`

**Rejected:** width alone loses encoding semantics (`RawU16` vs `F32` vs weight encodings); decode and
`GLEAPH.WEIGHT` need `EdgeInlineValueProfile`.

### C. Store profiles in graph-index or a new “schema canister”

**Rejected:** edge labels already router-owned; adding a fourth canister for a map keyed by
`EdgeLabelId` violates ADR 0006 catalog consolidation without a demonstrated need.

### D. Retain graph stable for “unlabeled / wildcard-only” queries

**Rejected:** unlabeled edges use 0-byte default without catalog access; wildcard enumeration is a
**logical graph** concern and belongs in router plan resolution, not shard-local stable state.

---

## References

- [0006 — Pre-federation foundation](0006-pre-federation-foundation.md) §2 Catalog ownership
- [0007 — Stable-memory layout](0007-stable-memory-layout.md) — repack gate
- [labeled-edge-inline-values.md](../storage/labeled-edge-inline-values.md) — LARA physical model
- [stable-memory-inventory.md](../storage/stable-memory-inventory.md) — current MemoryId tables
- [refactoring-roadmap.md](../architecture/refactoring-roadmap.md) — dev data discard policy
