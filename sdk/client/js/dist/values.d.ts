import type { ApiExecutePreparedRequest, ApiPathElement, ApiPrepareRequest, ApiQueryRequest, ApiValue, PreparedOptions, PreparedSortSpec } from "./types";
export declare function isApiValue(value: unknown): value is ApiValue;
export declare function toApiPathElement(value: unknown): ApiPathElement;
export declare function toApiValue(value: unknown): ApiValue;
export declare function fromApiValue(value: ApiValue): unknown;
export declare function toApiParams(params?: Record<string, unknown | ApiValue>): Record<string, ApiValue>;
export declare function makeQueryRequest(query: string, params?: Record<string, unknown | ApiValue>): ApiQueryRequest;
export declare function makePrepareRequest(name: string, query: string, options?: PreparedOptions): ApiPrepareRequest;
export declare function makeExecutePreparedRequest(name: string, params?: Record<string, unknown | ApiValue>, sort?: PreparedSortSpec[]): ApiExecutePreparedRequest;
//# sourceMappingURL=values.d.ts.map