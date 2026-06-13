import { IDL } from "@icp-sdk/core/candid";

const IcWirePathElement = IDL.Variant({
  Vertex: IDL.Vec(IDL.Nat8),
  Edge: IDL.Vec(IDL.Nat8),
});

const IcWireValue: IDL.Type = IDL.Rec();
const IcWireValueVariant = IDL.Variant({
  Null: IDL.Null,
  Bool: IDL.Bool,
  Int64: IDL.Int64,
  Uint64: IDL.Nat64,
  Int128: IDL.Int,
  Uint128: IDL.Nat,
  Int256: IDL.Text,
  Uint256: IDL.Text,
  Float64: IDL.Float64,
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
  ExtensionLeaf: IDL.Record({
    type_name: IDL.Text,
    payload: IDL.Vec(IDL.Nat8),
  }),
  ValueBinary: IDL.Vec(IDL.Nat8),
  List: IDL.Vec(IcWireValue),
  Path: IDL.Vec(IcWirePathElement),
  Record: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
});

(IcWireValue as unknown as { fill: (value: IDL.Type) => void }).fill(IcWireValueVariant);

export const IcWirePlanQueryResult = IDL.Record({
  rows: IDL.Vec(
    IDL.Record({
      columns: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
    }),
  ),
});

const GqlQueryResult = IDL.Record({
  row_count: IDL.Nat64,
  rows_blob: IDL.Opt(IDL.Vec(IDL.Nat8)),
});

const RouterError = IDL.Variant({
  NotAuthorized: IDL.Null,
  Forbidden: IDL.Null,
  NotFound: IDL.Text,
  Conflict: IDL.Text,
  InvalidArgument: IDL.Text,
  ExecutionPathMismatch: IDL.Record({
    entrypoint: IDL.Text,
    program_kind: IDL.Text,
    call_kind: IDL.Text,
    remedy: IDL.Text,
  }),
  GraphUnavailable: IDL.Null,
  GraphContextMismatch: IDL.Record({
    api_graph: IDL.Text,
    resolved_graph: IDL.Text,
  }),
  ShardNotRegistered: IDL.Null,
  ShardAlreadyRegistered: IDL.Null,
  VertexNotFound: IDL.Null,
  PlacementAlreadyCommitted: IDL.Null,
  UnallocatedLogicalVertex: IDL.Null,
  IdExhausted: IDL.Text,
  Internal: IDL.Text,
});

export const routerIdlFactory = ({ IDL: LocalIDL }: { IDL: typeof IDL }) =>
  LocalIDL.Service({
    gql_query: LocalIDL.Func(
      [LocalIDL.Text, LocalIDL.Vec(LocalIDL.Nat8)],
      [
        LocalIDL.Variant({
          Ok: GqlQueryResult,
          Err: RouterError,
        }),
      ],
      ["query"],
    ),
  });
