# Graph Registry Canister Design

## Goal

Define the registry layer required before full `USE GRAPH` runtime routing:

- Graph name to canister resolution.
- Access control on graph lookup.
- Graph canister provisioning lifecycle.
- Stable error semantics for callers.

## Responsibilities

Registry canister responsibilities:

- Resolve `graph_name` to graph execution target (`canister_id`).
- Store graph metadata and lifecycle state.
- Enforce lookup/admin permissions.
- Provision new graph canisters and attach existing graph canisters.

Graph execution canister responsibilities:

- Parse/plan/execute queries against one graph backend.
- Execute routed requests after registry resolution.

## Data Model

Minimal registry record:

- `graph_name: String` (unique, case policy defined by registry)
- `canister_id: Principal`
- `owner: Principal`
- `admins: Vec<Principal>`
- `status: GraphStatus` (`Active`, `ReadOnly`, `Deprecated`, `Deleting`)
- `version: u64`
- `updated_at_ns: u64`
- `provisioning_state: ProvisioningState`

Provisioning states:

- `None`
- `Pending { request_id }`
- `Failed { request_id, reason }`

## APIs

Read APIs:

- `resolve_graph(name, caller) -> GraphResolution`
- `list_graphs(caller, cursor, limit) -> Page<GraphRecordSummary>`

Admin APIs:

- `create_graph(name, owner, options) -> CreateGraphResult`
- `attach_existing_graph(name, canister_id, owner) -> AttachResult`
- `update_graph(name, patch) -> UpdateResult`
- `deprecate_graph(name) -> Result`

## Provisioning Flow

1. Validate caller and graph name uniqueness.
2. Persist `Pending` provisioning state with idempotency key.
3. Create graph canister.
4. Initialize graph canister (schema/bootstrap options).
5. Persist final mapping and mark state `None`.

Failure handling:

- Persist `Failed` with reason.
- Keep idempotency key to safely retry.
- Expose retry/reconcile admin operation.

## Error Model

Registry/domain errors:

- `NotFound`
- `Forbidden`
- `Conflict`
- `InvalidName`
- `ProvisioningFailed`
- `Unavailable`

Transport/integration errors:

- `Timeout`
- `RemoteRejected`
- `DecodeError`

## Runtime Contract for Query Layer

`USE GRAPH <name>` pipeline:

1. Resolve alias (`CURRENT_GRAPH`, `HOME_GRAPH`) to concrete name.
2. Call registry resolver.
3. If resolution is local, execute sub-plan in-process.
4. If resolution is remote, delegate to remote graph canister router.

The caller sees normalized execution errors regardless of local/remote execution.

## Operational Policies

- Cache resolved entries with short TTL.
- Invalidate cache on explicit graph update/deprecate events.
- Enforce provisioning quota/cycles guardrails per owner/tenant.
- Keep compatibility policy for graph rename/migration (forwarding window).
