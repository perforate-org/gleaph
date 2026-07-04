import {
  Actor,
  HttpAgent,
  type ActorSubclass,
  type Identity,
} from "@icp-sdk/core/agent";
import { Principal } from "@icp-sdk/core/principal";
import {
  createGraphClient,
  type GraphClient,
  type GraphTransport,
} from "./client";
import { GleaphCanisterError } from "./errors";
import { graphIdlFactory } from "./idl";
import type {
  ApiExecutePreparedRequest,
  ApiListPreparedResponse,
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  ApiQueryResponse,
} from "./types";
import { toApiParams } from "./values";

type Result<T> = { Ok: T; Err?: never } | { Ok?: never; Err: string };
type ActorInterfaceFactory = Parameters<typeof Actor.createActor>[0];

interface GraphActorMethods {
  explain(query: string): Promise<Result<ApiPlanResponse>>;
  query(
    query: string,
    params: [] | [[string, ReturnType<typeof toApiParams>[string]][]],
  ): Promise<Result<ApiQueryResponse>>;
  prepare(
    name: string,
    query: string,
    options: [] | [ApiPrepareRequest["options"]],
  ): Promise<Result<ApiPrepareResponse>>;
  list_prepared_api(): Promise<Result<ApiListPreparedResponse>>;
  execute_prepared_query(
    name: string,
    params: [string, ReturnType<typeof toApiParams>[string]][],
    sort: [] | [NonNullable<ApiExecutePreparedRequest["sort"]>],
  ): Promise<Result<ApiQueryResponse>>;
  execute_prepared_update(
    name: string,
    params: [string, ReturnType<typeof toApiParams>[string]][],
  ): Promise<Result<ApiQueryResponse>>;
  drop_prepared(name: string): Promise<Result<{ dropped: boolean }>>;
}

type GraphActor = ActorSubclass<GraphActorMethods>;

export interface IcGraphTransportOptions {
  canisterId: string | Principal;
  host?: string;
  identity?: Identity;
  fetchRootKey?: boolean;
}

function principalFrom(canisterId: string | Principal): Principal {
  return typeof canisterId === "string"
    ? Principal.fromText(canisterId)
    : canisterId;
}

function toCandidParams(params: Record<string, ReturnType<typeof toApiParams>[string]>): [
  string,
  ReturnType<typeof toApiParams>[string],
][] {
  return Object.entries(params);
}

function unwrapResult<T>(result: Result<T>): T {
  if ("Ok" in result) {
    return result.Ok;
  }
  throw new GleaphCanisterError(result.Err ?? "unknown Gleaph canister error", result);
}

class IcGraphTransport implements GraphTransport {
  constructor(private readonly actor: GraphActor) {}

  async plan(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return unwrapResult<ApiPlanResponse>(await this.actor.explain(request.query));
  }

  async execute(request: ApiQueryRequest): Promise<ApiQueryResponse> {
    return unwrapResult<ApiQueryResponse>(
      await this.actor.query(request.query, [toCandidParams(toApiParams(request.params))]),
    );
  }

  async prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return unwrapResult<ApiPrepareResponse>(
      await this.actor.prepare(request.name, request.query, request.options ? [request.options] : []),
    );
  }

  async listPrepared(): Promise<ApiListPreparedResponse> {
    return unwrapResult<ApiListPreparedResponse>(await this.actor.list_prepared_api());
  }

  async executePreparedQuery(
    request: ApiExecutePreparedRequest,
  ): Promise<ApiQueryResponse> {
    return unwrapResult<ApiQueryResponse>(
      await this.actor.execute_prepared_query(
        request.name,
        toCandidParams(toApiParams(request.params)),
        request.sort ? [request.sort] : [],
      ),
    );
  }

  async executePreparedUpdate(
    request: ApiExecutePreparedRequest,
  ): Promise<ApiQueryResponse> {
    return unwrapResult<ApiQueryResponse>(
      await this.actor.execute_prepared_update(
        request.name,
        toCandidParams(toApiParams(request.params)),
      ),
    );
  }

  async dropPrepared(name: string): Promise<boolean> {
    const result = unwrapResult<{ dropped: boolean }>(await this.actor.drop_prepared(name));
    return result.dropped;
  }
}

export async function createIcGraphTransport(
  options: IcGraphTransportOptions,
): Promise<GraphTransport> {
  const agentOptions: { host: string; identity?: Identity } = {
    host: options.host ?? "https://icp-api.io",
  };
  if (options.identity !== undefined) {
    agentOptions.identity = options.identity;
  }
  const agent = HttpAgent.createSync(agentOptions);
  if (options.fetchRootKey) {
    await agent.fetchRootKey();
  }
  const actor = Actor.createActor<GraphActorMethods>(
    graphIdlFactory as unknown as ActorInterfaceFactory,
    {
      agent,
      canisterId: principalFrom(options.canisterId),
    },
  );
  return new IcGraphTransport(actor);
}

export async function createIcGraphClient(
  options: IcGraphTransportOptions,
): Promise<GraphClient> {
  const transport = await createIcGraphTransport(options);
  return createGraphClient(transport);
}
