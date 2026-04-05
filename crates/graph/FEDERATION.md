# Graph-unit federation (multi-canister `USE GRAPH`)

This document fixes **acceptance criteria** for **graph-unit** federation: distinct **logical** graphs; one canister delegates a sub-plan to another **by name** via `execute_routed_query_batch` / `execute_routed_query_with_subject`, with explicit `USE GRAPH` in the query plan.

**Intra-graph sharding** (one **logical** graph physically spanning multiple canisters; **no** GQL syntax to control shards) is a **separate** concept. See [`DATA_PLANE_SHARDING.md`](./DATA_PLANE_SHARDING.md). Do not use `USE GRAPH` to stitch what is **already** one graph at the language level—that routing belongs under the executor/kernel for that single graph.

## Allowed query shapes

- Remote pushdown must be **statically supported** by the planner (`analyze_remote_use_graph_pushdown` / `check_remote_use_graph_pushdown` in `gleaph-gql-planner`). Unsupported shapes fail at plan time on the **caller** canister with a clear error.
- The remote side re-parses generated GQL from `subplan_to_routed_query` (`crates/graph/src/graph_registry.rs`). Only shapes accepted by both the planner check and that serializer are valid.

## Caller / ACL model

- **Without delegation**: inter-canister `msg_caller()` is the peer graph canister. The remote ACL map is keyed by that principal (or the anonymous/read path as today).
- **With `query_subject`**: the **home** canister may pass the end-user principal. The remote applies ACL using **`query_subject`** (not `msg_caller`) **only if** `msg_caller()` is a **controller** or is listed in **`GleaphService::federation_trusted_callers`**. Otherwise the call is rejected.
- Configure trusted peers on each **remote** graph canister after deployment (they are not magic defaults).

## SLO and limits

- **Batch size**: `execute_routed_query_batch` rejects more than `MAX_FEDERATION_ROUTED_PARAM_ROWS` parameter rows (see `crates/graph/src/lib.rs`) to bound payload size and cycles.
- **Inter-canister calls**: one batch call replaces one call per input row for supported remote `USE GRAPH` plans. Timeouts and cycle limits follow the Internet Computer and `bounded_wait` behavior.

## Operations endpoints

| Method | Role |
|--------|------|
| `execute_routed_query` | Legacy 2-tuple entry; ACL = `msg_caller()` only. |
| `execute_routed_query_with_subject` | Optional third principal for ACL when caller is trusted. |
| `execute_routed_query_batch` | Same as `with_subject`, but many param rows in one round-trip. |

## Observability

- On wasm32, `ic_cdk::println!` lines prefixed with `gleaph-fed` log batch size, delegation usage, and delegation rejection (see `crates/graph/src/lib.rs`).
