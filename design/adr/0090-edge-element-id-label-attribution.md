# 0090. Edge element-id label attribution

Date: 2026-08-29
Status: proposed
Last revised: 2026-08-29

## Context

ADR 0005 § *Global edge identity (query-time)* defines
`GlobalEdgeId = (shard_id, owner_vertex_id, edge_slot_index)` (12 bytes) and uses it as the
canonical input to the 12-byte Feistel-encoded wire id (`EncodedEdgeId`). ADR 0005 § *Oversized
edge path ids* and § *Alternatives considered* record an explicit decision to keep the
encoded edge id at 12 bytes (the 16-byte alternative was rejected as wasteful) — so the 12-byte
layout is treated as a load-bearing contract.

The same ADR records that `edge_slot_index` is the **LARA CSR bucket-local position**
(`ic_stable_lara::traverse::BucketEntryPosition`, re-exported as `EdgeSlotIndex` in
`crates/graph-kernel/src/entry/edge/id.rs`). LARA buckets are keyed by `(owner, label)`, so
`edge_slot_index` is naturally **per (owner, label) bucket**: the slot index `0` under
`AUTHORED_BY` and the slot index `0` under `OWNS` for the same owner vertex are independent
rows in independent buckets.

The element-id encoder ignores the label entirely
(`crates/graph/src/plan/query/executor/path/materialize.rs:116` and `:145`; `eval.rs:1010`
singleton arm, `:1024` EdgeGroup arm, and the host test `hop_id` closure at `:2214`). It
takes an `EdgeHandle` whose `label_id` is already populated at the call site
(`edge_insert.rs:203,331`: `EdgeHandle::at_slot(source_vertex_id, label, location.logical_slot)`),
but the label is dropped at the `GraphPathEdgeId::new(...)` boundary.

Consequence: any two edges from the same source vertex under different labels with
coincident per-bucket slot indices produce **identical** `ELEMENT_ID(e)` bytes. Knowledge demo
validation surfaced this with 12 collisions out of 37 seeded edges (alice has `AUTHORED_BY
slot0`, `BELONGS_TO slot0`, `OWNS slot0`, `ROUTED_VIA slot0`, all encoding to
`(shard, alice, 0)`). The bug is not the data; it is the identity.

The Feistel encoding (`crates/graph-kernel/src/federation/encoded.rs`) is **not** the source
of the collision: it is a bijective map over its 12-byte input domain, so two distinct inputs
would always produce two distinct outputs. The collision is upstream of the bijection, in what
is fed into it.

## Problem

The wire-format element id is **not unique per edge** under the same owner across labels.
Callers that use the element id as a stable handle (e.g. the native graph explorer, which
dedups by element id and under-renders edges; future code that stores edge ids in client
state) see fewer distinct ids than edges exist. This violates the implicit
"encoded bytes identify exactly one edge under the per-graph key" contract that ADR 0005
§ *Encoded wire ids (client-facing)* advertises (bijection, deterministic, round-trippable
per element).

The collision is currently masked by two coincidental properties:

1. The per-bucket slot model means collisions are **only** between edges of the **same**
   owner vertex under **different** labels with **coincident** slot indices. A graph where
   every owner has at most one label (e.g. the original demo seed) is collision-free.
2. The 0306 / 0307 plans (authz attribution, group binding executor) made the singleton-arm
   encoding work end-to-end, so the collision stayed invisible at the authz and group-binding
   layers and only surfaced in the explorer load.

## Existing Architecture Assessment

Three options exist:

### Option A — Include the label in `GlobalEdgeId`

Identity becomes `(shard_id, owner_vertex_id, label_id, edge_slot_index)`. Wire layout grows
by the label's storage width.

- **Feasibility**: yes. The label is in scope at every existing call site
  (`handle.label_id` for path materialization, `edge.handle.label_id` for the executor arms)
  or is a constant `0` for the rare label-free reverse-edge case (already the default in
  `edge_insert.rs:179-181`). `EdgeLabelId` is `u16`; widening to `u32` in the canonical
  identity keeps the 4-byte alignment and the Feistel bijection clean (the bijection operates
  on `u32` halves after the wire-level key derivation; high 16 bits are zero on input and
  decode is masked).
- **Boundary impact**: this is a wire-format change. The CLI stderr warning
  (introduced 2026-08-25, see `plans/0306-edge-element-id-attribution.md`) already states
  that edge element ids are "not stable identifiers and may change over time" — the
  contradiction that an edge id might collide with another edge is the same kind of
  instability the warning covers, so no new client warning is needed.
- **Cost**: 4 extra bytes per `ELEMENT_ID(e)` value, 4 extra bytes per `GraphPathEdgeId`,
  one new field on `GlobalEdgeId`, the Feistel head/tail scheme updated to a 16-byte
  canonical form. ADR 0005's rejected 16-byte alternative is now the chosen form (see §
  *Alternatives considered* below).

### Option B — Make `edge_slot_index` globally unique per owner across all labels

Storage-local change: instead of `slot_index` being the bucket-local position, allocate
non-bucket slots and renumber.

- **Cost**: defeats the per-(owner, label) bucket locality that drives per-label scanning and
  tombstoning in LARA. Either the CSR itself has to abandon buckets (large refactor) or a
  parallel "global slot" table is maintained (every insert must reserve a global slot, every
  scan must translate from global to bucket slot).
- **Boundary impact**: storage layout changes, not just identity. Cross-crate contract
  surface expands. The bucket placement model is referenced by ADR 0001
  (labeled edge physical layer), ADR 0008 (edge inline property profile), ADR 0009 (edge
  property index DDL), and the LARA backend; all of these would have to be revisited.
- **Rejection rationale**: the right place to express cross-label uniqueness is the
  **identity**, not the **storage** layout. The bucket model is a storage property; the
  identity is a separate concern. Mixing them is a layer violation.

### Option C — Pack the label into the high bits of `edge_slot_index`

Keep 12 bytes; encode `label_id` into the unused high bits of the `u32` slot (e.g.
`slot_index | (label_id << 30)`).

- **Cost**: invisible to the bijection (a `u32` is a `u32`), but the slot is now a packed
  payload. The label-id room is 2 bits (the high bits of the slot are used for LARA CSR
  tombstones, see `VertexRef::tombstone()` at `crates/graph-kernel/src/entry/vertex_ref.rs`).
- **Boundary impact**: every u32-handle site that previously read a "pure" slot must mask
  out the high bits. Existing call sites (LARA internals, the journal, the index build
  admission) operate on raw slots, so this introduces a per-site decode obligation.
- **Rejection rationale**: conflates two semantically distinct fields in a single `u32`.
  Future expansion has no headroom (only 2 bits of label room — `EdgeLabelId` is `u16`,
  catalog-allocatable since ADR 0008; 2 bits covers 4 labels, the rest is unusable). Couples
  identity to a packing trick that future readers must know to undo.

## Decision

Adopt **Option A**: extend `GlobalEdgeId` to
`(shard_id, owner_vertex_id, label_id, edge_slot_index)` with
`label_id: EdgeLabelId` widened to `u32` in the canonical identity for 4-byte alignment, and
update the Feistel bijection to a 16-byte canonical form (head: 8-byte Feistel-4 over the
first half; tail: 8-byte XOR under a key-derived mask mixing the 4-byte key tail and the
encoded head).

Constants and wire layout:

```text
ENCODED_VERTEX_ID_BYTES = 8
ENCODED_EDGE_ID_BYTES   = 16   // changed (ADR 0005: 12)
GLOBAL_VERTEX_ID_BYTES  = 8
GLOBAL_EDGE_ID_BYTES    = 16   // changed (ADR 0005: 12)
```

`EncodedEdgeId([u8; 16])`, `GraphPathEdgeId` (which wraps `EncodedEdgeId`), and
`gleaph_gql::gql_params::EdgePathElementId([u8; 16])` grow accordingly. The SDK type
aliases used by the explorer and CLI presentations also grow.

`GraphPathEdgeId::new` gains a `label_id: EdgeLabelId` parameter; the 3-arg form is removed
(no production data to preserve; pre-production contract per the CLI warning; AGENTS.md
"Pre-production Simplicity" rule). All call sites in
`crates/graph/src/plan/query/executor/path/materialize.rs` and `eval.rs` read the label from
`EdgeHandle.label_id` (which is already populated at insert time). Remote-edge
`EdgeBinding::from_federated_neighbor_hit` defaults the label to `0` when the federated
expand hit has no resolved label, matching the existing label-free reverse-edge pattern in
`edge_insert.rs:179-181`.

`GlobalEdgeId` and `EncodedEdgeId` are not `Storable` (unchanged from ADR 0005); the 16-byte
canonical form does not enter stable storage, only wires. The 16-byte form is the
**fresh** identity decision; the prior 12-byte form is marked **SUPERSEDED** in ADR 0005
and is not retroactively rewritten.

### Synchronization

- `design/glossary.md` rows for `Global edge id` and `Encoded edge id` updated to 16 bytes.
- `design/adr/0005-vertex-identity.md`:
  - § *Identity layers* table: 12 → 16 bytes for `GlobalEdgeId` and `EncodedEdgeId`.
  - § *Global edge identity (query-time)* paragraph: 4-field description.
  - § *Encoded wire ids* constants block: 12 → 16 for the edge constants.
  - § *Oversized edge path ids* bullet: **removed** (the "12 bytes carry semantics, 4 bytes
    zero padding" observation is no longer true — there is no padding; the label is
    semantic content).
  - § *Alternatives considered* `### 16-byte EncodedEdgeId`: a footnote noting "the 12-byte
    form was the ADR-0005 decision; ADR 0090 re-adopts the 16-byte form as the fresh
    decision in pre-production; this row is preserved as the rejected-alternative record."
  - Status footer: `accepted` (unchanged; superseded decisions stay in the ADR).
- `design/adr/0006-pre-federation-foundation.md` § 4 client-facing ids table: 12 → 16
  bytes, cross-link to 0090.
- `design/adr/0019-graph-local-shard-id-and-index-clusters.md` § 3 layout sketch: 12 → 16
  bytes, cross-link to 0090.
- `design/execution/group-variables.md` ELEMENT_ID rules row: per-hop list still uses the
  same singleton encoding, now label-bearing; semantics unchanged; cross-link to 0090.
- `demo/knowledge/README.md` known-issue blockquote gains a dated 2026-08-29 update noting
  the fix; the 2026-08-25 CLI warning remains accurate (edge ids are still physical handles
  unstable across compaction; the fix only removes cross-label collisions, not
  cross-compaction identity).

## Consequences

### Positive

- Element ids become unique per edge under the per-graph key (within the unstable-handle
  contract). `ELEMENT_ID(e)` collisions across labels disappear; the explorer load and any
  client that stores edge ids in local state recover full distinctness.
- The 12 → 16 bump is the cleanest field addition; the bijection is preserved by construction
  (both halves of the 16-byte form are individually bijective on their input domains and the
  key is fixed per `ElementIdEncodingKey`).
- The 16-byte layout is no longer "oversized": every byte now carries semantics, so ADR
  0005's "12 bytes save wire space" claim is replaced with a defensible "16 bytes match
  information content" claim.
- Identity becomes a 4-tuple `(shard, owner, label, slot)`, which mirrors the storage
  bucket key exactly — the encoding is now a faithful externalization of the internal
  identity instead of a lossy projection.

### Negative / Migration

- **Wire-format breaking change** for `ELEMENT_ID(e)` and `PathElement::Edge` bytes (12 → 16).
  This is a **pre-production** change; the CLI warning documents edge ids as unstable
  ("not stable identifiers and may change over time"); no on-disk stable layout depends on
  the wire bytes (ADR 0005 § *Encoded wire ids* — encoded types do not implement `Storable`).
- Coordinated refactor across `gleaph-graph-kernel`, `gleaph-graph`, `gleaph-gql-params`
  (`EdgePathElementId`), the CLI, the explorer (presentation layer only), and the
  `knowledge_demo_citation_reach_flow` PocketIC test (which asserts hop-list entries
  non-empty; the byte width changes, the structural assertion is preserved by construction
  since the singleton and list arms share the encoder).
- The `GraphPathEdgeId::new` signature change is a **breaking compile-time** change. All call
  sites are in-workspace; no external SDK consumers at this point.
- The remote-edge default-label `0` introduces an ambiguity if a future federated expand hit
  genuinely has no resolvable label and another edge in the same bucket slot 0 has label 0.
  This is bounded by the existing label-free reverse-edge pattern; if it becomes a real
  collision source, a follow-up slice introduces a sentinel label or threads the resolved
  label through the hit.

### Implementation status

| Item                                                                  | Status      |
| --------------------------------------------------------------------- | ----------- |
| ADR and type definitions in `graph-kernel`                            | proposed    |
| `GraphPathEdgeId::new` 4-arg form and call-site rewrites              | proposed    |
| Feistel head/tail updated to 16-byte form                             | proposed    |
| Host-level contract tests (`edge_element_id_*`)                        | proposed    |
| PocketIC target against knowledge demo seed (`37 unique` assertion)   | proposed    |
| Glossary / 0005 / 0006 / 0019 / group-variables / demo README updates | proposed    |
| GAP ledger entry added; mark RESOLVED on completion                   | proposed    |

## Alternatives considered

### Option B — Globally unique per-owner slot

Rejected. See § *Existing Architecture Assessment* Option B. Layer violation: storage bucket
locality is a storage property; cross-label uniqueness is an identity property. Reshaping the
storage layout to express an identity property inverts the dependency direction.

### Option C — Pack the label into the high bits of `edge_slot_index`

Rejected. See § *Existing Architecture Assessment* Option C. Couples identity to a packing
trick; leaves 2 bits of label room (the LARA CSR tombstone bit and the high reserved bit
already occupy the high bits of the slot); forces every u32-handle site that previously
read a pure slot to mask.

### Add a post-hoc disambiguation layer (e.g. shadow table mapping `(shard, owner, slot)` → `label`)

Rejected. The wire form is the only thing the client sees; a shadow table would need to be
either client-side (every client rebuilds the table — burden duplication) or
server-side-with-extra-roundtrip (the element id read now needs a second call to look up the
label). Both defeat the single-roundtrip property of `ELEMENT_ID(e)`.

### Change only the path-id form; keep `GlobalEdgeId` 12 bytes

Rejected. `GlobalEdgeId` and `EncodedEdgeId` are the same identity; the wire form is a
bijection of the canonical form. Reshaping only one is incoherent — clients would receive
`EncodedEdgeId` (16 bytes) that decodes to a `GlobalEdgeId` (12 bytes) which cannot
distinguish labels. The two move together.

## References

- `crates/graph/src/plan/query/executor/path/materialize.rs:116,145` — encoder and
  path-materialization call sites (drop `label_id` at the boundary).
- `crates/graph/src/plan/query/executor/eval.rs:1010,1024,2214` — singleton, EdgeGroup,
  and host-test helper call sites.
- `crates/graph/src/facade/store/edge_insert.rs:203,331` — `EdgeHandle::at_slot` already
  populates `label_id` at the insert boundary.
- `crates/graph-kernel/src/federation/global_edge_id.rs` — `GlobalEdgeId` 3-field
  canonical form.
- `crates/graph-kernel/src/federation/encoded.rs:94-128` — 12-byte Feistel encoder and
  decoder.
- `crates/graph-kernel/src/path.rs:78-128` — `GraphPathEdgeId` 3-arg constructor.
- `crates/graph-kernel/src/entry/edge/id.rs` — `EdgeSlotIndex` re-export
  (`ic_stable_lara::traverse::BucketEntryPosition`); slot is per `(owner, label)` bucket.
- `design/adr/0005-vertex-identity.md` — prior identity decision (amended).
- `design/adr/0006-pre-federation-foundation.md` § 4 — pre-federation client-facing ids.
- `design/adr/0019-graph-local-shard-id-and-index-clusters.md` § 3 — graph-local layout.
- `design/glossary.md` — `Global edge id` / `Encoded edge id` rows.
- `design/execution/group-variables.md` — ELEMENT_ID expression rules.
- `plans/0306-edge-element-id-attribution.md` — authz contract (distinct from this ADR).
- `plans/0307-group-element-id.md` — executor implementation for `ELEMENT_ID` on group
  bindings (uses the same singleton encoder; inherits the fix automatically).
- `demo/knowledge/README.md` — known-issue blockquote.
