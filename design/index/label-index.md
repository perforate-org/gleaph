# Label index

## Status

**Planned** — see [ADR 0004](../adr/0004-label-index.md). Not implemented.

## Purpose

Global **vertex label membership** postings on graph-index, parallel to the [property index](property-index.md).
Enables router seed routing for `PlanOp::NodeScan { label: Some(_) }` and extends the federated
aggregate fast path ([ADR 0003](../adr/0003-federated-aggregate-merge.md)) to
`MATCH (n:Label) … GROUP BY` on indexed properties.

## Non-goals

- Edge label postings (separate from vertex labels).
- Range or sort on labels.
- Replacing label telemetry (`LabelUsageDelta`) — telemetry stays aggregate metadata.

## Posting model

```text
LabelPostingKey { vertex_label_id, shard_id, vertex_id }
```

Sorted: `label_id → shard_id → vertex_id`. Multi-label vertices have one posting per label.

## Read APIs (planned)

| API | Role |
|-----|------|
| `lookup_label(label_id)` | All `PostingHit` for one vertex label |

## Write path (planned)

Graph shards enqueue label posting changes on vertex insert, label set/add/remove, and vertex
delete; flush to graph-index with the same compensate-and-retry semantics as property postings
(`graph/src/index/pending.rs`).

## Router (planned)

- **Seeds:** `lookup_label` → slice by `shard_id` → `seed_bindings_blob` (same as property seeds).
- **Aggregate fast path:** `lookup_label` → `vertex_filter_packed` on
  `count_postings_by_value` for `MATCH (n:L) GROUP BY n.prop, COUNT(*)`.

## Related documents

- [ADR 0004](../adr/0004-label-index.md)
- [property-index.md](property-index.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
