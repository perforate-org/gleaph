# gleaph-gql-executor

Executes `PhysicalPlan` from `gleaph-gql-planner` against a `GraphRead + GraphWrite` implementation from `gleaph-graph-kernel`.

## Property projection

Scans and `Expand` operators may carry `property_projection`, `edge_property_projection`, and `dst_property_projection`. The planner fills these from `RETURN` and downstream expressions (`apply_node_property_projections` in `gleaph-gql-planner`). The executor hydrates only those keys when present.

## Terminal `flush`

After a successful plan, `execute_plan_with_context` may call `graph.flush()`:

- **Skipped** for read-only plans: when `PhysicalPlan::has_dml()` is false (including DML nested in sub-plans), the executor skips the terminal flush unless `ExecutionContext::force_terminal_graph_flush` is true.
- **Run** when the plan contains any DML operator, or when `force_terminal_graph_flush` is set (for example a procedure wrote to the graph without DML in the plan, or callers need an explicit stable-memory round-trip).

Nested execution passes `flush_at_end: false` so only the outer plan performs one flush.

For why read-only paths on the PMA kernel overlay skip flush safely, see the module-level documentation in `crates/graph-store/src/integration/graph_read_impl.rs`.

## Federation (`USE GRAPH` across canisters)

**Graph-unit federation** (explicit `USE GRAPH`, routed queries, ACL delegation) is covered in [`crates/graph/FEDERATION.md`](../graph/FEDERATION.md). **Intra-graph sharding** (one logical graph, many canisters; not controlled from GQL) is described in [`crates/graph/DATA_PLANE_SHARDING.md`](../graph/DATA_PLANE_SHARDING.md), including optional `{edge}__hop_aux` for auxiliary/observability use.
