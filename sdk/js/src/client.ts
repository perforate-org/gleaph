import type {
  ApiListPreparedResponse,
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiQueryRequest,
  ApiQueryResponse,
  ApiExecutePreparedRequest,
  ApiValue,
  PreparedSortSpec,
} from "./types";
import { makeExecutePreparedRequest } from "./values";

export interface GraphTransport {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<ApiQueryResponse>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(): Promise<ApiListPreparedResponse>;
  executePreparedQuery(request: ApiExecutePreparedRequest): Promise<ApiQueryResponse>;
  executePreparedUpdate(request: ApiExecutePreparedRequest): Promise<ApiQueryResponse>;
  dropPrepared(name: string): Promise<boolean>;
}

export interface GraphClient {
  plan(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  execute(request: ApiQueryRequest): Promise<ApiQueryResponse>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(): Promise<ApiListPreparedResponse>;
  executePrepared(
    request: ApiExecutePreparedRequest,
  ): Promise<ApiQueryResponse>;
  executePrepared(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<ApiQueryResponse>;
  executePreparedMutation(
    request: ApiExecutePreparedRequest,
  ): Promise<ApiQueryResponse>;
  executePreparedMutation(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<ApiQueryResponse>;
  dropPrepared(name: string): Promise<boolean>;
}

class TransportBackedGraphClient implements GraphClient {
  constructor(private readonly transport: GraphTransport) {}

  plan(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return this.transport.plan(request);
  }

  execute(request: ApiQueryRequest): Promise<ApiQueryResponse> {
    return this.transport.execute(request);
  }

  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return this.transport.prepare(request);
  }

  listPrepared(): Promise<ApiListPreparedResponse> {
    return this.transport.listPrepared();
  }

  executePrepared(
    requestOrName: ApiExecutePreparedRequest | string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<ApiQueryResponse> {
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
  ): Promise<ApiQueryResponse> {
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
