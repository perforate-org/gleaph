import type { Principal } from "@icp-sdk/core/principal";

export type ApiValue =
  | { Null: null }
  | { Bool: boolean }
  | { Int64: bigint | number }
  | { Uint64: bigint | number }
  | { Int128: bigint | number }
  | { Uint128: bigint | number }
  | { Int256: string }
  | { Uint256: string }
  | { Float64: number }
  | { Decimal: string }
  | { Text: string }
  | { Bytes: Uint8Array }
  | { Date: number }
  | { Time: bigint | number }
  | { LocalTime: bigint | number }
  | { DateTime: { seconds: bigint | number; nanos: number } }
  | { LocalDateTime: { seconds: bigint | number; nanos: number } }
  | { ZonedDateTime: { seconds: bigint | number; nanos: number; offset_seconds: number } }
  | { ZonedTime: { nanos: bigint | number; offset_seconds: number } }
  | { Duration: { months: number; nanos: bigint | number } }
  | { Principal: Principal | string }
  | { List: ApiValue[] }
  | { Path: ApiPathElement[] }
  | { Record: Record<string, ApiValue> };

export type ApiPathElement =
  | { Vertex: bigint | number }
  | { Edge: { src: bigint | number; dst: bigint | number; label?: string | null } };

export interface ApiQueryRequest {
  query: string;
  params: Record<string, ApiValue>;
}

export interface ApiPrepareRequest {
  name: string;
  query: string;
  options?: PreparedOptions;
}

export interface ApiExecutePreparedRequest {
  name: string;
  params: Record<string, ApiValue>;
  sort?: PreparedSortSpec[];
}

export interface PreparedOptions {
  description?: string;
  allowed_sorts?: PreparedSortKey[];
  default_sort?: PreparedSortSpec[];
}

export interface PreparedSortKey {
  key: string;
  label?: string;
  direction?: "asc" | "desc";
}

export interface PreparedSortSpec {
  key: string;
  direction: "asc" | "desc";
}

export interface ApiPlanSummary {
  estimated_rows?: number | null;
  estimated_cost?: number | null;
  has_dml: boolean;
  dml_error_count: number;
  dml_warning_count: number;
  type_warning_count: number;
}

export interface ApiExecutionSummary {
  row_count: number;
  warning_count: number;
  had_dml: boolean;
}

export interface ApiExecutionResult {
  rows: Record<string, ApiValue>[];
  warnings: string[];
  summary: ApiExecutionSummary;
}

export interface ApiUseGraphPushdownInfo {
  graph_name: string;
  supported: boolean;
  reason?: string | null;
}

export interface ApiPlanResponse {
  explain: string;
  summary: ApiPlanSummary;
  use_graph_pushdown: ApiUseGraphPushdownInfo[];
}

export interface ApiPreparedParameterInfo {
  name: string;
  required: boolean;
  nullable: boolean;
  inferred: boolean;
  type_hints: string[];
}

export interface ApiPreparedColumnInfo {
  name: string;
  expr: string;
  aliased: boolean;
}

export interface ApiTypeDiagnostic {
  code?: string | null;
  message: string;
  span_start: number;
  span_end: number;
  severity: "Error" | "Warning";
}

export interface ApiPreparedQueryInfo {
  name: string;
  kind: "Query" | "Update";
  requires_caller: boolean;
  extension_types: string[];
  source: string;
  description?: string | null;
  columns: ApiPreparedColumnInfo[];
  parameters: ApiPreparedParameterInfo[];
  allowed_sorts: PreparedSortKey[];
  default_sort?: PreparedSortSpec[] | null;
  type_warnings: ApiTypeDiagnostic[];
  explain: string;
  summary: ApiPlanSummary;
  use_graph_pushdown: ApiUseGraphPushdownInfo[];
}

export interface ApiPrepareResponse {
  prepared: ApiPreparedQueryInfo;
}

export interface ApiQueryResponse {
  explain: string;
  plan_summary: ApiPlanSummary;
  use_graph_pushdown: ApiUseGraphPushdownInfo[];
  execution: ApiExecutionResult;
}

export interface ApiListPreparedResponse {
  statements: ApiPreparedQueryInfo[];
}
