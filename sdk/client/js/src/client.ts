import type {
  ApiListPreparedResponse,
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  GqlMutationResult,
  GqlQueryResult,
  ApiExecutePreparedRequest,
  ApiValue,
  PreparedManifest,
  PreparedSortSpec,
} from "./types";
import { makeExecutePreparedRequest } from "./values";

export interface GraphTransport {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(): Promise<ApiListPreparedResponse>;
  getPreparedManifest(graphName: string): Promise<PreparedManifest>;
  executePreparedQuery(request: ApiExecutePreparedRequest): Promise<GqlQueryResult>;
  executePreparedUpdate(request: ApiExecutePreparedRequest): Promise<GqlMutationResult>;
  dropPrepared(name: string): Promise<boolean>;
}

export interface GraphClient {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(): Promise<ApiListPreparedResponse>;
  getPreparedManifest(graphName: string): Promise<PreparedManifest>;
  executePrepared(request: ApiExecutePreparedRequest): Promise<GqlQueryResult>;
  executePrepared(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult>;
  executePreparedMutation(request: ApiExecutePreparedRequest): Promise<GqlMutationResult>;
  executePreparedMutation(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
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

  listPrepared(): Promise<ApiListPreparedResponse> {
    return this.transport.listPrepared();
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

  executePreparedMutation(
    requestOrName: ApiExecutePreparedRequest | string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult> {
    const request =
      typeof requestOrName === "string"
        ? makeExecutePreparedRequest(requestOrName, params, sort)
        : requestOrName;
    return this.transport.executePreparedUpdate(request);
  }

  dropPrepared(name: string): Promise<boolean> {
    return this.transport.dropPrepared(name);
  }
}

export function createGraphClient(transport: GraphTransport): GraphClient {
  return new TransportBackedGraphClient(transport);
}
