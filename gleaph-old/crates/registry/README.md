# gleaph-registry

Registry canister for managing Gleaph graph instances (one graph canister per tenant/project).

## Responsibilities

- Create/provision graph canisters
- Delete/deprovision graph canisters
- Store graph metadata and ACLs
- List graphs visible to the caller
- Persist registry state across upgrades
- Maintain compatibility with older registry snapshot format (legacy graph-map-only snapshots)

## Exposed Candid Methods (current)

Update methods:

- `create_graph(config: GraphConfig) -> Result<GraphInfo, text>`
- `delete_graph(id: nat64) -> bool`
- `grant_access(graph_id, principal, level) -> bool`

Query methods:

- `list_graphs() -> vec GraphInfo`

## Provisioning Notes

On `wasm32`, `create_graph` uses the IC management canister to:

- create a child canister
- install the embedded `gleaph-graph` wasm
- pass graph init args (`max_vertices`, `initial_edge_capacity`)

For large wasm payloads, the implementation can switch to chunked install.

On non-wasm targets (e.g. unit tests), provisioning is stubbed and returns `None`.

## Upgrade Compatibility

This crate includes a compatibility restore path for legacy snapshots that persisted only:

- `BTreeMap<u64, GraphRecord>`

The current format persists:

- `TenantRegistry { next_id, graphs }`

When loading legacy snapshots, `next_id` is rebuilt from the highest existing graph ID.

## Build

```bash
cargo build -p gleaph-registry
cargo build -p gleaph-registry --target wasm32-unknown-unknown
```
