import { Actor, HttpAgent, } from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import { createGraphClient, } from "./client";
import { GleaphCanisterError } from "./errors";
import { graphIdlFactory } from "./idl";
import { toApiParams } from "./values";
function principalFrom(canisterId) {
    return typeof canisterId === "string"
        ? Principal.fromText(canisterId)
        : canisterId;
}
function toCandidParams(params) {
    return Object.entries(params);
}
function unwrapResult(result) {
    if ("Ok" in result) {
        return result.Ok;
    }
    throw new GleaphCanisterError(result.Err ?? "unknown Gleaph canister error", result);
}
class IcGraphTransport {
    actor;
    constructor(actor) {
        this.actor = actor;
    }
    async plan(request) {
        return unwrapResult(await this.actor.explain(request.query));
    }
    async execute(request) {
        return unwrapResult(await this.actor.query(request.query, [toCandidParams(toApiParams(request.params))]));
    }
    async prepare(request) {
        return unwrapResult(await this.actor.prepare(request.name, request.query, request.options ? [request.options] : []));
    }
    async listPrepared() {
        return unwrapResult(await this.actor.list_prepared_api());
    }
    async executePreparedQuery(request) {
        return unwrapResult(await this.actor.execute_prepared_query(request.name, toCandidParams(toApiParams(request.params)), request.sort ? [request.sort] : []));
    }
    async executePreparedUpdate(request) {
        return unwrapResult(await this.actor.execute_prepared_update(request.name, toCandidParams(toApiParams(request.params))));
    }
    async dropPrepared(name) {
        const result = unwrapResult(await this.actor.drop_prepared(name));
        return result.dropped;
    }
}
export async function createIcGraphTransport(options) {
    const agentOptions = {
        host: options.host ?? "https://icp-api.io",
    };
    if (options.identity !== undefined) {
        agentOptions.identity = options.identity;
    }
    const agent = HttpAgent.createSync(agentOptions);
    if (options.fetchRootKey) {
        await agent.fetchRootKey();
    }
    const actor = Actor.createActor(graphIdlFactory, {
        agent,
        canisterId: principalFrom(options.canisterId),
    });
    return new IcGraphTransport(actor);
}
export async function createIcGraphClient(options) {
    const transport = await createIcGraphTransport(options);
    return createGraphClient(transport);
}
