# Incremental vertex migration

## Purpose

Document instruction-bounded vertex migration between graph shards: physical states, journal, chunked copy, and cutover.

## Physical states (per shard)

| State | Visibility | Writes |
|-------|------------|--------|
| `Active` | Yes | User |
| `SourceMigrating(epoch)` | Yes (authoritative) | User + journal |
| `TargetStaging(epoch)` | No | Maintenance only |
| `ForwardingStub { logical, cached, epoch }` | Resolves via router | No direct user writes |

Router `VertexPlacement::Migrating` remains the global authority until cutover sets `Active` on the destination.

## APIs (graph shard)

| Method | Shard | Role |
|--------|-------|------|
| `migration_start` | Source | Router begin, `SourceMigrating`, enqueue item |
| `migration_staging_begin` | Destination | Create staging row, `TargetStaging`, enqueue item |
| `migration_apply_chunk` | Destination | Apply copied `X.o` / `X.i` chunk |
| `migration_maintenance_tick` | Either | LARA maintenance + one migration step |
| `migration_cutover` | Both (idempotent) | Router finish, `Active` / `ForwardingStub`, cleanup |
| `migration_status` | Either | Ops snapshot |

Bulk `migration_export` / `migration_import` / `migration_tombstone` are removed.

## Recovery

| Router | Source local | Dest local | Action |
|--------|--------------|------------|--------|
| `Migrating` | `SourceMigrating` | `TargetStaging` | Resume queue item |
| `Migrating` | `SourceMigrating` | missing | Recreate staging if safe |
| `Active(dest)` | not stub | — | Retry source `ForwardingStub` |
| `Active(dest)` | stub | stale queue | Drop stale queue/journal |
| epoch mismatch | — | — | No-op stale work |

## Related

- [model.md](model.md)
- [operations.md](operations.md)
