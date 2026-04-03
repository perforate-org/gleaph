import { Principal } from "@icp-sdk/core/principal";
import { createIcGraphClient, makeQueryRequest, unsupportedUseGraphPushdowns, useGraphPushdownWarnings, } from "@gleaph/sdk";
export async function previewUseGraphPushdown(canisterId) {
    const graph = await createIcGraphClient({ canisterId });
    const plan = await graph.plan(makeQueryRequest("USE tenantGraph MATCH (u:User) WHERE u.owner = $owner RETURN u.name AS name", { owner: Principal.anonymous() }));
    return {
        unsupported: unsupportedUseGraphPushdowns(plan),
        explain: plan.explain,
    };
}
export async function executeSimpleQuery(canisterId) {
    const graph = await createIcGraphClient({ canisterId });
    const result = await graph.execute(makeQueryRequest("MATCH (u:User) RETURN u.name AS name LIMIT 5"));
    return {
        rows: result.execution.rows,
        warnings: useGraphPushdownWarnings(result),
    };
}
