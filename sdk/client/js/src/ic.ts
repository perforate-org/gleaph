import { Actor, HttpAgent, type ActorSubclass, type Identity } from "@icp-sdk/core/agent";
import { IDL } from "@icp-sdk/core/candid";
import { Principal } from "@icp-sdk/core/principal";
import { createGraphClient, type GraphClient, type GraphTransport } from "./client";
import { GleaphCanisterError } from "./errors";
import { GqlQueryRows, graphIdlFactory } from "./idl";
import type {
  ApiExecutePreparedRequest,
  ApiPreparedMutationRequest,
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  GqlMutationResult,
  GqlQueryResult,
  PreparedManifest,
  ReadMode,
} from "./types";
import type {
  BulkLoadCommand,
  BulkLoadResponse,
  BulkLoadStatusPage,
  BulkLoadStatusRequest,
} from "./bulk";
import { toApiParams } from "./values";
import { encodeCanonicalGqlValue } from "./canonical-value";

type Result<T> = { Ok: T; Err?: never } | { Ok?: never; Err: Record<string, unknown> };
type ActorInterfaceFactory = Parameters<typeof Actor.createActor>[0];

interface GraphActorMethods {
  explain(query: string): Promise<Result<ApiPlanResponse>>;
  gql_query(
    query: string,
    params: Uint8Array,
    read_mode: ReadMode,
  ): Promise<Result<GqlQueryWireResult>>;
  gql_mutate(
    query: string,
    params: Uint8Array,
    client_mutation_key: string,
  ): Promise<Result<GqlQueryWireResult>>;
  prepare(
    name: string,
    query: string,
    options: [] | [ApiPrepareRequest["options"]],
  ): Promise<Result<ApiPrepareResponse>>;
  list_prepared(graphName: string): Promise<Result<PreparedManifest>>;
  prepared_query(
    name: string,
    params: Uint8Array,
    sort: [] | [{ key: string; direction: string }[]],
    read_mode: ReadMode,
  ): Promise<Result<GqlQueryWireResult>>;
  prepared_mutate(
    name: string,
    params: Uint8Array,
    client_mutation_key: string,
  ): Promise<Result<GqlQueryWireResult>>;
  bulk_load(command: BulkLoadCommand): Promise<Result<BulkLoadResponse>>;
  bulk_load_status(
    logicalGraphName: string,
    clientBulkKey: string,
    receiptCursor: [] | [number],
    maxReceipts: number,
  ): Promise<Result<BulkLoadStatusPage>>;
  drop_prepared(name: string): Promise<Result<null>>;
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
          shards: result.token[0].shards.map((shard) => {
            const labelStatsSeq = shard.label_stats_seq[0];
            return labelStatsSeq === undefined
              ? { shard_id: shard.shard_id }
              : { shard_id: shard.shard_id, label_stats_seq: labelStatsSeq };
          }),
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
        await this.actor.gql_query(request.query, encodeParams(request.params), { Eventual: null }),
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

  async getPreparedManifest(graphName: string): Promise<PreparedManifest> {
    return unwrapResult<PreparedManifest>(await this.actor.list_prepared(graphName));
  }

  async executePreparedQuery(request: ApiExecutePreparedRequest): Promise<GqlQueryResult> {
    const sort: [] | [{ key: string; direction: string }[]] =
      request.sort && request.sort.length > 0
        ? [request.sort.map(({ key, direction }) => ({ key, direction }))]
        : [];
    return toGqlQueryResult(
      unwrapResult<GqlQueryWireResult>(
        await this.actor.prepared_query(request.name, encodeParams(request.params), sort, {
          Eventual: null,
        }),
      ),
    );
  }

  async executePreparedUpdate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult> {
    return toGqlQueryResult(
      unwrapResult<GqlQueryWireResult>(
        await this.actor.prepared_mutate(
          request.name,
          encodeParams(request.params),
          request.client_mutation_key,
        ),
      ),
    );
  }

  async bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse> {
    return unwrapResult<BulkLoadResponse>(await this.actor.bulk_load(command));
  }

  async bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage> {
    return unwrapResult<BulkLoadStatusPage>(
      await this.actor.bulk_load_status(
        request.logical_graph_name,
        request.client_bulk_key,
        request.receipt_cursor === undefined ? [] : [request.receipt_cursor],
        request.max_receipts,
      ),
    );
  }

  async dropPrepared(name: string): Promise<boolean> {
    unwrapResult<null>(await this.actor.drop_prepared(name));
    return true;
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
