# Sharding and standalone mode

## Purpose

Entry point for **how Gleaph runs today (standalone)** versus **target multi-shard federation**. Use this before changing router dispatch, graph-index APIs, or federation code.

## Status

| Document | Status |
|----------|--------|
| [standalone-mode.md](standalone-mode.md) | **Planned** — current engineering direction |
| [federation-target.md](federation-target.md) | **Planned** — target architecture (not fully implemented) |

## Audience

| Reader | Start here |
|--------|------------|
| Implementing query/index work now | [standalone-mode.md](standalone-mode.md) |
| Designing multi-shard dispatch | [federation-target.md](federation-target.md) → [index/lookup-intersection.md](../index/lookup-intersection.md) |
| Federation identifiers | [federation/model.md](../federation/model.md) |

## Related documents

- [index/property-index.md](../index/property-index.md)
- [index/lookup-intersection.md](../index/lookup-intersection.md)
- [federation/query-semantics.md](../federation/query-semantics.md)
- [architecture/overview.md](../architecture/overview.md)
