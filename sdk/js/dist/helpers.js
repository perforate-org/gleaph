export const USE_GRAPH_PUSHDOWN_WARNING_PREFIX = "remote USE GRAPH pushdown unavailable";
export function isUnsupportedUseGraphPushdown(info) {
    return !info.supported;
}
export function unsupportedUseGraphPushdowns(value) {
    return value.use_graph_pushdown.filter(isUnsupportedUseGraphPushdown);
}
export function useGraphPushdownWarnings(value) {
    return value.execution.warnings.filter((warning) => warning.startsWith(USE_GRAPH_PUSHDOWN_WARNING_PREFIX));
}
