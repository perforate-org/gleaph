import { IDL } from "@icp-sdk/core/candid";

const ApiPathElement = IDL.Variant({
  Vertex: IDL.Vec(IDL.Nat8),
  Edge: IDL.Vec(IDL.Nat8),
});

const ApiValue: IDL.Type = IDL.Rec();
const ApiValueVariant = IDL.Variant({
  Null: IDL.Null,
  Bool: IDL.Bool,
  Int8: IDL.Int8,
  Int16: IDL.Int16,
  Int32: IDL.Int32,
  Int64: IDL.Int64,
  Uint8: IDL.Nat8,
  Uint16: IDL.Nat16,
  Uint32: IDL.Nat32,
  Uint64: IDL.Nat64,
  Int128: IDL.Int,
  Uint128: IDL.Nat,
  Int256: IDL.Text,
  Uint256: IDL.Text,
  Float16: IDL.Nat16,
  Float32: IDL.Float32,
  Float64: IDL.Float64,
  Float128: IDL.Vec(IDL.Nat8),
  Float256: IDL.Vec(IDL.Nat8),
  Decimal: IDL.Text,
  Text: IDL.Text,
  Bytes: IDL.Vec(IDL.Nat8),
  Date: IDL.Int32,
  Time: IDL.Nat64,
  LocalTime: IDL.Nat64,
  DateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
  }),
  LocalDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
  }),
  ZonedDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
    offset_seconds: IDL.Int32,
  }),
  ZonedTime: IDL.Record({
    nanos: IDL.Nat64,
    offset_seconds: IDL.Int32,
  }),
  Duration: IDL.Record({
    months: IDL.Int32,
    nanos: IDL.Int64,
  }),
  Principal: IDL.Principal,
  List: IDL.Vec(ApiValue),
  Path: IDL.Vec(ApiPathElement),
  Record: IDL.Vec(IDL.Tuple(IDL.Text, ApiValue)),
});
(ApiValue as unknown as { fill: (value: IDL.Type) => void }).fill(ApiValueVariant);

const ApiPlanSummary = IDL.Record({
  estimated_rows: IDL.Opt(IDL.Float64),
  estimated_cost: IDL.Opt(IDL.Float64),
  has_dml: IDL.Bool,
  dml_error_count: IDL.Nat64,
  dml_warning_count: IDL.Nat64,
  type_warning_count: IDL.Nat64,
});

const ApiExecutionSummary = IDL.Record({
  row_count: IDL.Nat64,
  warning_count: IDL.Nat64,
  had_dml: IDL.Bool,
});

const ApiExecutionResult = IDL.Record({
  rows: IDL.Vec(IDL.Vec(IDL.Tuple(IDL.Text, ApiValue))),
  warnings: IDL.Vec(IDL.Text),
  summary: ApiExecutionSummary,
});

const ApiUseGraphPushdownInfo = IDL.Record({
  graph_name: IDL.Text,
  supported: IDL.Bool,
  reason: IDL.Opt(IDL.Text),
});

const ApiQueryResponse = IDL.Record({
  explain: IDL.Text,
  plan_summary: ApiPlanSummary,
  use_graph_pushdown: IDL.Vec(ApiUseGraphPushdownInfo),
  execution: ApiExecutionResult,
});

const ApiPlanResponse = IDL.Record({
  explain: IDL.Text,
  summary: ApiPlanSummary,
  use_graph_pushdown: IDL.Vec(ApiUseGraphPushdownInfo),
});

const PreparedSortKey = IDL.Record({
  key: IDL.Text,
  label: IDL.Opt(IDL.Text),
  direction: IDL.Opt(IDL.Text),
});

const PreparedSortSpec = IDL.Record({
  key: IDL.Text,
  direction: IDL.Text,
});

const PreparedOptions = IDL.Record({
  description: IDL.Opt(IDL.Text),
  allowed_sorts: IDL.Vec(PreparedSortKey),
  default_sort: IDL.Opt(IDL.Vec(PreparedSortSpec)),
});

const ApiTypeDiagnostic = IDL.Record({
  code: IDL.Opt(IDL.Text),
  message: IDL.Text,
  span_start: IDL.Nat32,
  span_end: IDL.Nat32,
  severity: IDL.Variant({
    Error: IDL.Null,
    Warning: IDL.Null,
  }),
});

const ApiPreparedParameterInfo = IDL.Record({
  name: IDL.Text,
  required: IDL.Bool,
  nullable: IDL.Bool,
  inferred: IDL.Bool,
  type_hints: IDL.Vec(IDL.Text),
});

const ApiPreparedColumnInfo = IDL.Record({
  name: IDL.Text,
  expr: IDL.Text,
  aliased: IDL.Bool,
});

const ApiPreparedQueryInfo = IDL.Record({
  name: IDL.Text,
  kind: IDL.Variant({
    Query: IDL.Null,
    Update: IDL.Null,
  }),
  requires_caller: IDL.Bool,
  extension_types: IDL.Vec(IDL.Text),
  source: IDL.Text,
  description: IDL.Opt(IDL.Text),
  columns: IDL.Vec(ApiPreparedColumnInfo),
  parameters: IDL.Vec(ApiPreparedParameterInfo),
  allowed_sorts: IDL.Vec(PreparedSortKey),
  default_sort: IDL.Opt(IDL.Vec(PreparedSortSpec)),
  type_warnings: IDL.Vec(ApiTypeDiagnostic),
  explain: IDL.Text,
  summary: ApiPlanSummary,
  use_graph_pushdown: IDL.Vec(ApiUseGraphPushdownInfo),
});

const ApiPrepareResponse = IDL.Record({
  prepared: ApiPreparedQueryInfo,
});

const ApiListPreparedResponse = IDL.Record({
  statements: IDL.Vec(ApiPreparedQueryInfo),
});

const PreparedSemanticType: IDL.Type = IDL.Rec();
const PreparedManifestRecordField = IDL.Record({
  name: IDL.Text,
  type: PreparedSemanticType,
  nullable: IDL.Bool,
});
const PreparedSemanticTypeVariant = IDL.Variant({
  Null: IDL.Null,
  Bool: IDL.Null,
  Int8: IDL.Null,
  Int16: IDL.Null,
  Int32: IDL.Null,
  Int64: IDL.Null,
  Uint8: IDL.Null,
  Uint16: IDL.Null,
  Uint32: IDL.Null,
  Uint64: IDL.Null,
  Int128: IDL.Null,
  Uint128: IDL.Null,
  Int256: IDL.Null,
  Uint256: IDL.Null,
  Float16: IDL.Null,
  Float32: IDL.Null,
  Float64: IDL.Null,
  Float128: IDL.Null,
  Float256: IDL.Null,
  Decimal: IDL.Null,
  Text: IDL.Null,
  Bytes: IDL.Null,
  Date: IDL.Null,
  Time: IDL.Null,
  Principal: IDL.Null,
  LocalTime: IDL.Null,
  DateTime: IDL.Null,
  LocalDateTime: IDL.Null,
  ZonedDateTime: IDL.Null,
  ZonedTime: IDL.Null,
  Duration: IDL.Null,
  List: IDL.Record({ element: PreparedSemanticType }),
  Record: IDL.Record({ fields: IDL.Vec(PreparedManifestRecordField) }),
  Path: IDL.Null,
});
(PreparedSemanticType as unknown as { fill: (value: IDL.Type) => void }).fill(
  PreparedSemanticTypeVariant,
);

const PreparedManifest = IDL.Record({
  manifest_version: IDL.Nat32,
  graph: IDL.Record({
    id: IDL.Text,
    name: IDL.Opt(IDL.Text),
  }),
  operations: IDL.Vec(
    IDL.Record({
      name: IDL.Text,
      description: IDL.Opt(IDL.Text),
      kind: IDL.Variant({ Query: IDL.Null, Update: IDL.Null }),
      parameters: IDL.Vec(
        IDL.Record({
          name: IDL.Text,
          description: IDL.Opt(IDL.Text),
          required: IDL.Bool,
          nullable: IDL.Bool,
          type: PreparedSemanticType,
        }),
      ),
      result: IDL.Record({
        columns: IDL.Vec(
          IDL.Record({
            name: IDL.Text,
            type: PreparedSemanticType,
            nullable: IDL.Bool,
          }),
        ),
      }),
      supports_consistency: IDL.Bool,
      supports_idempotency: IDL.Bool,
      allowed_sorts: IDL.Vec(
        IDL.Record({
          key: IDL.Text,
          label: IDL.Opt(IDL.Text),
        }),
      ),
    }),
  ),
});

export const graphIdlFactory = ({ IDL: LocalIDL }: { IDL: typeof IDL }) =>
  LocalIDL.Service({
    query: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Opt(LocalIDL.Vec(LocalIDL.Tuple(LocalIDL.Text, ApiValue)))],
      [LocalIDL.Variant({ Ok: ApiQueryResponse, Err: LocalIDL.Text })],
      ["query"],
    ),
    explain: LocalIDL.Func(
      [LocalIDL.Text],
      [LocalIDL.Variant({ Ok: ApiPlanResponse, Err: LocalIDL.Text })],
      ["query"],
    ),
    update: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Opt(LocalIDL.Vec(LocalIDL.Tuple(LocalIDL.Text, ApiValue)))],
      [LocalIDL.Variant({ Ok: ApiQueryResponse, Err: LocalIDL.Text })],
      [],
    ),
    prepare: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Text, LocalIDL.Opt(PreparedOptions)],
      [LocalIDL.Variant({ Ok: ApiPrepareResponse, Err: LocalIDL.Text })],
      [],
    ),
    list_prepared_api: LocalIDL.Func(
      [],
      [LocalIDL.Variant({ Ok: ApiListPreparedResponse, Err: LocalIDL.Text })],
      ["query"],
    ),
    prepared_manifest: LocalIDL.Func(
      [LocalIDL.Text],
      [LocalIDL.Variant({ Ok: PreparedManifest, Err: LocalIDL.Text })],
      ["query"],
    ),
    execute_prepared_query: LocalIDL.Func(
      [
        LocalIDL.Text,
        LocalIDL.Vec(LocalIDL.Tuple(LocalIDL.Text, ApiValue)),
        LocalIDL.Opt(LocalIDL.Vec(PreparedSortSpec)),
      ],
      [LocalIDL.Variant({ Ok: ApiQueryResponse, Err: LocalIDL.Text })],
      ["query"],
    ),
    execute_prepared_update: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Vec(LocalIDL.Tuple(LocalIDL.Text, ApiValue))],
      [LocalIDL.Variant({ Ok: ApiQueryResponse, Err: LocalIDL.Text })],
      [],
    ),
    drop_prepared: LocalIDL.Func(
      [LocalIDL.Text],
      [
        LocalIDL.Variant({
          Ok: LocalIDL.Record({ dropped: LocalIDL.Bool }),
          Err: LocalIDL.Text,
        }),
      ],
      [],
    ),
  });
