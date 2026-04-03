import type { ApiListPreparedResponse, ApiPlanResponse, ApiPrepareRequest, ApiPrepareResponse, ApiQueryRequest, ApiQueryResponse, ApiExecutePreparedRequest, ApiValue, PreparedSortSpec } from "./types";
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
    executePrepared(request: ApiExecutePreparedRequest): Promise<ApiQueryResponse>;
    executePrepared(name: string, params?: Record<string, unknown | ApiValue>, sort?: PreparedSortSpec[]): Promise<ApiQueryResponse>;
    executePreparedMutation(request: ApiExecutePreparedRequest): Promise<ApiQueryResponse>;
    executePreparedMutation(name: string, params?: Record<string, unknown | ApiValue>, sort?: PreparedSortSpec[]): Promise<ApiQueryResponse>;
    dropPrepared(name: string): Promise<boolean>;
}
export declare function createGraphClient(transport: GraphTransport): GraphClient;
//# sourceMappingURL=client.d.ts.map