# @gleaph/sdk

Workspace package for the JS/TS Gleaph SDK.

Current scope:

- core graph DTO types
- generic `GraphClient` contract
- `USE GRAPH` pushdown helpers
- IC canister transport factory
- `ApiValue` codec and request builders
- prepared-query codegen runtime surface (`executePrepared` / `executePreparedMutation`)
- codegen integration with `gleaph-codegen`

Planned scope:

- `frontend` sample integration

Planned entrypoint shape:

```ts
import { createIcGraphClient, unsupportedUseGraphPushdowns } from "@gleaph/sdk";

const graph = await createIcGraphClient({
  canisterId: "bkyz2-fmaaa-aaaaa-qaaaq-cai",
});

const plan = await graph.plan(
  makeQueryRequest(
    "USE myGraph MATCH ANY SHORTEST (a)-[:KNOWS]->{1,3}(b) RETURN b",
  ),
);

console.log(unsupportedUseGraphPushdowns(plan));
```

Generated prepared clients from `gleaph-codegen` can call the SDK directly:

```ts
import { createIcGraphClient } from "@gleaph/sdk";
import { createPreparedClient } from "./generated/gleaph.prepared";

const graph = await createIcGraphClient({
  canisterId: "bkyz2-fmaaa-aaaaa-qaaaq-cai",
});

const prepared = createPreparedClient(graph);
await prepared.find_user({ name: "alice" });
```
