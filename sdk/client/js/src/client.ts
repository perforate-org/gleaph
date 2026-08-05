import type {
  ApiPlanResponse,
  ApiPrepareRequest,
  ApiPrepareResponse,
  ApiPreparedQueryRequest,
  ApiQueryRequest,
  GqlMutationResult,
  GqlQueryResult,
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
import { makePreparedQueryRequest } from "./values.ts";

export interface GleaphTransport {
  explain(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  gqlQuery(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(graphName: string): Promise<PreparedManifest>;
  preparedQuery(request: ApiPreparedQueryRequest): Promise<GqlQueryResult>;
  preparedMutate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse>;
  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage>;
  dropPrepared(name: string): Promise<boolean>;
}

export interface GleaphClient {
  explain(request: ApiQueryRequest): Promise<ApiPlanResponse>;
  gqlQuery(request: ApiQueryRequest): Promise<GqlQueryResult>;
  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse>;
  listPrepared(graphName: string): Promise<PreparedManifest>;
  preparedQuery(request: ApiPreparedQueryRequest): Promise<GqlQueryResult>;
  preparedQuery(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult>;
  preparedMutate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  preparedMutate(
    name: string,
    params: Record<string, unknown | ApiValue> | undefined,
    clientMutationKey: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse>;
  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage>;
  dropPrepared(name: string): Promise<boolean>;
}

class TransportBackedGleaphClient implements GleaphClient {
  constructor(private readonly transport: GleaphTransport) {}

  explain(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return this.transport.explain(request);
  }

  gqlQuery(request: ApiQueryRequest): Promise<GqlQueryResult> {
    return this.transport.gqlQuery(request);
  }

  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return this.transport.prepare(request);
  }

  listPrepared(graphName: string): Promise<PreparedManifest> {
    return this.transport.listPrepared(graphName);
  }

  preparedQuery(
    requestOrName: ApiPreparedQueryRequest | string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult> {
    const request =
      typeof requestOrName === "string"
        ? makePreparedQueryRequest(requestOrName, params, sort)
        : requestOrName;
    return this.transport.preparedQuery(request);
  }

  preparedMutate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  preparedMutate(
    name: string,
    params: Record<string, unknown | ApiValue> | undefined,
    clientMutationKey: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
  preparedMutate(
    requestOrName: ApiPreparedMutationRequest | string,
    params?: Record<string, unknown | ApiValue>,
    clientMutationKey?: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult> {
    if (typeof requestOrName !== "string") {
      return this.transport.preparedMutate(requestOrName);
    }
    if (clientMutationKey === undefined) {
      throw new Error("clientMutationKey is required for prepared mutations");
    }
    return this.transport.preparedMutate({
      ...makePreparedQueryRequest(requestOrName, params, sort),
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

/**
 * Forwarding base for generated prepared clients.
 *
 * A `GleaphClientWrapper` delegates every operation to an inner `GleaphClient`, so generated
 * code can extend it and add prepared operations while keeping the full dynamic GQL surface on
 * the same value. The generated `withPreparedQueries` helper constructs it; the inner client is
 * never mutated.
 */
export class GleaphClientWrapper implements GleaphClient {
  constructor(protected readonly inner: GleaphClient) {}

  explain(request: ApiQueryRequest): Promise<ApiPlanResponse> {
    return this.inner.explain(request);
  }

  gqlQuery(request: ApiQueryRequest): Promise<GqlQueryResult> {
    return this.inner.gqlQuery(request);
  }

  prepare(request: ApiPrepareRequest): Promise<ApiPrepareResponse> {
    return this.inner.prepare(request);
  }

  listPrepared(graphName: string): Promise<PreparedManifest> {
    return this.inner.listPrepared(graphName);
  }

  preparedQuery(request: ApiPreparedQueryRequest): Promise<GqlQueryResult>;
  preparedQuery(
    name: string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult>;
  preparedQuery(
    requestOrName: ApiPreparedQueryRequest | string,
    params?: Record<string, unknown | ApiValue>,
    sort?: PreparedSortSpec[],
  ): Promise<GqlQueryResult> {
    if (typeof requestOrName === "string") {
      return this.inner.preparedQuery(requestOrName, params, sort);
    }
    return this.inner.preparedQuery(requestOrName);
  }

  preparedMutate(request: ApiPreparedMutationRequest): Promise<GqlMutationResult>;
  preparedMutate(
    name: string,
    params: Record<string, unknown | ApiValue> | undefined,
    clientMutationKey: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult>;
  preparedMutate(
    requestOrName: ApiPreparedMutationRequest | string,
    params?: Record<string, unknown | ApiValue>,
    clientMutationKey?: string,
    sort?: PreparedSortSpec[],
  ): Promise<GqlMutationResult> {
    if (typeof requestOrName !== "string") {
      return this.inner.preparedMutate(requestOrName);
    }
    if (clientMutationKey === undefined) {
      throw new Error("clientMutationKey is required for prepared mutations");
    }
    return this.inner.preparedMutate(requestOrName, params, clientMutationKey, sort);
  }

  bulkLoad(command: BulkLoadCommand): Promise<BulkLoadResponse> {
    return this.inner.bulkLoad(command);
  }

  bulkLoadStatus(request: BulkLoadStatusRequest): Promise<BulkLoadStatusPage> {
    return this.inner.bulkLoadStatus(request);
  }

  dropPrepared(name: string): Promise<boolean> {
    return this.inner.dropPrepared(name);
  }
}

export function createGleaphClientFromTransport(transport: GleaphTransport): GleaphClient {
  return new TransportBackedGleaphClient(transport);
}
