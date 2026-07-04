import type { ApiPlanResponse, ApiPreparedQueryInfo, ApiQueryResponse, ApiUseGraphPushdownInfo } from "./types";
export declare const USE_GRAPH_PUSHDOWN_WARNING_PREFIX = "remote USE GRAPH pushdown unavailable";
export declare function isUnsupportedUseGraphPushdown(info: ApiUseGraphPushdownInfo): boolean;
export declare function unsupportedUseGraphPushdowns(value: Pick<ApiPlanResponse, "use_graph_pushdown"> | Pick<ApiPreparedQueryInfo, "use_graph_pushdown"> | Pick<ApiQueryResponse, "use_graph_pushdown">): ApiUseGraphPushdownInfo[];
export declare function useGraphPushdownWarnings(value: Pick<ApiQueryResponse, "execution">): string[];
//# sourceMappingURL=helpers.d.ts.map