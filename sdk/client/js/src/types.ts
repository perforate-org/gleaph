import type { Principal } from "@icp-sdk/core/principal";

export type ApiValue =
  | { Null: null }
  | { Bool: boolean }
  | { Int8: number }
  | { Int16: number }
  | { Int32: number }
  | { Int64: bigint | number }
  | { Uint8: number }
  | { Uint16: number }
  | { Uint32: number }
  | { Uint64: bigint | number }
  | { Int128: bigint | number }
  | { Uint128: bigint | number }
  | { Int256: string }
  | { Uint256: string }
  | { Float16: number }
  | { Float32: number }
  | { Float64: number }
  | { Float128: Uint8Array }
  | { Float256: Uint8Array }
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

export type ApiPathElement = { Vertex: Uint8Array } | { Edge: Uint8Array };

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

export type MutationLifecyclePhase =
  | "Routing"
  | "CanonicalPending"
  | "CanonicalCommitted"
  | "ProjectionPending"
  | "Completed"
  | "Failed";

export interface MutationTokenShard {
  shard_id: number;
  label_stats_seq?: bigint;
}

export interface MutationToken {
  mutation_id: bigint;
  shards: MutationTokenShard[];
}

/** Decoded result returned by Router GQL query and prepared-query calls. */
export interface GqlQueryResult<Row = Record<string, ApiValue>> {
  row_count: bigint;
  rows: Row[];
  phase: MutationLifecyclePhase | null;
  token: MutationToken | null;
}

/** Result returned by Router update calls that expose only the affected row count. */
export interface GqlMutationResult {
  row_count: bigint;
}

export interface ApiListPreparedResponse {
  statements: ApiPreparedQueryInfo[];
}

export interface PreparedManifest {
  manifest_version: number;
  graph: PreparedManifestGraph;
  operations: PreparedManifestOperation[];
}

export interface PreparedManifestGraph {
  id: string;
  name?: string | null;
}

export type PreparedManifestOperationKind = "Query" | "Update";

export interface PreparedManifestOperation {
  name: string;
  description?: string | null;
  kind: PreparedManifestOperationKind;
  parameters: PreparedManifestParameter[];
  result: PreparedManifestResultSchema;
  supports_consistency: boolean;
  supports_idempotency: boolean;
  allowed_sorts: PreparedManifestSortKey[];
}

export interface PreparedManifestParameter {
  name: string;
  description?: string | null;
  required: boolean;
  nullable: boolean;
  type: PreparedSemanticType;
}

export interface PreparedManifestResultSchema {
  columns: PreparedManifestColumn[];
}

export interface PreparedManifestColumn {
  name: string;
  type: PreparedSemanticType;
  nullable: boolean;
}

export interface PreparedManifestSortKey {
  key: string;
  label?: string | null;
}

export interface PreparedManifestRecordField {
  name: string;
  type: PreparedSemanticType;
  nullable: boolean;
}

export type PreparedSemanticType =
  | { Null: null }
  | { Bool: null }
  | { Int8: null }
  | { Int16: null }
  | { Int32: null }
  | { Int64: null }
  | { Uint8: null }
  | { Uint16: null }
  | { Uint32: null }
  | { Uint64: null }
  | { Int128: null }
  | { Uint128: null }
  | { Int256: null }
  | { Uint256: null }
  | { Float16: null }
  | { Float32: null }
  | { Float64: null }
  | { Float128: null }
  | { Float256: null }
  | { Decimal: null }
  | { Text: null }
  | { Bytes: null }
  | { Date: null }
  | { Time: null }
  | { Principal: null }
  | { LocalTime: null }
  | { DateTime: null }
  | { LocalDateTime: null }
  | { ZonedDateTime: null }
  | { ZonedTime: null }
  | { Duration: null }
  | { List: { element: PreparedSemanticType } }
  | { Record: { fields: PreparedManifestRecordField[] } }
  | { Path: null };
