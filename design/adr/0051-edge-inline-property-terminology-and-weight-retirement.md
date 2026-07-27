# 0051. Edge inline property terminology unification and legacy weight compatibility retirement

Date: 2026-07-26
Status: implemented
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

| Issue                         | Effect                                                                                                                                                                                               |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Three names for one concept   | Reviewers and new contributors cannot tell whether a field or document refers to the GQL syntax, the schema profile, or the physical bytes.                                                          |
| `payload` is overloaded       | "payload" also means bundle payloads, procedure arguments, IC message payloads, and generic value-extension payloads. In the edge context it is indistinguishable from those.                        |
| `inline value` is ambiguous   | Does it mean a GQL value, a prepared decoder value, or raw stored bytes?                                                                                                                             |
| Legacy weight surface lingers | `GLEAPH.WEIGHT(e)` keeps the old weight encoder alive and signals that edge-local fast data is still a special "weight" concept rather than an ordinary `INLINE` property.                           |
| Documents disagree            | ADR 0008 is titled "Edge payload profile schema", `gql_dialect.rs` classifies `GLEAPH.WEIGHT` as an `EdgeInlineValueFunction`, and `extension-syntax.md` calls the feature "Edge inline properties". |

This is not a functional bug. It is an architectural vocabulary drift that makes the boundary
between public GQL syntax, logical schema, and physical storage harder to reason about.

## Existing architecture assessment

The right boundaries already exist. The problem is only the names:

- **Router** owns logical edge-inline-property schema (`ROUTER_EDGE_PAYLOAD_PROFILES`).
- **graph-kernel** carries the physical profile on the wire (`ResolvedEdgeLabel::inline_property_profile`).
- **Graph** executes reads/mutations using the wire-derived schema and never writes the inline
  property id to sidecar state.
- **ic-stable-lara** stores fixed-width bytes independently from edge rows
  (`EdgeInlinePropertyBytesStore`, `inline_property_byte_width`).
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

Adopt **Alternative C**, executed as two consecutive implementation phases documented in this ADR:
Phase A unifies the terminology across code and documents, and Phase B retires the legacy weight
compatibility surface. Phase A was completed in preceding commits; Phase B is completed in this
commit.

### Phase A: terminology unification (completed)

Use exactly these names:

| Concept                         | Canonical term                 | Notes                                                       |
| ------------------------------- | ------------------------------ | ----------------------------------------------------------- |
| GQL syntax / user-facing idea   | **inline property**            | `CREATE EDGE LABEL ... { <prop> <type> INLINE }`, `e.prop`. |
| Logical schema / catalog record | **inline property schema**     | Router SSOT: `(GraphId, EdgeLabelId) → schema record`.      |
| Physical byte width + encoding  | **inline property profile**    | Wire-carried profile consumed by Graph.                     |
| Stored fixed-width bytes        | **inline property bytes**      | LARA slab/log/blob bytes. Avoid "payload".                  |
| Per-edge byte width             | `inline_property_byte_width`   | Replaces `payload_byte_width`.                              |
| Storage store                   | `EdgeInlinePropertyBytesStore` | Replaces `EdgeInlineValueStore`.                            |

Eliminate:

- `inline value` as a standalone term.
- bare `payload` in the edge-inline context.
- `inline payload` as an intermediate compromise.

Key renames:

| Old                                              | New                                                    |
| ------------------------------------------------ | ------------------------------------------------------ |
| `EdgeInlineValueProfile`                         | `EdgeInlinePropertyProfile`                            |
| `EdgeInlineValueEncoding`                        | `EdgeInlinePropertyEncoding`                           |
| `EdgeInlineValue`                                | `EdgeInlinePropertyBytes`                              |
| `MAX_EDGE_INLINE_VALUE_BYTES`                    | `MAX_EDGE_INLINE_PROPERTY_BYTES`                       |
| `PreparedEdgeInlineValueDecoder`                 | `PreparedEdgeInlinePropertyBytesDecoder`               |
| `DecodedEdgeInlineValue`                         | `DecodedEdgeInlinePropertyBytes`                       |
| `EdgeInlineValuePredicate` (gql-planner)         | `EdgeInlinePropertyPredicate`                          |
| `EdgeInlineValueSchemaRecord` (router)           | `EdgeInlinePropertySchemaRecord`                       |
| `EdgeInlineValueProfileStore` (router)           | `EdgeInlinePropertyProfileStore`                       |
| `ROUTER_EDGE_PAYLOAD_PROFILES`                   | `ROUTER_EDGE_INLINE_PROPERTY_PROFILES`                 |
| `EdgeInlineValueStore` (ic-stable-lara)          | `EdgeInlinePropertyBytesStore`                         |
| `payload_byte_width` (LabelBucket)               | `inline_property_byte_width`                           |
| `payload_slab` / `payload_log` / `payload_blobs` | `inline_property_bytes_slab` / `..._log` / `..._blobs` |
| `ResolvedEdgeLabel::inline_value_profile`        | `inline_property_profile`                              |

`EdgeInlineValueProfileStore` in graph-kernel is similarly renamed to
`EdgeInlinePropertyProfileStore`. The test-only graph-local `EDGE_PAYLOAD_PROFILES` is renamed
to `EDGE_INLINE_PROPERTY_PROFILES`; no production stable region remains to rename on the Graph
canister.

`AnchorSource::InlinePropertyEquality` in `gql-planner` is **not** part of this rename. It refers
to a vertex-pattern literal (`(n:Label {prop: value})`), which is unrelated to the edge `INLINE`
storage modifier. It should be renamed separately to avoid conflating the two concepts, for
example to `PatternPropertyLiteralEquality`.

### Phase B: legacy weight compatibility retirement (completed in this patch)

Phase A terminology renames landed in preceding commits. Once `COST BY e.property` was exercised
by graph path/expand tests and the vector predicates were already reading the Router-resolved inline
property profile, the remaining gates were met. This patch therefore removes the legacy weight
surface:

- `GLEAPH.WEIGHT(e)` runtime function.
- `GLEAPH.COST BY GLEAPH.WEIGHT(e)` planner/executor compatibility path.
- `EdgeWeightProfile`, `WeightEncoding`, `PreparedWeightDecoder`, `decode_edge_weight`.
- `EdgeInlinePropertyEncoding::WeightRawU16`, `WeightLinearU16`, `WeightLogU16`, `WeightBinary16`.

The implementation is no longer present; no further gate or deprecation period applies.

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

### Phase A migration (completed)

All items were completed in the commits that landed ADR 0051:

1. ✅ Added this ADR and updated `design/adr/README.md`.
2. ✅ Renamed types and fields in graph-kernel, router, graph, gql-planner, and ic-stable-lara.
3. ✅ Bumped stable record versions where candid-serialized names changed.
4. ✅ Updated reopen tests and `bench_layout_graph_stable_reopen_touch` expectations.
5. ✅ Updated `design/adr/0008-edge-inline-property-profile-router-ssot.md`.
6. ✅ Updated `design/adr/0034-gleaph-gql-extension-syntax.md`, `design/gql/extension-syntax.md`,
   `design/storage/labeled-edge-inline-properties.md`, and `design/gql/plan-format.md`.
7. ✅ Updated `gleaph-graph-kernel::gql_dialect` manifest.
8. ✅ Wiped development stable data and ran Router/Graph reopen + PocketIC E2E suites.

### Phase B migration (completed in this patch)

The legacy weight surface was removed without a separate deprecation period because the
replacement (`COST BY e.property` and ordinary `e.property` access) was already covered by tests:

1. ✅ Removed `GLEAPH.WEIGHT(e)` runtime function.
2. ✅ Removed `GLEAPH.COST BY GLEAPH.WEIGHT(e)` planner/executor compatibility path.
3. ✅ Removed `EdgeWeightProfile`, `WeightEncoding`, `PreparedWeightDecoder`, and `decode_edge_weight`.
4. ✅ Removed weight encodings from `EdgeInlinePropertyEncoding`.
5. ✅ Updated ADR 0034 and `extension-syntax.md` to state that the legacy weight surface is removed.
6. ✅ Updated remaining tests and benchmarks that referenced `GLEAPH.WEIGHT(e)`.

## Design documentation impact

| Document                                                      | Update                                                                                                                                                                                                                    | Status          |
| ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| `design/adr/README.md`                                        | Add ADR 0051 entry.                                                                                                                                                                                                       | ✅              |
| `design/adr/0008-edge-inline-property-profile-router-ssot.md` | Retitle; replace `payload_profile`, `EdgeInlineValueProfile`, `ROUTER_EDGE_PAYLOAD_PROFILES`; refresh stable-memory layout table.                                                                                         | ✅              |
| `design/adr/0034-gleaph-gql-extension-syntax.md`              | Replace "inline value" and "payload" with "inline property" / "inline property bytes"; state that `GLEAPH.WEIGHT` is removed.                                                                                             | ✅ (this patch) |
| `design/gql/extension-syntax.md`                              | Rename syntax class table from "Edge inline value" to "Edge inline property"; replace all bare "payload" usage in edge context; state `GLEAPH.WEIGHT` is removed.                                                         | ✅ (this patch) |
| `design/gql/layers.md`                                        | Remove the `weight` module from `gleaph-gql-integration` description.                                                                                                                                                     | ✅ (this patch) |
| `design/execution/operators.md`                               | Remove `SUM(GLEAPH.WEIGHT(e))` horizontal aggregate example; use ordinary inline property access.                                                                                                                         | ✅ (this patch) |
| `design/execution/group-variables.md`                         | Replace `GLEAPH.WEIGHT(e)` examples with `e.distance`; note group edge property semantics.                                                                                                                                | ✅ (this patch) |
| `design/storage/labeled-edge-inline-properties.md`            | Rename `EdgeInlineValueStore` → `EdgeInlinePropertyBytesStore`, `payload_byte_width` → `inline_property_byte_width`, `payload_slab/log/blobs` → `inline_property_bytes_*`.                                                | ✅              |
| `design/gql/plan-format.md`                                   | `payload_profile` → `inline_property_profile`.                                                                                                                                                                            | ✅              |
| `design/adr/0016-overflow-log-tombstones-and-src-fields.md`   | Rename LARA-internal `payload_log` / `payload_blobs` / `payload_cell` only where they refer to the inline property bytes sequence. Preserve unrelated edge-row "payload" discussion if it refers to the 4-byte edge body. | ✅              |
| `crates/gql-planner/CLAUDE.md`                                | Keep "inline-property-equality" terminology but add a note that it refers to vertex pattern literals, not edge `INLINE` storage.                                                                                          | ✅              |

## Related ADRs

- [0051 — this ADR](0051-edge-inline-property-terminology-and-weight-retirement.md): accepted; both Phase A terminology unification and Phase B weight retirement are complete.
- [0006 — Pre-federation foundation](0006-pre-federation-foundation.md): router owns label/property id catalogs.
- [0007 — Stable-memory layout](0007-stable-memory-layout.md): stable region naming and MemoryId repack policy.
- [0008 — Edge inline property profile: router SSOT](0008-edge-inline-property-profile-router-ssot.md): retitled and updated by Phase A.
- [0034 — Gleaph GQL extension syntax surface](0034-gleaph-gql-extension-syntax.md): defines the `INLINE` public syntax; legacy weight surface is now removed.
- [0050 — LARA traverse read API](0050-lara-traverse-read-api.md): may overlap with ic-stable-lara naming.
