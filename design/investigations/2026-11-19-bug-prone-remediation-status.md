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

## Update (same day, after this record)

P3-14 and P3-16 were fixed after the table above was written:

- `ConfigEnv::from_env_iter` now rejects an unknown `GLEAPH_*` variable with `UnknownEnv`
  (unrelated non-`GLEAPH_` variables are still ignored).
- `panel.rs::step_layout` sets `dirty` once in the moving branch; the duplicate condition is gone.

The "Confirmed open" table remains the state at the start of this re-check; P1-04 and P1-08 are the
only items from that table still open.

## P1 closure (2026-11-19)

- **P1-04 closed.** The per-deployment singleton is enforced below the grant scope:
  `store::check_singleton_issuance` returns `RouterAlreadyIssued` for a Completed first job and
  `RouterIssuanceInProgress` while the first job is live. Added
  `relay_issuer_cannot_issue_a_second_router_for_one_deployment`; a Relay can name another
  deployment but still cannot issue a second Router for it. The remaining Relay trust (it may
  issue non-Router resources for a deployment on the account's behalf) is the intended relay
  contract, not an unchecked Router path.
- **P1-08 closed.** The publish-side check is now `spill_is_live` ->
  `SpillRunStore::is_live_raw`, which reads the run's used-length header and compares it with
  `FREE_MARKER`; release writes that marker, so a freed/reused run is not accepted by range
  alone. `publish_bucket_descriptor` asserts liveness for both the edge and values planes before
  writing the descriptor.

No confirmed-open P1/P2 items remain from the 2026-11-14 audit. The un-re-audited list above
still needs a fresh pass before claiming the whole report is closed.

## Additional verification (2026-11-19)

A batch code pass closed most of the previous "not re-audited" list without code changes:
F-08 (last-owner guard uses `InvalidRole`), F-09 (shared `TextCanisterInitArgs`), F-10
(artifact wire re-exported from `artifact-api`), F-13 (single `analyzer::is_registered`), F-14
(one budget derivation), F-15 (caller equality), G-03 (one `ordinal_id!` macro), G-05
(`MutationId`/`LocalVertexId` newtypes), G-06 (pure native-testable guards), G-08
(`ThemeChoice::ALL`/`from_index`), G-10 (`Option<GraphId>`), G-12 (`ScanCursor`), E-08
(`ShardProgress`), E-09 (`Option<&[..]>`), E-10 (`Quarantined { next_retry_ns }`), and E-12
(the obsolete peer-sync module is gone).

G-04 was narrowed: `IndexLabelIntersectionRequest.vertex_label_ids`,
`LabelPostingCursor.vertex_id`, `LabelLookupPageRequest.vertex_label_id`, and
`LabelIntersectionPageRequest.{walk_label_id,sieve_label_ids}` now use the posting newtypes;
raw ids remain only at the graph-index store key boundary.
