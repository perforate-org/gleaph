import type { Principal } from "@icp-sdk/core/principal";

/** TC39 Temporal types used by generated bindings and SDK conversions. */
export type { Temporal } from "@js-temporal/polyfill";

/** Arbitrary-precision decimal value used by generated bindings (decimal.js). */
export { default as GqlDecimal } from "decimal.js";

/** One value in the Router wire format (the `IcWireValue` candid mirror). */
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
  | { Int256: Uint8Array }
  | { Uint256: Uint8Array }
  | { Float16: number }
  | { Float32: number }
  | { Float64: number }
  | { Float128: Uint8Array }
  | { Float256: Uint8Array }
  | { Decimal: Uint8Array }
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

/** User-facing `ZonedTime` value: time of day with a fixed UTC offset. */
export interface GqlZonedTime {
  nanos: bigint;
  offset_seconds: number;
}

/** Semantic-type hint passed to `toApiValue` by generated code for exact wire conversion. */
export type ApiValueHint =
  | "Null"
  | "Bool"
  | "Int8"
  | "Int16"
  | "Int32"
  | "Int64"
  | "Uint8"
  | "Uint16"
  | "Uint32"
  | "Uint64"
  | "Int128"
  | "Uint128"
  | "Int256"
  | "Uint256"
  | "Float16"
  | "Float32"
  | "Float64"
  | "Float128"
  | "Float256"
  | "Decimal"
  | "Text"
  | "Bytes"
  | "Date"
  | "Time"
  | "LocalTime"
  | "DateTime"
  | "LocalDateTime"
  | "ZonedDateTime"
  | "ZonedTime"
  | "Duration"
  | "Principal"
  | "List"
  | "Path"
  | "Record";

export interface ApiQueryRequest {
  query: string;
  params: Record<string, ApiValue>;
}

export interface ApiPrepareRequest {
  name: string;
  query: string;
  options?: PreparedOptions;
}

export interface ApiPreparedQueryRequest {
  name: string;
  params: Record<string, ApiValue>;
  sort?: PreparedSortSpec[];
}

export interface ApiPreparedMutationRequest extends ApiPreparedQueryRequest {
  client_mutation_key: string;
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
  type_hints: string[];
}

export interface ApiPreparedQueryInfo {
  name: string;
  description?: string | null;
  kind: "Query" | "Update";
  parameters: ApiPreparedParameterInfo[];
  result: { columns: ApiPreparedColumnInfo[] };
  supports_consistency: boolean;
  supports_idempotency: boolean;
  allowed_sorts: PreparedSortSpec[];
  use_graph_pushdown: ApiUseGraphPushdownInfo[];
  explain: string;
}

export interface ApiTypeDiagnostic {
  severity: "error" | "warning" | "info";
  message: string;
  span: { start: number; end: number };
  kind: string;
}

export interface ApiPrepareResponse {
  diagnostics: ApiTypeDiagnostic[];
  prepared: ApiPreparedQueryInfo;
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

export type MutationLifecyclePhase =
  | "Routing"
  | "CanonicalPending"
  | "CanonicalCommitted"
  | "ProjectionPending"
  | "Completed"
  | "Failed";

export interface MutationTokenShard {
  shard_id: number;
  label_stats_seq?: bigint | null;
}

export interface MutationToken {
  mutation_id: bigint;
  shards: MutationTokenShard[];
}

export type ReadMode = { Eventual: null } | { AtLeast: MutationToken };

export interface PreparedManifestGraph {
  id: string;
  name?: string | null;
}

export type PreparedSemanticType =
  | "Null"
  | "Bool"
  | "Int8"
  | "Int16"
  | "Int32"
  | "Int64"
  | "Int128"
  | "Int256"
  | "Uint8"
  | "Uint16"
  | "Uint32"
  | "Uint64"
  | "Uint128"
  | "Uint256"
  | "Float16"
  | "Float32"
  | "Float64"
  | "Float128"
  | "Float256"
  | "Decimal"
  | "Text"
  | "Bytes"
  | "Date"
  | "Time"
  | "LocalTime"
  | "DateTime"
  | "LocalDateTime"
  | "ZonedDateTime"
  | "ZonedTime"
  | "Duration"
  | "Principal"
  | "Path"
  | "Record";

export interface PreparedManifestParameter {
  name: string;
  required: boolean;
  nullable: boolean;
  semantic_type: PreparedSemanticType;
}

export interface PreparedManifestRecordField {
  name: string;
  semantic_type: PreparedSemanticType;
  nullable: boolean;
}

export interface PreparedManifestColumn {
  name: string;
  semantic_type: PreparedSemanticType;
  nullable: boolean;
}

export interface PreparedManifestResultSchema {
  columns: PreparedManifestColumn[];
}

export type PreparedManifestOperationKind = "Query" | "Update";

export interface PreparedManifestSortKey {
  key: string;
  label?: string | null;
}

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

export interface PreparedManifest {
  manifest_version: number;
  graph: PreparedManifestGraph;
  operations: PreparedManifestOperation[];
}
