# Graph Canister Persistence Audit — Capacity, Compatibility, Redundancy

Status: Investigation complete; improvement candidates are proposals, not implemented.
Anchor timestamp: 2026-08-24 01:11:07 UTC +0000 (OS anchor; all claims verified against the tree as of this date).
Scope: the graph canister's stable-memory state only (`crates/graph`, regions 0–52, plus its direct dependencies in `crates/ic-stable-lara`, `crates/gql-value`, `crates/graph-kernel`). Router, graph-index, vector-canister, provision, and account internals are out of scope (see §8).
Method: static code reading with file:line evidence. Line numbers were accurate at the anchor date; re-verify at the cited site before implementing any candidate.

## 1. Purpose and evaluation criteria

The audit asks whether the graph canister's persistent data has capacity waste,
future-compatibility gaps, or redundant storage. Evaluation criteria (finalized
2026-08-24 with the project owner):

- **A. Record encoding efficiency** — field widths ≤ domain bounds; LARA's
  hand-packed rows (u40 offsets, packed words) are the reference standard;
  check whether non-LARA stores meet an equivalent bar.
- **B. Key design** — key width, per-row repetition of identity fields,
  sort-order/locality rationale, bounded vs unbounded choice.
- **C. Region-granularity slack** — page quanta × region count; measurable via
  existing admin stats.
- **D. Versioning / compatibility** — interpreted under the pre-production
  policy ([ADR 0007 §5](../adr/0007-stable-memory-layout.md): reinstall when the
  canonical layout changes; no backward-compatibility branches):
  - **D1'** Records that must survive into production are shaped so a future
    schema revision is wire-additive today: an enum envelope
    (`Record::V1(Inner)`) or an explicit version byte/magic checked on open.
  - **D2'** Single canonical variant during pre-production: no V2, no retained
    panic-on-decode legacy variants inside this canister.
  - **D3'** Layout-mismatch detection stays at the minimum necessary:
    container/library headers, exact-length asserts, version bytes where they
    already exist. Strict per-region detection is not required now.
  - **D4'** The compatibility-relevant vs disposable split must be explicit
    somewhere (registry or inventory), so the production gate
    ([ADR 0039](../adr/0039-production-stable-memory-evolution-and-upgrade-safety.md))
    has a checklist.
- **E. Redundancy** — every duplicate copy of logical data is classified
  (canonical vs derived vs maintenance), has one update path from its canonical
  source, and has a rebuild oracle when derived; unjustified duplication is a
  finding.
- **F. Durability inverse-check** — data that should be durable is not
  heap-only without a recovery path; tombstone/GC states have reclamation
  policies.
- **G. Documentation sync** — inventory/design docs mirror the typed registry.

## 2. Verdict summary

| # | Finding | Criterion | Priority |
|---|---------|-----------|----------|
| F1 | Canonical value encodings lack version guards: `StoredPropertyValue` (33/34), `VertexLabelSetBlob` (32); `CanonicalExportRecord` (51) is explicitly unversioned | D1' | **P2** — **closed**: 32/33/34 by [Plan 0296](../../plans/0296-graph-facade-value-envelopes.md), 51 by [Plan 0301](../../plans/0301-canonical-export-record-envelope.md) |
| F2 | Edge row payload is 30-bit (~1.07e9 vertices/shard) while `VertexId` is u32 and slab indices are 36-bit — undocumented shard-capacity ceiling | A/D | **P2** — **resolved** by [Plan 0297](../../plans/0297-lara-edge-payload-ceiling.md): bound documented as blessed-final (`design/storage/lara.md` § Shard capacity bounds; dgap-contract cross-reference) and enforced fail-closed at the minting constructors (`VertexRef::local` / `RemoteVertexId::from_raw`) with pinned boundary tests |
| F3 | No explicit compatibility-relevance marker per region/record for the production gate | D4' | **P2** — **closed** by [Plan 0298](../../plans/0298-registry-compat-column.md), 2026-08-24: every `GRAPH_STABLE_LAYOUT` entry carries an explicit `ProductionCompat` value (the registry is the SSOT); see §3 notes and the §8 follow-up for regions 41/46 |
| F4 | Free-span record stride 48 B carries 14 B padding and a derivable `bin_idx`; ~30% record-area shrink possible at next layout break | A | P3 |
| F5 | `LabelBucket` word bits 60–62 are neither assigned nor validated (only bit 63 is zero-checked) — **closed by [Plan 0299](../../plans/0299-lara-validation-hardening.md)**: decode now rejects the whole top nibble (bits 60–63) | D3' hygiene | P3 ✓ |
| F6 | Label-stats delta event duplicates `shard_event_seq` (8 B/event) already present as the map key | E/A | P3 |
| F7 | Journal primary reserves four fixed optional slots (28 B always resident) behind a validity bitmap | A | P3 |
| F8 | LARA header reserved-byte policy is inconsistent (some zeroed+checked, some declared but not written) — **closed by [Plan 0299](../../plans/0299-lara-validation-hardening.md)**: every store zero-fills its declared tail via a shared helper and rejects nonzero reserved bytes at open with precise variants (`InvalidLayout` where present; dedicated `ReservedRegionNonZero` in vertex / edge log / inline-prop log / counts / span-meta) | D3' hygiene | P3 ✓ |
| F9 | Dead code: `PropertyCatalog` type alias never instantiated (`facade/stable/property_catalog.rs`) — **closed by [Plan 0300](../../plans/0300-facade-hygiene-rider.md)**: alias and the unused line-4 re-export deleted with grep evidence; `PropertyCatalogError` kept | G/cleanup | P3 ✓ |
| F10 | `design/storage/stable-memory-inventory.md` checklist table drifted from registries (Graph row stale; Provision row stale) | G | P3 (fixed in this patch) |
| F11 | Mixed endianness across facade keys (BE for ordered map keys 32–34/52; LE for `EffectKey`, `PhysicalIndexId`, manual codecs) — **closed by [Plan 0300](../../plans/0300-facade-hygiene-rider.md)**: the three ordering conventions are now normative in the `facade/stable` module docs with region examples and new-store guidance | B hygiene | P4 ✓ |
| F12 | Derivable persisted fields: slab header `tree_height` (from `segment_count`), `num_edges` u64 wider than its 2^36 cap, counts `len` pinned to `2×segment_count` | A/E footnote | P4 |

Compliant-by-evidence verdicts (no action): LARA bit-level discipline is tight
where it matters (§4.1); version envelopes already exist for regions 36/39/42/43
(D1'); reverse adjacency, postings payload copies, and multi-level count tallies
are justified derived state with update paths/oracles (E); heap-only pending
queues have a proven durable recovery path (F).

## 3. Compatibility classification by region (D1'/D4')

Production-survival class: **C** = canonical business data that must survive
into production; **O** = operational/transient (journals, queues, projections);
**T** = telemetry.

| Regions | Content | Encoding / guard | Class | D1' verdict |
|---------|---------|------------------|-------|-------------|
| 0–14 | Forward LARA bundle | hand-packed slabs; magic+version+stride validated at open, init traps with actionable message (`crates/graph/src/facade/stable/memory.rs:273-282`) | C | guarded ✓ |
| 15–29 | Reverse LARA bundle (derived) | same guards | O (derived) | guarded ✓ |
| 30 | maintenance queue | `[0x5A][tag][version]` framing, unknown pairs rejected (`ic-stable-lara/src/labeled/deferred.rs`) | O | guarded ✓ |
| 32 | vertex label sets | `[v1 u8 = 1][sorted, deduped u16 LE label ids...]` versioned envelope (`facade/stable/vertex_labels.rs`); unknown versions and missing/truncated payloads panic at decode | C | ✓ closed by [Plan 0296](../../plans/0296-graph-facade-value-envelopes.md) |
| 33/34 | vertex/edge properties | `[v1 u8 = 1][gql-value binary bytes]` versioned envelope (`StoredPropertyValue::V1`); unknown versions and truncated payloads panic at decode | C | ✓ closed by [Plan 0296](../../plans/0296-graph-facade-value-envelopes.md) |
| 36 | graph metadata | `enum GraphMetadata::V1(..)`, Candid ✓ | C | ✓ |
| 37/38 | label stats seq/log | library Cell header / 1-byte `LABEL_DELTA_LAYOUT_VERSION=1`, panics on mismatch | T | minimum ✓ |
| 39 | mutation journal | `JOURNAL_LAYOUT_VERSION=1` + Rust `V1` wrapper; unknown versions/tags rejected | C (retention-bounded 9 days, ADR 0027) | ✓ |
| 40 | pending vertex purges | library roaring format; documented rebuildable | O | minimum ✓ |
| 41 | repair journal | bare Candid; any shape change = decode panic (fail-closed) | O | acceptable by policy |
| 42 | unique effect outbox | `UniqueEffectStableRecord::V1` ✓ | C (commit evidence until ack) | ✓ |
| 43 | local unique values | `LocalUniqueStableRecord::V1` ✓ | C | ✓ |
| 46 | derived index outbox | private Candid state enum acts as tag; shape change = panic | O | acceptable by policy |
| 51 | canonical export scopes | `CanonicalExportStableRecord::V1` Candid envelope (Plan 0301): rows survive upgrade windows byte-exactly; registry compat flipped to `VersionedSurvivor` — the prior regenerate-on-change contract was retired after investigation showed lifecycle watermarks are not re-derivable and the required procedure was never documented | C | ✓ closed by [Plan 0301](../../plans/0301-canonical-export-record-envelope.md) |
| 52 | index pending floor | fixed 17 B key, exact-length + owner-tag asserts; pure projection | O (derived) | ✓ |

**D2'** — the graph canister currently persists exactly one wire variant per
record type (all `V1`); compliant.

**D3' residual silent-misparse holes** (candidates for cheap hardening):
1. `VertexLabelSetBlob` even-length assert passes garbage after any width
   change (`vertex_labels.rs:38-47`). *(Closed for region 32 by the v1
   envelope, Plan 0296: a width change now trips the version byte or even-width
   check.)*
2. `LabelBucket` packed word bits 60–62 unvalidated
   (`ic-stable-lara/src/labeled/slot_index.rs:10-15`, checked only bit 63 at
   `record.rs:393-395`).
3. gql-value tag-space renumbering would silently reinterpret persisted bytes
   (theoretical; tags have been stable).

## 4. Capacity findings (A/B/C)

### 4.1 Where the LARA standard is met (reference register)

Exact-fit examples worth preserving as the review baseline: bucket-descriptor
slack u16 ↔ `MAX_VERTEX_LABEL_BUCKETS` = 65 536; 15-bit bucket label ↔
`EDGE_LABEL_CATALOG_MAX = 0x7FFF`; u40 value-slab offsets ↔ `BYTE_OFFSET_BITS`
= 40 (1 TiB); 36-bit slot base ↔ `SLOT_INDEX_BITS`; bypass log-head u8 ↔
enforced max 170 entries. Vertex row 21 B and bucket row 29 B have zero interior
padding; the edge row is 4 B total (topology-only; label/slot/bytes recovered
via the bucket layer, `graph-kernel/src/entry/edge.rs:84-109`).

### 4.2 Waste candidates inside LARA

- **Free-span records**: 48 B stride = start u64 + len u64 + prev/next u64 +
  flags u8 + bin_idx u8 + **14 B unused** (`lara/edge/free_span.rs:52-59`);
  `bin_idx` ≡ `size_class(len)` (decode validates agreement, :456-462). Six
  allocator pairs carry this layout (regions 2/3, 8/9, 11/12, 17/18, 23/24,
  26/27).
- **EDGE_COUNTS rows**: two i64 counters (16 B) though both domains are bounded
  by the 36-bit slot space (~≤ u48 needed) (`lara/edge/counts.rs:83-107`).
  Semantically derivable from bucket degrees + span geometry; kept as
  incremental PMA-density caches with reopen length pinning
  (`lara/edge/init.rs:123-125`).
- **Header reserved-byte inconsistency**: edge/bucket/value-slab headers zero
  their 16–28 B tails and some check them; the vertex store declares 52 B
  reserved but does not write it (`lara/vertex.rs:28, 570-576`); counts/span-meta
  declare-but-don't-zero similar tails. Unifying on zero-fill + open-time check
  buys free corruption detection.
- **Derivable persisted header fields**: `tree_height` (≡ floor_log2 of
  segment_count, `edges.rs:395-399`), `num_edges` as u64 despite the 2^36
  capacity cap (`edges.rs:204-206`), free-span summary fields
  (`active_count/free_bytes/largest_free_span`) with lazy repair paths.
  Header-only; negligible absolute cost; listed for completeness.

### 4.3 Facade encoding overheads (A2)

- gql-value codec is otherwise efficient: +1 tag byte/value; null = 1 B; Text/
  Bytes/List/Record pay one u32 LE length prefix; nesting capped at 64
  (`impls.rs:265-282, 556-872`).
- Mutation journal primary (52 B): first-seq/last-seq/recorded-at/next-index are
  **fixed slots always written, zero-filled when unused**, gated by a validity
  bitmap — up to 28 B resident overhead per entry for codec simplicity
  (`facade/stable/label_stats_delta.rs:605-725`). Retention-bounded (9-day
  window), so absolute waste is small.
- Label-stats delta events store `shard_event_seq` (8 B) inside the value while
  it is also the map key (`label_stats_delta.rs:958-1002`).
- Unique-effect receipts repeat both `EffectKey` fields inside the Candid value
  (`unique_effect_outbox.rs:24-86`, `graph-kernel/src/federation/unique_effect.rs`).

### 4.4 Keys (B)

- `EdgePropertyKey` = owner_vertex u32 + label u16 + slot_index u32 + property
  u32 (14 B BE, prefix-range ordered). The 10 B identity triple repeats once per
  property of the same edge; grouping into one row per edge would trade point-
  lookup/range ergonomics for density — a benchmark-gated redesign candidate,
  not recommended casually (ADR 0007 culture).
- Endianness convention is mixed (BE where byte order = numeric order for map
  keys; LE inside manual codecs and `EffectKey`/`PhysicalIndexId` where struct
  `Ord` governs). Functionally correct; worth one paragraph in the inventory to
  prevent future bugs.

### 4.5 Region-granularity slack (C)

Per-region bucket quanta are explicit and persisted
(`facade/stable/memory.rs:139-176`: 8p vertices/buckets, 16p edges/logs/journals,
32p value slabs, 64p property/unique stores; default 4p).
`admin_stable_memory_stats` exposes per-region `logical_pages`,
`allocated_pages` (bucket-rounded), and `slack_pages` (`memory.rs:296-446`).
53 regions each cost ≥1 virtual page once initialized; empty-state footprint is
already pinned by the layout cold-touch benches. Recommendation: capture
`slack_pages` totals periodically on a representative dataset rather than add a
new test; revisit quanta only with that evidence.

## 5. Redundancy register (E)

**Justified (single update path + oracle or clear role):**

- **Reverse orientation (15–29)** duplicates the forward bundle wholesale as
  Derived state; diff-based repair oracle `rebuild_reverse_adjacency`
  (`facade/derived_state/reverse_adjacency.rs:165-231`) reads forward edges and
  repairs diverged keys only. Cost: up to ~50% of the LARA bundle. Keep; see
  P4 quantification item.
- **Postings `payload_bytes`** duplicate canonical property values so the index
  canister receives sortable keys; module docs state the design intent
  (`index/pending.rs:1-26`). Failed-flush copies persist again in journal 41 /
  outbox 46, mirrored by exact floor keys in 52 — each copy is the durability
  mechanism itself, removed on ack/drain.
- **Multi-level degree tallies** (bucket.degree → vertex degree/bucket-row
  count → EDGE_COUNTS.actual → header num_edges) and stored-slots tallies are
  maintained caches enabling O(1) reads and PMA density decisions; all updated
  inside single LARA operations. Note: the bucket slab overloads the shared
  slab header's `num_edges` u64 as its own row-occupancy watermark
  (`bucket_store.rs:484-497`) — same bytes, different meaning per store; keep
  this documented to avoid future confusion.
- **Overflow-log heads** appear three times (referencing-row copy, log-internal
  allocation watermark, `prev` chain ordering); each serves a distinct access
  path.
- **Primary-label duplication** into the LARA vertex row (hint + sidecar bit)
  serves traversal fast paths (`vertex_labels.rs:113-126`).

**Needs documentation/decision:**

- Regions 42 vs 43 overlap for ShardLocalGlobal constraints: 43 is the declared
  single source of truth (`local_unique.rs:1-11`); 42 receipts independently
  carry constraint_id + encoded_value + owner_element_id as pinned commit
  evidence. Roles differ (uniqueness enforcement vs commit evidence), but the
  release path must consume 43, not 42 — verify and state this invariant in the
  inventory.

## 6. Durability and GC checks (F)

- **Heap-only pending queues** (`index/pending.rs:66-68`, `edge_pending.rs`,
  `label_pending.rs`, `vector_pending.rs`): lost on upgrade by design; recovery
  is journal-41/outbox-46 replay through the maintenance timer, which
  `post_upgrade` re-arms and `durable_delivery_pending` gates
  (`canister/handlers.rs:132-141`, `maintenance_timer.rs:136-159, 215-291`).
  Committed mutations land deltas durably before returning; volatile batches on
  first delivery failure are compensated then journaled
  (`pending.rs:30-36`). Verdict: sound; no action.
- **Tombstone/GC states**: purge bitmap 40 is gate-first marked and cleared by
  the purge-completion observer; journal entries persist `Retired{at_ns}` until
  the 9-day window passes (`label_stats_delta.rs:1194-1213`); outbox quarantine
  parks failed rows durably with retained floor keys until acknowledgment. All
  states have reclamation paths. Verdict: sound.

## 7. Improvement candidates (priority-ordered)

Gate for anything touching hot layouts: focused canbench before/after per
[ADR 0007 §6](../adr/0007-stable-memory-layout.md); pre-production permits fresh
state, so P3 items below should piggyback on the next forced layout break rather
than motivate one.

| ID | Candidate | Priority | Notes |
|----|-----------|----------|-------|
| C1 | Add `Record::V1(...)` envelopes (or a version byte) to regions 32, 33/34 values; confirm 51's regenerate-on-change contract is the intended production answer or envelope it too. Wire-additive today, no behavior change | **P2** | **Fully closed**: 32/33/34 by [Plan 0296](../../plans/0296-graph-facade-value-envelopes.md); 51 by [Plan 0301](../../plans/0301-canonical-export-record-envelope.md) (2026-08-24) — investigation found lifecycle watermarks non-re-derivable and the regeneration procedure undocumented, so the envelope path was chosen and registry compat flipped to `VersionedSurvivor` |
| C2 | Document the 30-bit edge-payload ceiling as an explicit shard-capacity bound (~1.07e9 local vertex refs) in `design/storage/lara.md` / dgap contract, and either bless it as final or schedule widening | **P2** | closes F2; pure documentation + decision — **Implemented** ([Plan 0297](../../plans/0297-lara-edge-payload-ceiling.md), 2026-08-24): documented + enforced fail-closed; **blessed as final**, widening only via a future product requirement and a new layout ADR (8-byte rows) |
| C3 | Extend the stable-layout registry (or inventory table) with a compatibility-relevance column so the production migration checklist is mechanical | **P2** | closes F3 (D4') — **Implemented** ([Plan 0298](../../plans/0298-registry-compat-column.md), 2026-08-24): registry entries gained a required `ProductionCompat` field; all 53 graph regions classified per §3 (41/46 deliberately `VersionedSurvivor` despite bare Candid, exposing the missing-envelope gap); router/graph-index/vector/provision carry `Unaudited` placeholders until per-canister slices |
| C4 | Hygiene batch: zero-check bucket word bits 60–62; unify LARA header reserved-byte policy (zero-fill + validate); delete dead `PropertyCatalog` alias; add the endianness-convention note | P3 | **Fully implemented**: F5/F8 by [Plan 0299](../../plans/0299-lara-validation-hardening.md), F9/F11 by [Plan 0300](../../plans/0300-facade-hygiene-rider.md) (both 2026-08-24, review approved) |
| C5 | At the next layout-breaking reinstall window: shrink free-span stride (drop padding + derivable bin_idx), drop duplicated `shard_event_seq` from delta values, compact journal optional slots, narrow EDGE_COUNTS counters | P3 | closes F4/F6/F7/F12; each needs the §7 gate if measured hot |
| C6 | Capture reverse-mirror share of LARA bytes via `admin_stable_memory_stats` on a representative workload; record the number here before anyone proposes removing/merging orientations | P4 | informs F-verdict longevity |

## 8. Out-of-scope follow-ups (recorded, not audited)

- **Router registry drift**: runtime allocates MemoryIds 54
  (`ROUTER_INDEX_RETIRED`) and 55 (`ROUTER_AUTH_GRANT_ROWS`) in
  `crates/router/src/facade/stable/memory.rs:53,145`, while
  `ROUTER_STABLE_LAYOUT` and its test pin max id 53 / 54 regions
  (`graph-kernel/src/stable_layout.rs:1772-1840`). **Closed by
  [Plan 0304](../../plans/0304-router-registry-drift-reconciliation.md)**
  (2026-08-25): the registry carries both regions with concrete
  classifications (54 `VersionedSurvivor` deliberately exposing its bare-Candid
  envelope gap — envelope work is a separate router slice; 55 `VersionedSurvivor`
  satisfied by `GrantRow`'s tag codec), pins updated to 56 regions / max id 55,
  ADR 0007 synced. Residuals: the MemoryId 54 envelope itself, and the
  inventory sync deferred behind the vector pane's unstaged Status-header
  hunks.
- **Cross-canister duplication study** (shard↔canister catalogs on three
  canisters; Router bulk-load receipt fingerprints) requires the neighbor
  scopes; deferred per audit scope decision.
- **Router vertex-index namespace uniqueness vs label scope** (reported
  2026-08-24 by knowledge-demo bring-up, GAP owned there): a graph with
  multiple label-scoped indexes on one property name (`Concept.name` +
  `Person.name`, migration 000002) makes label-unbound property resolution
  fail closed at prepared-query time with `InvalidArgument("multiple active
  vertex property index namespaces for property N")` — verified intentional
  per `indexed_catalog.rs:215-217` ("selecting an arbitrary label would
  silently return false negatives"). Labeled resolution resolves uniquely;
  only unbound plans hit the guard. Open design decision belongs to the
  Router/index-DDL domain: whether DDL admission should reject same-property
  cross-label index sets, or planner must bind labels, leaving runtime
  rejection as last resort. Not covered by this audit's slices (Router was
  out of scope); tracked as a GAP record by the demo pane.
- **`ic-stable-text-postings`** is workspace-built but depended on by nothing as
  of 2026-08-24; decide retain-vs-delete when the text index research lands.
- **Graph regions 41/46 lack value envelopes** (`INDEX_REPAIR_JOURNAL`,
  `DERIVED_INDEX_OUTBOX`): bare Candid today, so any record-shape change
  decode-panics across an upgrade window. The Plan 0298 compat classification
  records both as `VersionedSurvivor` on purpose — their live rows
  (failed-flush repair postings, undelivered outbox operations) span upgrade
  windows and are not re-derivable — which exposes this gap instead of papering
  over it with a rebuildable label. **Closed by
  [Plan 0302](../../plans/0302-repair-journal-and-outbox-envelopes.md)**
  (2026-08-24): both regions persist behind `RepairJournalStableRecord::V1` /
  `DerivedIndexOutboxStableRecord::V1`, completing versioned-envelope
  discipline for every `VersionedSurvivor` region in the graph registry.
- Vector-canister record versioning (e.g., multi-variant defs) is governed by
  its own scope; the D2' single-variant rule stated here applies graph-side.

## 9. Documentation sync performed in this patch

- `design/storage/stable-memory-inventory.md` checklist table corrected to the
  typed-registry facts: Graph 53 regions (0–52), Provision 13 regions (0–12);
  both previously stale against their registry tests.
