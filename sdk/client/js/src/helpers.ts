import type {
  ApiPlanResponse,
  ApiPreparedQueryInfo,
  ApiQueryResponse,
  ApiUseGraphPushdownInfo,
} from "./types";

export const USE_GRAPH_PUSHDOWN_WARNING_PREFIX =
  "remote USE GRAPH pushdown unavailable";

export function isUnsupportedUseGraphPushdown(
  info: ApiUseGraphPushdownInfo,
): boolean {
  return !info.supported;
}

export function unsupportedUseGraphPushdowns(
  value:
    | Pick<ApiPlanResponse, "use_graph_pushdown">
    | Pick<ApiPreparedQueryInfo, "use_graph_pushdown">
    | Pick<ApiQueryResponse, "use_graph_pushdown">,
): ApiUseGraphPushdownInfo[] {
  return value.use_graph_pushdown.filter(isUnsupportedUseGraphPushdown);
}

export function useGraphPushdownWarnings(
  value: Pick<ApiQueryResponse, "execution">,
): string[] {
  return value.execution.warnings.filter((warning) =>
    warning.startsWith(USE_GRAPH_PUSHDOWN_WARNING_PREFIX),
  );
}
