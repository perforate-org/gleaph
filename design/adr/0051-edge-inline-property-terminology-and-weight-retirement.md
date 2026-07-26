# 0051. Edge inline property terminology unification and legacy weight compatibility retirement

Date: 2026-07-26
Status: accepted
Last revised: 2026-07-27
Anchor timestamp: 2026-07-26 20:03:57 UTC +0000

## Context

Gleaph's GQL dialect exposes fast, fixed-width edge-local data through the `INLINE` schema
modifier. The public syntax is ordinary property access:

```gql
CREATE EDGE LABEL ROAD {
  distance FLOAT32 INLINE
}

MATCH (a)-[e:ROAD]->(b)
RETURN b, e.distance
ORDER BY e.distance ASC
```

This concept has accumulated three competing names across the codebase and design documents:

- **payload** — from early LARA physical storage (`payload_byte_width`, `EdgeInlineValueStore`, `ROUTER_EDGE_PAYLOAD_PROFILES`, "edge payload").
- **inline value** — from the first generalization of the old 2-byte weight concept (`EdgeInlineValueProfile`, `EdgeInlineValuePredicate`, `edge_inline_value.rs`).
- **inline property** — from the GQL syntax design in ADR 0034 (`INLINE` modifier, `ResolvedEdgeLabel::inline_schema`, `InlinePropertyBytes`).

ADR 0034 settled on `INLINE` as the public surface and on ordinary property access as the daily
query shape. The internal code and documents have not caught up: types, fields, stable-region
names, and explanatory prose still mix all three terms.

Separately, `GLEAPH.WEIGHT(e)` and the dedicated `EdgeWeightProfile` / `WeightEncoding` codecs exist
only as a compatibility surface for the pre-`INLINE` 2-byte weight encoding. Once users can declare
any scalar `INLINE` property and use `COST BY e.property`, the legacy weight path becomes
redundant.

## Problem

| Issue | Effect |
|-------|--------|
| Three names for one concept | Reviewers and new contributors cannot tell whether a field or document refers to the GQL syntax, the schema profile, or the physical bytes. |
| `payload` is overloaded | "payload" also means bundle payloads, procedure arguments, IC message payloads, and generic value-extension payloads. In the edge context it is indistinguishable from those. |
| `inline value` is ambiguous | Does it mean a GQL value, a prepared decoder value, or raw stored bytes? |
| Legacy weight surface lingers | `GLEAPH.WEIGHT(e)` keeps the old weight encoder alive and signals that edge-local fast data is still a special "weight" concept rather than an ordinary `INLINE` property. |
| Documents disagree | ADR 0008 is titled "Edge payload profile schema", `gql_dialect.rs` classifies `GLEAPH.WEIGHT` as an `EdgeInlineValueFunction`, and `extension-syntax.md` calls the feature "Edge inline properties". |

This is not a functional bug. It is an architectural vocabulary drift that makes the boundary
between public GQL syntax, logical schema, and physical storage harder to reason about.

## Existing architecture assessment

The right boundaries already exist. The problem is only the names:

- **Router** owns logical edge-inline-property schema (`ROUTER_EDGE_PAYLOAD_PROFILES`).
- **graph-kernel** carries the physical profile on the wire (`ResolvedEdgeLabel::payload_profile`).
- **Graph** executes reads/mutations using the wire-derived schema and never writes the inline
  property id to sidecar state.
- **ic-stable-lara** stores fixed-width bytes independently from edge rows
  (`EdgeInlineValueStore`, `payload_byte_width`).
- **gql-planner** has a predicate type for fixed-width edge-inline comparisons
  (`EdgeInlineValuePredicate`).

No new module or boundary is needed. The change is a large-scale rename plus a later deletion of
the legacy weight compatibility layer.

Because the rename touches candid-serialized types (`ResolvedEdgeLabel`, `EdgeInlineValueSchemaRecord`)
and stable-region names, it is a breaking change regardless of whether functionality changes.

## Alternatives

### A. Do nothing
Keep the three terms. Treat `payload` as context-dependent, `inline value` as a physical concept,
and `inline property` as the GQL syntax.

- **Benefit:** Zero churn.
- **Drawback:** The vocabulary drift continues. Every future feature that touches edge-local fast
  data will re-encounter the same naming confusion.

### B. Rename only in new code
Introduce `inline property` names for new features but leave existing `payload` / `inline value`
names in place.

- **Benefit:** Smaller diff.
- **Drawback:** Creates two parallel vocabularies inside one codebase. The old names stay in the
  hottest paths (storage, planner, wire), so the confusion is not resolved.

### C. Unify all names to `inline property` / `inline property bytes` now, and retire weight later
Rename types, fields, stable-region names, and documents to a single vocabulary. Schedule the
removal of `GLEAPH.WEIGHT(e)` and `EdgeWeightProfile` for a later phase after ordinary `INLINE`
properties fully cover the same use cases.

- **Benefit:** One concept, one name. Physical bytes are clearly separated from logical property
  access. The weight removal becomes a natural follow-up rather than an unrelated cleanup.
- **Drawback:** Large breaking diff; candid/stable fields change; design documents need broad
  updates.

### D. Unify names and remove weight immediately
Do the rename and delete `GLEAPH.WEIGHT` / `EdgeWeightProfile` in the same patch.

- **Benefit:** One disruptive window instead of two.
- **Drawback:** Users lose a compatibility surface before the replacement (`COST BY e.property`) has
  been exercised in production. Risk of regressions in benchmarks and demos that still rely on
  `GLEAPH.WEIGHT`.

## Decision

Adopt **Alternative C**: unify terminology in one ADR and a first implementation phase, then retire
the legacy weight surface in a gated second phase.

### Phase A: terminology unification (immediate)

Use exactly these names:

| Concept | Canonical term | Notes |
|--------|----------------|-------|
| GQL syntax / user-facing idea | **inline property** | `CREATE EDGE LABEL ... { <prop> <type> INLINE }`, `e.prop`. |
| Logical schema / catalog record | **inline property schema** | Router SSOT: `(GraphId, EdgeLabelId) → schema record`. |
| Physical byte width + encoding | **inline property profile** | Wire-carried profile consumed by Graph. |
| Stored fixed-width bytes | **inline property bytes** | LARA slab/log/blob bytes. Avoid "payload". |
| Per-edge byte width | `inline_property_byte_width` | Replaces `payload_byte_width`. |
| Storage store | `EdgeInlinePropertyBytesStore` | Replaces `EdgeInlineValueStore`. |

Eliminate:

- `inline value` as a standalone term.
- bare `payload` in the edge-inline context.
- `inline payload` as an intermediate compromise.

Key renames:

| Old | New |
|-----|-----|
| `EdgeInlineValueProfile` | `EdgeInlinePropertyProfile` |
| `EdgeInlineValueEncoding` | `EdgeInlinePropertyEncoding` |
| `EdgeInlineValue` | `EdgeInlinePropertyBytes` |
| `MAX_EDGE_INLINE_VALUE_BYTES` | `MAX_EDGE_INLINE_PROPERTY_BYTES` |
| `PreparedEdgeInlineValueDecoder` | `PreparedEdgeInlinePropertyDecoder` |
| `DecodedEdgeInlineValue` | `DecodedEdgeInlinePropertyValue` |
| `EdgeInlineValuePredicate` (gql-planner) | `EdgeInlinePropertyPredicate` |
| `EdgeInlineValueSchemaRecord` (router) | `EdgeInlinePropertySchemaRecord` |
| `EdgeInlineValueProfileStore` (router) | `EdgeInlinePropertyProfileStore` |
| `ROUTER_EDGE_PAYLOAD_PROFILES` | `ROUTER_EDGE_INLINE_PROPERTY_PROFILES` |
| `EdgeInlineValueStore` (ic-stable-lara) | `EdgeInlinePropertyBytesStore` |
| `payload_byte_width` (LabelBucket) | `inline_property_byte_width` |
| `payload_slab` / `payload_log` / `payload_blobs` | `inline_property_bytes_slab` / `..._log` / `..._blobs` |
| `ResolvedEdgeLabel::inline_value_profile` | `inline_property_profile` |

`EdgeInlineValueProfileStore` in graph-kernel is similarly renamed to
`EdgeInlinePropertyProfileStore`. Graph-local `EDGE_PAYLOAD_PROFILES` is already retired; no
stable region remains to rename on the Graph canister.

`AnchorSource::InlinePropertyEquality` in `gql-planner` is **not** part of this rename. It refers
to a vertex-pattern literal (`(n:Label {prop: value})`), which is unrelated to the edge `INLINE`
storage modifier. It should be renamed separately to avoid conflating the two concepts, for
example to `PatternPropertyLiteralEquality`.

### Phase B: legacy weight compatibility retirement (gated)

After Phase A is complete and ordinary `INLINE` scalar properties have been used in production for
shortest-path `COST BY` and vector predicates, remove:

- `GLEAPH.WEIGHT(e)` runtime function.
- `GLEAPH.COST BY GLEAPH.WEIGHT(e)` planner/executor compatibility path.
- `EdgeWeightProfile`, `WeightEncoding`, `PreparedWeightDecoder`, `decode_edge_weight`.
- `EdgeInlinePropertyEncoding::WeightRawU16`, `WeightLinearU16`, `WeightLogU16`, `WeightBinary16`.

The gate is **not** a calendar date. The gate is:

1. `COST BY e.property` works for all scalar `INLINE` types used in existing benchmarks and demos.
2. `GLEAPH.VECTOR.*` predicates have been migrated to read from the Router-resolved inline property
   profile without the weight fast path.
3. No production demo or canbench target depends on `GLEAPH.WEIGHT(e)`.

Until the gate is met, `GLEAPH.WEIGHT` remains classified as `Compatibility` in the Rust extension
manifest and its implementation remains in place.

## Consequences

### Positive

- One vocabulary across crates, types, fields, stable regions, and documents.
- `inline property bytes` is unambiguous: it is always the physical byte representation of an edge
  inline property.
- `payload` can return to its original meaning: bundle/procedure/IC message payloads.
- Phase B has a clear, testable gate instead of a vague future cleanup.
- ADR 0008 can be revised to use the same vocabulary, strengthening the router-SSOT contract.

### Negative / costs

- Large, mechanical, breaking rename across at least five crates.
- Candid field renames require development stable data wipe (acceptable per roadmap).
- Every design document that mentions edge-local fast data must be updated.
- Phase B removes a query-time compatibility surface; downstream users must migrate to `e.property`.

## Migration

### Phase A migration

1. Add this ADR and update `design/adr/README.md`.
2. Rename types and fields in graph-kernel, router, graph, gql-planner, and ic-stable-lara.
3. Bump stable record versions where candid-serialized names change:
   - Router `EdgeInlinePropertySchemaRecord`.
   - `ResolvedEdgeLabel` wire type.
4. Update reopen tests and `bench_layout_graph_stable_reopen_touch` expectations.
5. Update `design/adr/0008-edge-inline-value-profile-router-ssot.md`:
   - title to "Edge inline property schema: router SSOT and graph stable retirement"
   - `payload_profile` → `inline_property_profile`
   - `ROUTER_EDGE_PAYLOAD_PROFILES` → `ROUTER_EDGE_INLINE_PROPERTY_PROFILES`
6. Update `design/adr/0034-gleaph-gql-extension-syntax.md`, `design/gql/extension-syntax.md`,
   `design/storage/labeled-edge-inline-values.md`, and `design/gql/plan-format.md`.
7. Update `gleaph-graph-kernel::gql_dialect` manifest:
   - `GqlDialectExtensionKind::EdgeInlineValueFunction` → `EdgeInlinePropertyFunction`
   - doc anchors to `#edge-inline-properties`
8. Wipe development stable data and run full Router/Graph reopen + PocketIC E2E suites.

### Phase B migration

1. Mark `GLEAPH.WEIGHT` and `GLEAPH.COST` weight paths `Deprecated` in the Rust manifest.
2. Add targeted tests that prove `COST BY e.property` replaces every existing `GLEAPH.WEIGHT`
   canbench scenario.
3. Remove `EdgeWeightProfile` and weight encodings from `EdgeInlinePropertyEncoding`.
4. Remove `GLEAPH.WEIGHT` runtime function and planner fusion.
5. Update ADR 0034 and `extension-syntax.md` to state that the legacy weight surface is removed.
6. Wipe development stable data if any weight-encoded stable bytes remain in test fixtures.

## Design documentation impact

| Document | Update |
|----------|--------|
| `design/adr/README.md` | Add ADR 0051 entry. |
| `design/adr/0008-edge-inline-value-profile-router-ssot.md` | Retitle; replace `payload_profile`, `EdgeInlineValueProfile`, `ROUTER_EDGE_PAYLOAD_PROFILES`; refresh stable-memory layout table. |
| `design/adr/0034-gleaph-gql-extension-syntax.md` | Replace "inline value" and "payload" with "inline property" / "inline property bytes"; describe `GLEAPH.WEIGHT` as a Phase-B-removed legacy surface. |
| `design/gql/extension-syntax.md` | Rename syntax class table from "Edge inline value" to "Edge inline property"; replace all bare "payload" usage in edge context. |
| `design/storage/labeled-edge-inline-values.md` | Rename `EdgeInlineValueStore` → `EdgeInlinePropertyBytesStore`, `payload_byte_width` → `inline_property_byte_width`, `payload_slab/log/blobs` → `inline_property_bytes_*`. |
| `design/gql/plan-format.md` | `payload_profile` → `inline_property_profile`. |
| `design/adr/0016-overflow-log-tombstones-and-src-fields.md` | Rename LARA-internal `payload_log` / `payload_blobs` / `payload_cell` only where they refer to the inline property bytes sequence. Preserve unrelated edge-row "payload" discussion if it refers to the 4-byte edge body. |
| `crates/gql-planner/CLAUDE.md` | Keep "inline-property-equality" terminology but add a note that it refers to vertex pattern literals, not edge `INLINE` storage. |

## Related ADRs

- [0051 — this ADR](0051-edge-inline-property-terminology-and-weight-retirement.md): accepted; Phase A terminology unification is the source of truth for all names in this document set.
- [0006 — Pre-federation foundation](0006-pre-federation-foundation.md): router owns label/property id catalogs.
- [0007 — Stable-memory layout](0007-stable-memory-layout.md): stable region naming and MemoryId repack policy.
- [0008 — Edge inline value profile: router SSOT](0008-edge-inline-value-profile-router-ssot.md): will be retitled and updated by Phase A.
- [0034 — Gleaph GQL extension syntax surface](0034-gleaph-gql-extension-syntax.md): defines the `INLINE` public syntax that justifies the vocabulary.
- [0050 — LARA traverse read API](0050-lara-traverse-read-api.md): may overlap with ic-stable-lara naming.
