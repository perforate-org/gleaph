import { Actor, HttpAgent, type ActorSubclass, type Identity } from "@icp-sdk/core/agent";
import { IDL } from "@icp-sdk/core/candid";
import { Principal } from "@icp-sdk/core/principal";
import { createGraphClient, type GraphClient, type GraphTransport } from "./client";
import { GleaphCanisterError } from "./errors";
import { GqlQueryRows, graphIdlFactory } from "./idl";
import type {
  ApiExecutePreparedRequest,
  ApiListPreparedResponse,
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  GqlMutationResult,
  GqlQueryResult,
  PreparedManifest,
} from "./types";
import { toApiParams } from "./values";
import { encodeCanonicalGqlValue } from "./canonical-value";

type Result<T> = { Ok: T; Err?: never } | { Ok?: never; Err: Record<string, unknown> };
type ActorInterfaceFactory = Parameters<typeof Actor.createActor>[0];

interface GraphActorMethods {
  explain(query: string): Promise<Result<ApiPlanResponse>>;
  query(query: string, params: Uint8Array): Promise<Result<GqlQueryWireResult>>;
  prepare(
    name: string,
    query: string,
    options: [] | [ApiPrepareRequest["options"]],
  ): Promise<Result<ApiPrepareResponse>>;
  list_prepared_api(): Promise<Result<ApiListPreparedResponse>>;
  prepared_manifest(graphName: string): Promise<Result<PreparedManifest>>;
  prepared_execute_query(name: string, params: Uint8Array): Promise<Result<GqlQueryWireResult>>;
  prepared_execute_update(name: string, params: Uint8Array): Promise<Result<bigint>>;
  drop_prepared(name: string): Promise<Result<{ dropped: boolean }>>;
}

type GqlQueryWireResult = {
  row_count: bigint;
  rows_blob: [] | [Uint8Array];
  phase: [] | [Record<string, null>];
  token:
    | []
    | [{ mutation_id: bigint; shards: { shard_id: number; label_stats_seq: [] | [bigint] }[] }];
};

type GraphActor = ActorSubclass<GraphActorMethods>;

function decodeRows(result: GqlQueryWireResult): Record<string, import("./types").ApiValue>[] {
  if (result.rows_blob.length === 0) {
    return [];
  }
  const [decoded] = IDL.decode([GqlQueryRows], result.rows_blob[0]);
  return (decoded as { rows: { columns: [string, unknown][] }[] }).rows.map(({ columns }) =>
    Object.fromEntries(columns.map(([key, value]) => [key, fromWireValue(value)])),
  );
}

function fromWireValue(value: unknown): import("./types").ApiValue {
  if (!value || typeof value !== "object") {
    throw new Error("invalid GQL wire value");
  }
  const record = value as Record<string, unknown>;
  if ("Record" in record) {
    return {
      Record: Object.fromEntries(
        (record.Record as [string, unknown][]).map(([key, nested]) => [key, fromWireValue(nested)]),
      ),
    };
  }
  if ("List" in record) {
    return { List: (record.List as unknown[]).map(fromWireValue) };
  }
  if ("ExtensionLeaf" in record || "ValueBinary" in record) {
    throw new Error("GQL extension values require a typed SDK decoder");
  }
  return value as import("./types").ApiValue;
}

function toGqlQueryResult(result: GqlQueryWireResult): GqlQueryResult {
  const phase =
    result.phase.length === 0 ? null : (Object.keys(result.phase[0])[0] as GqlQueryResult["phase"]);
  const token =
    result.token.length === 0
      ? null
      : {
          mutation_id: result.token[0].mutation_id,
          shards: result.token[0].shards.map((shard) => ({
            shard_id: shard.shard_id,
            label_stats_seq:
              shard.label_stats_seq.length === 0 ? undefined : shard.label_stats_seq[0],
          })),
        };
  return {
    row_count: result.row_count,
    rows: decodeRows(result),
    phase,
    token,
  };
}

export interface IcGraphTransportOptions {
  canisterId: string | Principal;
  host?: string;
  identity?: Identity;
  fetchRootKey?: boolean;
}

function principalFrom(canisterId: string | Principal): Principal {
  return typeof canisterId === "string" ? Principal.fromText(canisterId) : canisterId;
}

function encodeParams(params: Record<string, unknown>): Uint8Array {
  return encodeCanonicalGqlValue({ Record: toApiParams(params) });
}

function unwrapResult<T>(result: Result<T>): T {
  if ("Ok" in result) {
    return result.Ok;
  }
  const message = result.Err ? JSON.stringify(result.Err) : "unknown Gleaph canister error";
  throw new GleaphCanisterError(message, result);
}

class IcGraphTransport implements GraphTransport {
  constructor(private readonly actor: GraphActor) {}

  async plan(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return unwrapResult<ApiPlanResponse>(await this.actor.explain(request.query));
  }

  async execute(request: ApiQueryRequest): Promise<GqlQueryResult> {
    return toGqlQueryResult(
      unwrapResult<GqlQueryWireResult>(
        await this.actor.query(request.query, encodeParams(request.params)),
      ),
    );
  }

  async prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return unwrapResult<ApiPrepareResponse>(
      await this.actor.prepare(
        request.name,
        request.query,
        request.options ? [request.options] : [],
      ),
    );
  }

  async listPrepared(): Promise<ApiListPreparedResponse> {
    return unwrapResult<ApiListPreparedResponse>(await this.actor.list_prepared_api());
  }

  async getPreparedManifest(graphName: string): Promise<PreparedManifest> {
    return unwrapResult<PreparedManifest>(await this.actor.prepared_manifest(graphName));
  }

  async executePreparedQuery(request: ApiExecutePreparedRequest): Promise<GqlQueryResult> {
    if (request.sort !== undefined && request.sort.length > 0) {
      throw new Error("prepared sort is not supported by the current Router wire API");
    }
    return toGqlQueryResult(
      unwrapResult<GqlQueryWireResult>(
        await this.actor.prepared_execute_query(request.name, encodeParams(request.params)),
      ),
    );
  }

  async executePreparedUpdate(request: ApiExecutePreparedRequest): Promise<GqlMutationResult> {
    const rowCount = unwrapResult<bigint>(
      await this.actor.prepared_execute_update(request.name, encodeParams(request.params)),
    );
    return { row_count: rowCount };
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

export async function createIcGraphClient(options: IcGraphTransportOptions): Promise<GraphClient> {
  const transport = await createIcGraphTransport(options);
  return createGraphClient(transport);
}
