import { IDL } from "@icp-sdk/core/candid";

const IcWirePathElement = IDL.Variant({
  Vertex: IDL.Vec(IDL.Nat8),
  Edge: IDL.Vec(IDL.Nat8),
});

const IcWireValue: IDL.Type = IDL.Rec();
const IcWireValueVariant = IDL.Variant({
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
  DateTime: IDL.Record({ seconds: IDL.Int64, nanos: IDL.Nat32 }),
  LocalDateTime: IDL.Record({ seconds: IDL.Int64, nanos: IDL.Nat32 }),
  ZonedDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
    offset_seconds: IDL.Int32,
  }),
  ZonedTime: IDL.Record({ nanos: IDL.Nat64, offset_seconds: IDL.Int32 }),
  Duration: IDL.Record({ months: IDL.Int32, nanos: IDL.Int64 }),
  Principal: IDL.Principal,
  ExtensionLeaf: IDL.Record({ type_name: IDL.Text, payload: IDL.Vec(IDL.Nat8) }),
  ValueBinary: IDL.Vec(IDL.Nat8),
  List: IDL.Vec(IcWireValue),
  Path: IDL.Vec(IcWirePathElement),
  Record: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
});
(IcWireValue as unknown as { fill: (value: IDL.Type) => void }).fill(IcWireValueVariant);

const IcWirePlanQueryResult = IDL.Record({
  rows: IDL.Vec(
    IDL.Record({
      columns: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
    }),
  ),
});

const MutationLifecyclePhase = IDL.Variant({
  Routing: IDL.Null,
  CanonicalPending: IDL.Null,
  CanonicalCommitted: IDL.Null,
  ProjectionPending: IDL.Null,
  Completed: IDL.Null,
  Failed: IDL.Null,
});

const MutationToken = IDL.Record({
  mutation_id: IDL.Nat64,
  shards: IDL.Vec(
    IDL.Record({
      shard_id: IDL.Nat32,
      label_stats_seq: IDL.Opt(IDL.Nat64),
    }),
  ),
});

const ReadMode = IDL.Variant({
  Eventual: IDL.Null,
  AtLeast: MutationToken,
});

export const GqlQueryResult = IDL.Record({
  row_count: IDL.Nat64,
  rows_blob: IDL.Opt(IDL.Vec(IDL.Nat8)),
  phase: IDL.Opt(MutationLifecyclePhase),
  token: IDL.Opt(MutationToken),
});

export const GqlQueryRows = IcWirePlanQueryResult;

const ApiPlanSummary = IDL.Record({
  estimated_rows: IDL.Opt(IDL.Float64),
  estimated_cost: IDL.Opt(IDL.Float64),
  has_dml: IDL.Bool,
  dml_error_count: IDL.Nat64,
  dml_warning_count: IDL.Nat64,
  type_warning_count: IDL.Nat64,
});

const ApiUseGraphPushdownInfo = IDL.Record({
  graph_name: IDL.Text,
  supported: IDL.Bool,
  reason: IDL.Opt(IDL.Text),
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
  severity: IDL.Variant({ Error: IDL.Null, Warning: IDL.Null }),
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
  kind: IDL.Variant({ Query: IDL.Null, Update: IDL.Null }),
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

const VectorActivationBlockReason = IDL.Variant({
  MissingEmbeddingIncarnationFence: IDL.Null,
  DispatchNotActivated: IDL.Null,
  ShardsNotVectorAttached: IDL.Null,
});

const AtomicInsertPropertyV1 = IDL.Record({
  property_name: IDL.Text,
  value: IDL.Vec(IDL.Nat8),
});

const AtomicInsertVertexV1 = IDL.Record({
  vertex_labels: IDL.Vec(IDL.Text),
  initial_properties: IDL.Vec(AtomicInsertPropertyV1),
});

const BulkLoadEdgeV1 = IDL.Record({
  source: IDL.Vec(IDL.Nat8),
  target: IDL.Vec(IDL.Nat8),
  directed: IDL.Bool,
  edge_label_name: IDL.Opt(IDL.Text),
  inline_property: IDL.Opt(IDL.Vec(IDL.Nat8)),
  initial_edge_properties: IDL.Vec(AtomicInsertPropertyV1),
});

const BulkLoadChunkV1 = IDL.Variant({
  Vertices: IDL.Vec(AtomicInsertVertexV1),
  Edges: IDL.Vec(BulkLoadEdgeV1),
});

const BulkLoadCommand = IDL.Variant({
  Start: IDL.Record({ logical_graph_name: IDL.Text, client_bulk_key: IDL.Text }),
  Append: IDL.Record({
    logical_graph_name: IDL.Text,
    client_bulk_key: IDL.Text,
    chunk_index: IDL.Nat32,
    chunk: BulkLoadChunkV1,
  }),
  Finalize: IDL.Record({ logical_graph_name: IDL.Text, client_bulk_key: IDL.Text }),
  Abort: IDL.Record({ logical_graph_name: IDL.Text, client_bulk_key: IDL.Text }),
});

const AtomicInsertReceiptV1 = IDL.Record({
  logical_operation_count: IDL.Nat64,
  logical_vertex_count: IDL.Nat64,
  logical_edge_count: IDL.Nat64,
  allocated_vertex_ids: IDL.Vec(IDL.Vec(IDL.Nat8)),
});

const BulkLoadPublicStateV1 = IDL.Variant({
  Open: IDL.Null,
  AppendPending: IDL.Null,
  FinalizePending: IDL.Null,
  AbortPending: IDL.Null,
  Completed: IDL.Null,
  Aborted: IDL.Null,
  Failed: IDL.Record({ reason: IDL.Text }),
});

const BulkLoadResponse = IDL.Variant({
  Started: IDL.Record({ next_chunk_index: IDL.Nat32 }),
  Appended: IDL.Record({ chunk_index: IDL.Nat32, receipt: AtomicInsertReceiptV1 }),
  FinalizeAccepted: IDL.Record({ state: BulkLoadPublicStateV1 }),
  AbortAccepted: IDL.Record({ state: BulkLoadPublicStateV1 }),
});

const BulkLoadStatusPage = IDL.Record({
  state: BulkLoadPublicStateV1,
  next_chunk_index: IDL.Nat32,
  committed_chunk_count: IDL.Nat32,
  completed_chunk_count: IDL.Nat32,
  terminal_at_ns: IDL.Opt(IDL.Nat64),
  expires_at_ns: IDL.Opt(IDL.Nat64),
  receipts: IDL.Vec(IDL.Record({ chunk_index: IDL.Nat32, receipt: AtomicInsertReceiptV1 })),
  next_receipt_cursor: IDL.Opt(IDL.Nat32),
});

const RouterError = IDL.Variant({
  NotAuthorized: IDL.Null,
  Forbidden: IDL.Null,
  NotFound: IDL.Text,
  Conflict: IDL.Text,
  Busy: IDL.Record({ operation: IDL.Text }),
  InvalidArgument: IDL.Text,
  ExecutionPathMismatch: IDL.Record({
    entrypoint: IDL.Text,
    program_kind: IDL.Text,
    call_kind: IDL.Text,
    remedy: IDL.Text,
  }),
  GraphUnavailable: IDL.Null,
  GraphContextMismatch: IDL.Record({ api_graph: IDL.Text, resolved_graph: IDL.Text }),
  ShardNotRegistered: IDL.Null,
  ProjectionLag: IDL.Record({
    shard_id: IDL.Nat32,
    watermark: IDL.Text,
    required: IDL.Nat64,
    current: IDL.Nat64,
  }),
  UnsupportedMultiDmlBundle: IDL.Record({ dml_statements: IDL.Nat32, shard_count: IDL.Nat32 }),
  ShardAlreadyRegistered: IDL.Null,
  IdExhausted: IDL.Text,
  UniquenessViolation: IDL.Text,
  UniquenessReservationInFlight: IDL.Text,
  NotImplemented: IDL.Text,
  VectorDispatchActivationBlocked: VectorActivationBlockReason,
  ProvisionCallFailed: IDL.Text,
  ProvisionEncodingFailed: IDL.Text,
  ProvisionConflict: IDL.Text,
  ProvisionRejected: IDL.Text,
  UnknownDeployment: IDL.Text,
  AckConflict: IDL.Record({ stored: IDL.Nat64 }),
  InvalidState: IDL.Text,
  Internal: IDL.Text,
});

export const graphIdlFactory = ({ IDL: LocalIDL }: { IDL: typeof IDL }) =>
  LocalIDL.Service({
    gql_query: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Vec(LocalIDL.Nat8), ReadMode],
      [LocalIDL.Variant({ Ok: GqlQueryResult, Err: RouterError })],
      ["composite_query"],
    ),
    explain: LocalIDL.Func(
      [LocalIDL.Text],
      [LocalIDL.Variant({ Ok: ApiPlanResponse, Err: RouterError })],
      ["query"],
    ),
    gql_mutate: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Vec(LocalIDL.Nat8), LocalIDL.Text],
      [LocalIDL.Variant({ Ok: GqlQueryResult, Err: RouterError })],
      [],
    ),
    prepare: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Text, LocalIDL.Opt(PreparedOptions)],
      [LocalIDL.Variant({ Ok: ApiPrepareResponse, Err: RouterError })],
      [],
    ),
    list_prepared: LocalIDL.Func(
      [LocalIDL.Text],
      [LocalIDL.Variant({ Ok: PreparedManifest, Err: RouterError })],
      ["query"],
    ),
    prepared_query: LocalIDL.Func(
      [
        LocalIDL.Text,
        LocalIDL.Vec(LocalIDL.Nat8),
        LocalIDL.Opt(LocalIDL.Vec(PreparedSortSpec)),
        ReadMode,
      ],
      [LocalIDL.Variant({ Ok: GqlQueryResult, Err: RouterError })],
      ["composite_query"],
    ),
    prepared_mutate: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Vec(LocalIDL.Nat8), LocalIDL.Text],
      [LocalIDL.Variant({ Ok: GqlQueryResult, Err: RouterError })],
      [],
    ),
    bulk_load: LocalIDL.Func(
      [BulkLoadCommand],
      [LocalIDL.Variant({ Ok: BulkLoadResponse, Err: RouterError })],
      [],
    ),
    bulk_load_status: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Text, LocalIDL.Opt(LocalIDL.Nat32), LocalIDL.Nat32],
      [LocalIDL.Variant({ Ok: BulkLoadStatusPage, Err: RouterError })],
      ["query"],
    ),
    drop_prepared: LocalIDL.Func(
      [LocalIDL.Text],
      [LocalIDL.Variant({ Ok: LocalIDL.Null, Err: RouterError })],
      [],
    ),
  });
