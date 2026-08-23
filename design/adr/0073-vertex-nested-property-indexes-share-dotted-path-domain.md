# 0073. Vertex nested-record property indexes share the edge dotted-path leaf domain

Date: 2026-08-22
Status: accepted — slices 1–3 implemented and validated (2026-08-23); slice 4 landed on main, acceptance pending Plan 0285 validation
Last revised: 2026-08-23
Anchor timestamp: 2026-08-23 09:35:24 UTC +0000

> Implementation progress (2026-08-23): slices 1–3 are implemented and terminal-validated
> (focused unit gates, all-target check/clippy for graph/graph-index, focused canbenches,
> posting-level PocketIC lifecycle, and an independent detached-worktree run at the
> `aa7fd9124` baseline plus only the remediation patch). Slice 4 (planner anchors, Router
> seed probes, cross-shard GQL proof `router_gql_query.rs::
> federated_vertex_nested_leaf_index_match_equality_and_range`) is present on main since
> `39746f7b3` but its acceptance stays pending until Plan 0285's bounded contracts pass;
> do not cite it as proven. GAP-2026-07-29-005 remains open for slice-4 acceptance.
> The runtime contract notes below describe implemented behavior.

## Context

Facts verified against current main (2026-08-22):

- **Edge INLINE struct leaves already have a dotted-path domain.** The Router interns each
  indexed dotted leaf path as its own property identity; the catalog membership carries
  `field_path` to identify the decoded leaf, and Graph decodes that leaf from canonical inline
  bytes for posting maintenance (`crates/graph/src/index/catalog_context.rs:347`,
  `crates/graph/src/property/inline_dispatch.rs`). The old-key/new-key transition contract for
  these postings is single-sourced in `index_ops_for_value_change`
  (`crates/graph/src/property/change.rs`) and pinned by owning tests (GAP-2026-07-29-004).
- **Vertex memberships are flat-only today.** `IndexedVertexMembership` carries no field path
  (`crates/graph-kernel/src/index.rs:1072`), and vertex dispatch resolves memberships by
  `(labels, property_id)` alone
  (`crates/graph/src/index/catalog_context.rs::vertex_index_memberships_for_labels`). A `SET
  v.stats = {...}` therefore maintains no posting for any leaf of `stats`.
- **Vertex backfill is flat-only** (`crates/graph/src/index/vertex_property_backfill.rs`): it
  scans stored scalar values and has no record-walking path.
- **Posting bytes have one encoder.** Every domain funnels through
  `crate::property::sortable_index_key`; there is no second key encoding to reconcile.
- **The planner already models nested cost paths syntactically** (`COST BY v.stats.field`
  parses), but cannot select an index anchor because no such index domain exists.

## Decision

Vertex nested-record indexes use **the same canonical dotted-path leaf domain as edge INLINE
struct leaves**. One domain, one encoding, one transition contract:

1. **Identity.** The Router interns each declared vertex nested leaf path (for example
   `stats.score`) as its own `PropertyId`, exactly as it does for edge INLINE leaves.
   `IndexedVertexMembership` gains required `field_path` and `ancestor_property_id` wire
   fields with the same meaning as the edge membership field (flat memberships carry `""`
   and `0` explicitly), keeping one catalog shape for both domains.
2. **Leaf resolution.** Graph resolves a membership's leaf value by walking the stored sidecar
   `Value::Record` along the canonical dotted path at mutation, backfill, and scan time through
   one shared resolver. A missing intermediate node or a non-record node on the path yields "no
   value" — the same semantics as an absent inline struct field — never an error.
3. **Postings.** Leaf postings use the existing `sortable_index_key` encoding and the existing
   `index_ops_for_value_change` old-key/new-key transitions. No new posting semantics are
   introduced; the GAP-2026-07-29-004 owning tests remain the contract for both domains.
4. **Validation.** Declaring a nested vertex index validates at DDL time that the path depth is
   within a bounded maximum and that the leaf kind is scalar-indexable. Record shape drift at
   mutation time is handled by the absence rule above, not by schema enforcement in Graph; the
   Router remains the schema SSOT.
5. **Scope guard.** List/array leaves and non-record container nodes remain unindexable. A
   declared path whose leaf resolves to a list posts nothing, matching the fail-closed absence
   rule rather than introducing a second key domain silently.

### Implementation slices

1. Kernel + DDL: add required `field_path` / `ancestor_property_id` to
   `IndexedVertexMembership` and the registration args; Router DDL interns leaf identities
   and emits memberships. — **Implemented and validated (2026-08-23).**
2. Graph mutation dispatch: leaf-aware resolution in the vertex change path, sharing the edge
   resolver; rejection-free absence semantics covered by tests. — **Implemented and
   validated (2026-08-23).**
3. Backfill: extend the vertex export to walk records along declared paths under the same
   cursor protocol as the edge export's domains (ADR 0059 one-opaque-export rule), with
   exact nested reopen proofs on Graph MemoryId 51 and graph-index
   MemoryId 7. — **Implemented and validated (2026-08-23).**
4. Planner: seed anchors and range/equality candidates against interned leaf identities;
   statistics reuse the existing per-property estimates unchanged. — **Landed on main
   (`39746f7b3`); acceptance pending Plan 0285 validation.**

## Alternatives considered

- **Separate record-index key domain.** A dedicated nested-key namespace would duplicate the
  sortable encoding and the transition semantics, split planner/statistics knowledge across two
  key spaces, and break the symmetry that lets `COST BY v.stats.field` reuse the edge leaf
  machinery. Rejected as a second source of truth for the same concept.
- **Flatten records into synthetic top-level properties at write time.** This duplicates
  canonical data (the flattened copy is derived state persisted beside its source) and forces a
  rewrite of every synthetic property whenever an ancestor node changes shape. Violates the
  store-only-canonical-facts rule. Rejected.

## Consequences

- The kernel wire types gain REQUIRED fields (`field_path`, `ancestor_property_id`, and the
  build/export `record_source` projection) with explicit canonical flat values; pre-production
  requires fresh state when the catalog layout rolls forward, per the repository's
  pre-production simplicity rule. The export cursor has exactly one current encoding: its
  leading version byte rejects any other value as `CursorMalformed` as a corruption and
  mismatch guard, with no legacy decoder.
- Read-only catalog projections fail closed by omission: an inconsistent nested membership row
  (missing reverse name, malformed path, or missing/zero ancestor) drops out of stats and the
  maintenance catalog instead of flattening to a flat identity, while the DDL/write boundary
  rejects such rows before publication or registration.
- Equality and range planning over nested vertex fields becomes expressible without new planner
  anchor kinds: the interned leaf identity behaves like any other indexed vertex property.
- Absence semantics make partial records safe by construction: removing a leaf removes exactly
  its posting; a shape mismatch can never leave a stale candidate because transitions flow
  through the same Remove/Insert pair as scalar replacement.
- Unbounded-depth or container-leaf indexes stay rejected at DDL, so enumeration cost stays
  bounded and no silent non-indexed domain appears.

## Related

- [Property index design](../index/property-index.md)
- [Labeled edge inline properties](../storage/labeled-edge-inline-properties.md)
- [ADR 0008 — Edge inline property profile Router SSOT](0008-edge-inline-property-profile-router-ssot.md)
- [ADR 0059 — Create-index migration backfill](0059-create-index-migration-backfill.md)
