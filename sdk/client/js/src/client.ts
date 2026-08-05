import type {
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  GqlMutationResult,
  GqlQueryResult,
  ApiExecutePreparedRequest,
  ApiPreparedMutationRequest,
  ApiValue,
  PreparedManifest,
  PreparedSortSpec,
} from "./types.ts";
import type {
  BulkLoadCommand,
  BulkLoadResponse,
  BulkLoadStatusPage,
  BulkLoadStatusRequest,
} from "./bulk.ts";
import { makeExecutePreparedRequest } from "./values.ts";

export interface GraphTransport {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  getPreparedManifest(graphName: string): Promise<PreparedManifest>;
  executePreparedQuery(request: ApiExecutePreparedRequest): Promise<GqlQueryResult>;
  executePreparedUpdate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse>;
  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage>;
  dropPrepared(name: string): Promise<boolean>;
}

export interface GraphClient {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  getPreparedManifest(graphName: string): Promise<PreparedManifest>;
  executePrepared(request: ApiExecutePreparedRequest): Promise<GqlQueryResult>;
  executePrepared(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult>;
  executePreparedMutation(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  executePreparedMutation(
    name: string,
    params: Record<string, unknown | ApiValue> | undefined,
    clientMutationKey: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse>;
  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage>;
  dropPrepared(name: string): Promise<boolean>;
}

class TransportBackedGraphClient implements GraphClient {
  constructor(private readonly transport: GraphTransport) {}

  plan(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return this.transport.plan(request);
  }

  execute(request: ApiQueryRequest): Promise<GqlQueryResult> {
    return this.transport.execute(request);
  }

  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return this.transport.prepare(request);
  }

  getPreparedManifest(graphName: string): Promise<PreparedManifest> {
    return this.transport.getPreparedManifest(graphName);
  }

  executePrepared(
    requestOrName: ApiExecutePreparedRequest | string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult> {
    const request =
      typeof requestOrName === "string"
        ? makeExecutePreparedRequest(requestOrName, params, sort)
        : requestOrName;
    return this.transport.executePreparedQuery(request);
  }

  executePreparedMutation(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  executePreparedMutation(
    name: string,
    params: Record<string, unknown | ApiValue> | undefined,
    clientMutationKey: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
  executePreparedMutation(
    requestOrName: ApiPreparedMutationRequest | string,
    params?: Record<string, unknown | ApiValue>,
    clientMutationKey?: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult> {
    if (typeof requestOrName !== "string") {
      return this.transport.executePreparedUpdate(requestOrName);
    }
    if (clientMutationKey === undefined) {
      throw new Error("clientMutationKey is required for prepared mutations");
    }
    return this.transport.executePreparedUpdate({
      ...makeExecutePreparedRequest(requestOrName, params, sort),
      client_mutation_key: clientMutationKey,
    });
  }

  dropPrepared(name: string): Promise<boolean> {
    return this.transport.dropPrepared(name);
  }

  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse> {
    return this.transport.bulkLoad(command);
  }

  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage> {
    return this.transport.bulkLoadStatus(request);
  }
}

export function createGraphClient(transport: GraphTransport): GraphClient {
  return new TransportBackedGraphClient(transport);
}
