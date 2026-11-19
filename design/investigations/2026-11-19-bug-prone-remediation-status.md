# Bug-prone audit remediation status (2026-11-19)

Status: dated record. This does not change a contract; it re-checks the findings in
[`2026-11-14-bug-prone-pattern-audit.md`](2026-11-14-bug-prone-pattern-audit.md) against the
current tree. The 2026-11-14 line numbers are stale, and its Disposition section predates several
fixes, so a finding being absent from that section does not mean it is still open.

Base: `afb93371` (`main`). Method: code inspection of the named owner plus the compile-time/source
shape that the finding depends on. No behavior change in this record.

## Confirmed closed

| Finding | Evidence |
| --- | --- |
| P1-01〜P1-03, P1-05〜P1-07, P1-09 | 2026-11-14 Disposition records the fix and regression tests. |
| P1-04 | `DeploymentGrantScope` split (`OwnDeployment` / `Relay`) is implemented; residual trust is recorded below. |
| G-13 | `LogicalResourceTag` and `IndexPendingFloorOwner` use one `#[repr(u8)]` table for encode/decode. |
| P3-05 | `GlobalEdgeId` decode rejects a nonzero high half instead of truncating the label. |
| P3-07 | `IndexIntersectionResult::vertices()` is gone; the vertex scan matches `Vertices` and rejects `Edges`. |
| P3-09 | `streaming_op_preserves_row_cardinality` is the single predicate; `KeepBindings` is included. |
| P3-10 | `collect_property_reading_vars_in_op` descends through `plan_subplans`; `property_access_root` accepts the chain's `Variable` root. |
| X-01〜X-03 | `MAX_PREPARED_BATCH`, `MAX_CANDIDATE_KEYS`, and `MAX_CHAIN_HOPS` each have one owner. |
| X-04 | Bootstrap/grant wire types are owned by `gleaph-provision-api`; Provision, ingress-client, and CLI re-export them. |
| X-05 | `ExecutionProfile` replaces the four bools. |
| X-06 | `validate_batch_endpoint` enumerates both variants; no `_ => Ok(())`. |
| X-07 | `IndexPostingMutation` uses `PropertyId`, `IndexVertexId`, `IndexEdgeLabelId`, and `IndexVertexLabelId`. |
| D-02 | `SESSION SET` schema/time-zone/parameter forms return `InvalidArgument`; no silent `Ok(current)`. |
| D-03 | `general_routing` actually reads `node_labels` for endpoint filtering. |
| D-05 | `ResolvedGraphContext.graph_id` is private; the only bypass is the named `for_bound_graph` seam. |
| D-06 | Receipt projection enumerates the five receipt-less payload variants. |
| D-07 | Grant application traps on a failed write, so the IC message rolls back the whole block. |
| D-08 | The `force: bool` query/update bypass is gone. |
| D-09 | `PackedPostingKey` owns `(shard_id, vertex_id) ↔ u64`; no `as u32` truncation at the call sites. |
| D-10 | `PreparedQueryName` validates and owns the prepared-query name. |
| D-11 | `ParsedIndexDdl` + `try_execute_index_ddl` own parse/authorize/resolve/dispatch; the four DDL copies are gone. |
| E-03 | `IndexPostingPurgeStepResult` stores `next`, `examined`, `removed`; `done()` is derived. |
| E-04 | `LaneCursor<K>` + `LaneCursorKey` own the recovery lane cursor and EOF state. |
| E-05 | `TopologyEpoch` / `LifecycleCatalogEpoch` / `PreparedCatalogEpoch` replace the three raw `u64` fields. |
| E-07 | Label stats update uses `checked_add` / `checked_sub` and panics on inconsistency; no `saturating_sub` to zero. |
| E-09 | `shards()` returns `Option<&[RouterMutationShardV1]>`; a non-Scalar payload is not flattened to empty. |
| E-10 | `PendingEffectDisposition::{Active, Quarantined { next_retry_ns }}` replaced the history flag. |
| E-11 | `value_type_is_container` enumerates scalars and rejects unknown types. |
| F-07 | `reset_upload_for_retry` clears a failed upload and its chunks; Failed is recoverable. |
| F-11 | `DictState` enum replaces the persistent `u8` and the status/ingest disagreement. |
| G-07 | `read_state_file` maps only `NotFound` to `Ok(None)`; other I/O errors propagate. |

## Confirmed open

| Finding | Current evidence | Risk |
| --- | --- | --- |
| P1-04 relay trust | Relay grant still authorizes the Account to name any deployment, including one that already has a Router. | A compromised/incorrect relay can cross deployment boundaries; first-Router ownership stays in Account. This is a design decision, not a mechanical fix. |
| P1-08 spill-run liveness | The publish-side check remains a range check, not a free-list membership check. | A range-valid but freed run can be republished. |
| P3-14 `GLEAPH_*` typo | `ConfigEnv::from_env_iter` still has `_ => {}` for unknown variables. | A typo such as `GLEAPH_NETWOR` silently falls back to the file/default. |
| P3-16 layout dirty duplication | `panel.rs::step_layout` sets `dirty` in the `moving` branch and again after the branch. | Two copies of one condition drift; the duplicate is currently harmless. |

## Not re-audited in this record

The following lower-severity families were not exhaustively re-checked. Treat them as open until
inspected, not as fixed:

- D-01, D-04
- E-06, E-08, E-12
- F-01〜F-06, F-08〜F-10, F-12〜F-15
- G-01〜G-12 (except G-07, G-13)
- P3-01〜P3-04, P3-06, P3-08, P3-11〜P3-13, P3-15
- The remaining `ShardId`-family, `MutationId`, and `ShardEventSeq` newtype work
- The general wildcard / fail-open sweep

## Next

1. Decide P1-04 (relay cannot name an already-Router'd deployment?) before launch.
2. Fix P3-14 and P3-16 as small, local changes.
3. Re-check the not-re-audited families in batches, starting with D-01/D-04 and F-08/G-01.
